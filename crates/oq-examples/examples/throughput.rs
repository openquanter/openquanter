//! Measure backtest throughput, and fail below a floor.
//!
//! ```text
//! cargo run --release --example throughput
//! cargo run --release --example throughput -- --floor 2000000
//! ```
//!
//! `cargo bench` is the tool for comparing two versions of the code on
//! one machine: it repeats, discards outliers, and reports confidence
//! intervals. This is the other job — a single pass with a hard floor,
//! cheap enough to run on every push.
//!
//! The floor is set far below what any real machine produces, and that
//! is deliberate. Shared CI runners vary by several times from hour to
//! hour, so a tight gate would fail on noise and be switched off within
//! a week. This one cannot see a five percent regression. It can see
//! the engine becoming ten times slower, which is the failure that
//! actually happens — someone allocates in the hot loop, or turns an
//! O(1) lookup into a scan.
//!
//! What it measures is the **in-memory engine loop**: matching, margin,
//! accounting and the strategy. Reading and parsing tick files is not
//! included, and on a real workload that is often the larger cost.

use std::process::ExitCode;
use std::time::Instant;

use oq_backtest::{Context, Intent, MarginMode, RunConfig, Strategy, run};
use oq_examples::{MarketShape, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

const TICKS: usize = 400_000;
const DEFAULT_FLOOR: f64 = 2_000_000.0;

struct MaCross {
    fast: Vec<i64>,
    slow: Vec<i64>,
    fast_sum: i64,
    slow_sum: i64,
    n: usize,
    long: bool,
    next_id: u64,
}

impl MaCross {
    fn new() -> Self {
        Self {
            fast: vec![0; 20],
            slow: vec![0; 100],
            fast_sum: 0,
            slow_sum: 0,
            n: 0,
            long: false,
            next_id: 1,
        }
    }
}

impl Strategy for MaCross {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let last = ctx.tick.last.0;
        let fi = self.n % self.fast.len();
        let si = self.n % self.slow.len();
        self.fast_sum += last - self.fast[fi];
        self.slow_sum += last - self.slow[si];
        self.fast[fi] = last;
        self.slow[si] = last;
        self.n += 1;
        if self.n < self.slow.len() {
            return;
        }

        let want_long = self.fast_sum / 20 > self.slow_sum / 100;
        if want_long == self.long {
            return;
        }
        self.long = want_long;
        let id = OrderId::new(self.next_id);
        self.next_id += 1;
        out.push(Intent::market(
            id,
            if want_long { Side::Buy } else { Side::Sell },
            QtyLots(if ctx.position.0 == 0 {
                5
            } else {
                ctx.position.0.abs()
            }),
        ));
    }

    fn name(&self) -> &str {
        "ma-cross"
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let floor = args
        .iter()
        .position(|a| a == "--floor")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_FLOOR);

    let ticks = series(MarketShape::trending(TICKS), 20_260_816);
    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(1_000_000),
    )
    .with_margin(MarginMode::Enforced);

    // One untimed pass so the measurement is not dominated by cold
    // caches and first-touch page faults.
    let mut warm = MaCross::new();
    let _ = run(&config, &mut warm, &ticks);

    let mut strategy = MaCross::new();
    let started = Instant::now();
    let result = run(&config, &mut strategy, &ticks);
    let elapsed = started.elapsed();

    #[allow(clippy::cast_precision_loss)]
    let rate = TICKS as f64 / elapsed.as_secs_f64();

    println!("ticks        {TICKS}");
    println!("fills        {}", result.fills.len());
    println!("elapsed      {:.3} ms", elapsed.as_secs_f64() * 1_000.0);
    println!("throughput   {:.2} M ticks/s", rate / 1e6);
    println!("floor        {:.2} M ticks/s", floor / 1e6);
    println!();
    println!("In-memory engine loop: matching, margin, accounting, strategy.");
    println!("Reading and parsing tick files is not included.");

    if rate < floor {
        eprintln!();
        eprintln!(
            "FAIL: {:.2} M ticks/s is below the floor of {:.2} M ticks/s.",
            rate / 1e6,
            floor / 1e6
        );
        eprintln!("The floor is set many times below normal, so this is not noise.");
        eprintln!("Look for an allocation in the hot loop or a scan where a lookup was.");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
