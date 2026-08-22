//! End-to-end backtest throughput, on data anyone can regenerate.
//!
//! ```text
//! cargo bench -p oq-examples
//! ```
//!
//! The market is seeded, so the workload is identical on every machine
//! and in every run. That is the property that makes a number quotable:
//! a benchmark over private data proves something only to the person
//! holding the data.
//!
//! Three measurements, chosen because each answers a question someone
//! actually asks:
//!
//! - **Full loop** — what a backtest costs per observation, strategy
//!   and accounting included. The headline figure.
//! - **Margin on versus off** — what modelling liquidation costs. The
//!   project argues that margin fidelity is worth paying for; the price
//!   should be visible rather than asserted.
//! - **Matching alone** — the engine without the strategy, so a slow
//!   strategy cannot be mistaken for a slow engine.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use oq_backtest::{Context, Intent, MarginMode, RunConfig, Strategy, run};
use oq_examples::{MarketShape, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

const TICKS: usize = 100_000;

/// Flips between long and flat on a moving-average crossing.
///
/// Representative rather than trivial: it keeps state, computes an
/// indicator per observation, and trades often enough that the fill and
/// accounting paths are exercised. A do-nothing strategy would measure
/// the loop and nothing else.
struct MaCross {
    fast: Sma,
    slow: Sma,
    long: bool,
    next_id: u64,
}

struct Sma {
    window: usize,
    samples: Vec<i64>,
    cursor: usize,
    filled: bool,
    sum: i64,
}

impl Sma {
    fn new(window: usize) -> Self {
        Self {
            window,
            samples: vec![0; window],
            cursor: 0,
            filled: false,
            sum: 0,
        }
    }

    fn push(&mut self, value: i64) -> Option<i64> {
        self.sum -= self.samples[self.cursor];
        self.samples[self.cursor] = value;
        self.sum += value;
        self.cursor = (self.cursor + 1) % self.window;
        if self.cursor == 0 {
            self.filled = true;
        }
        self.filled
            .then(|| self.sum / i64::try_from(self.window).unwrap_or(1))
    }
}

impl MaCross {
    fn new() -> Self {
        Self {
            fast: Sma::new(20),
            slow: Sma::new(100),
            long: false,
            next_id: 1,
        }
    }
}

impl Strategy for MaCross {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let last = ctx.tick.last.0;
        let (Some(fast), Some(slow)) = (self.fast.push(last), self.slow.push(last)) else {
            return;
        };
        let want_long = fast > slow;
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

/// Holds a position and never trades, so the measurement is the loop,
/// matching and accounting with the strategy's own work removed.
struct Passive {
    opened: bool,
}

impl Strategy for Passive {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        if !self.opened {
            self.opened = true;
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(1),
                offset: oq_types::Offset::Open,
            });
        }
    }

    fn name(&self) -> &str {
        "passive"
    }
}

fn config(margin: MarginMode) -> RunConfig {
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(1_000_000),
    )
    .with_margin(margin)
}

fn benches(c: &mut Criterion) {
    let ticks = series(MarketShape::trending(TICKS), 20_260_816);

    let mut group = c.benchmark_group("backtest");
    group.throughput(Throughput::Elements(TICKS as u64));

    group.bench_function("ma_cross/margin_enforced", |b| {
        b.iter(|| {
            let mut strategy = MaCross::new();
            black_box(run(
                &config(MarginMode::Enforced),
                &mut strategy,
                black_box(&ticks),
            ))
        });
    });

    // The cost of the thing this project argues for. Reported so the
    // trade-off is a number rather than a claim.
    group.bench_function("ma_cross/margin_ignored", |b| {
        b.iter(|| {
            let mut strategy = MaCross::new();
            black_box(run(
                &config(MarginMode::Ignored),
                &mut strategy,
                black_box(&ticks),
            ))
        });
    });

    group.bench_function("passive/margin_enforced", |b| {
        b.iter(|| {
            let mut strategy = Passive { opened: false };
            black_box(run(
                &config(MarginMode::Enforced),
                &mut strategy,
                black_box(&ticks),
            ))
        });
    });

    group.finish();

    // How throughput scales with the run length: a per-tick cost that
    // grows with history means something is accumulating that should
    // not be.
    let mut scaling = c.benchmark_group("scaling");
    for size in [10_000usize, 100_000, 400_000] {
        let ticks = series(MarketShape::trending(size), 20_260_816);
        scaling.throughput(Throughput::Elements(size as u64));
        scaling.bench_with_input(BenchmarkId::from_parameter(size), &ticks, |b, ticks| {
            b.iter(|| {
                let mut strategy = MaCross::new();
                black_box(run(&config(MarginMode::Enforced), &mut strategy, ticks))
            });
        });
    }
    scaling.finish();
}

criterion_group!(all, benches);
criterion_main!(all);
