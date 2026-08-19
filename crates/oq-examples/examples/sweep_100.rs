//! G4: one hundred configurations, with the statistics, inside thirty
//! minutes.
//!
//! ```text
//! cargo run --release -p oq-examples --example sweep_100
//! ```
//!
//! The gate is a wall-clock budget, so this measures wall clock and
//! prints it. It is also a floor rather than a regression gate: shared
//! machines vary by several times from hour to hour, and a tight
//! comparison would fail on noise and be switched off within a week.
//!
//! The statistics are the other half of the gate — "with DSR and PBO
//! emitted automatically". They are not requested here. `sweep` computes
//! them because a sweep that reports only its best configuration is the
//! instrument that produces overfitted strategies, and making that
//! opt-in would mean most sweeps never opted in.

use std::time::Instant;

use oq_backtest::sweep::Thresholds;
use oq_backtest::{Candidate, Context, Intent, MarginMode, RunConfig, Strategy, sweep};
use oq_examples::{MarketShape, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

/// The gate's budget.
const BUDGET_SECS: f64 = 30.0 * 60.0;

/// Configurations the gate names.
const CONFIGS: usize = 100;

/// Ticks per configuration. A year of one-minute bars is about 525,600;
/// this is a little over that, so the run is not smaller than the thing
/// the gate is about.
const TICKS: usize = 600_000;

/// A two-average crossover. Chosen because its parameters form a natural
/// grid and because it trades often enough that the accounting is
/// exercised rather than skipped.
struct Cross {
    fast: usize,
    slow: usize,
    fast_sum: f64,
    slow_sum: f64,
    history: Vec<f64>,
    long: bool,
    next_id: u64,
}

impl Cross {
    fn new(fast: usize, slow: usize) -> Self {
        Self {
            fast,
            slow,
            fast_sum: 0.0,
            slow_sum: 0.0,
            history: Vec::with_capacity(slow + 1),
            long: false,
            next_id: 1,
        }
    }
}

impl Strategy for Cross {
    fn name(&self) -> &str {
        "cross"
    }

    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let px = ctx.tick.last.0 as f64;
        self.history.push(px);
        self.fast_sum += px;
        self.slow_sum += px;
        if self.history.len() > self.fast {
            self.fast_sum -= self.history[self.history.len() - self.fast - 1];
        }
        if self.history.len() > self.slow {
            self.slow_sum -= self.history[self.history.len() - self.slow - 1];
            // Keep the buffer bounded; anything older cannot be needed.
            if self.history.len() > self.slow * 2 {
                self.history.drain(..self.slow);
            }
        } else {
            return;
        }

        let fast = self.fast_sum / self.fast as f64;
        let slow = self.slow_sum / self.slow as f64;
        let want_long = fast > slow;
        if want_long == self.long {
            return;
        }
        self.long = want_long;
        self.next_id += 1;
        out.push(Intent::Market {
            id: OrderId(self.next_id),
            side: if want_long { Side::Buy } else { Side::Sell },
            qty: QtyLots(1),
            offset: if ctx.position.0 == 0 {
                oq_types::Offset::Open
            } else {
                oq_types::Offset::Close
            },
        });
    }
}

fn main() {
    // Calm, not trending. `MarketShape::trending` applies its drift per
    // observation, so over 600,000 of them the price is exponential: it
    // ends at i64::MAX, and every statistic computed over it is about a
    // market where a contract costs more than the number system can
    // express. The first version of this gate used it, and the numbers
    // it reported — a PBO of 0.4975 — were arithmetic on a saturated
    // price. A benchmark whose market is nonsense measures the engine's
    // speed correctly and its statistics not at all.
    let ticks = series(MarketShape::calm(TICKS), 7);

    // Asserted rather than assumed, because the failure above was
    // silent: the run completed, printed plausible figures, and nothing
    // said the market had run off the end of the type.
    let last = ticks.last().map_or(0, |t| t.last.0);
    assert!(
        last > 0 && last < i64::MAX / 1_000_000,
        "the generated market reached {last}, which leaves no room for a notional; \
         the statistics below would be about a market that cannot exist"
    );

    // A 10 x 10 grid, skipping the degenerate fast >= slow half by
    // offsetting rather than by filtering, so the count is exactly 100
    // and the gate's number means what it says.
    let grid: Vec<(usize, usize)> = (0..10)
        .flat_map(|i| (0..10).map(move |j| (5 + i * 3, 40 + j * 12 + i)))
        .collect();
    assert_eq!(grid.len(), CONFIGS);

    let builders: Vec<Box<dyn Fn() -> Cross>> = grid
        .iter()
        .map(|&(f, s)| Box::new(move || Cross::new(f, s)) as Box<dyn Fn() -> Cross>)
        .collect();
    let candidates: Vec<Candidate<'_, Cross>> = grid
        .iter()
        .zip(&builders)
        .map(|(&(f, s), b)| Candidate {
            id: format!("fast={f},slow={s}"),
            build: b.as_ref(),
        })
        .collect();

    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(100_000),
    )
    .with_margin(MarginMode::Enforced)
    // A sweep cannot be scored without a return series, and the gate
    // requires the statistics, so this is part of the workload rather
    // than an optimisation the benchmark declined to do.
    .sampling_equity_every(64);

    let started = Instant::now();
    let report = sweep(&config, &candidates, &ticks);
    let elapsed = started.elapsed().as_secs_f64();

    let total_ticks = (CONFIGS * TICKS) as f64;
    println!("sweep gate (G4)");
    println!("  configurations   {}", report.results.len());
    println!("  ticks each       {TICKS}");
    println!("  elapsed          {elapsed:.2} s of a {BUDGET_SECS:.0} s budget");
    println!(
        "  throughput       {:.2} M ticks/s across the sweep",
        total_ticks / elapsed / 1e6
    );
    println!(
        "  headroom         {:.0}x",
        BUDGET_SECS / elapsed.max(f64::MIN_POSITIVE)
    );
    println!();

    match &report.deflated_sharpe {
        Ok(v) => println!("  deflated Sharpe  {v:.4}"),
        Err(e) => println!("  deflated Sharpe  unavailable: {e}"),
    }
    match &report.pbo {
        Ok(r) => {
            let v = r.pbo;
            println!("  PBO              {v:.4}");
            // The diagnostics, which were computed on every sweep and
            // discarded before anybody could read them. The slope is the
            // one worth the space: PBO says how often the winner stops
            // winning, and this says whether winning meant anything.
            println!(
                "  OOS/IS slope     {:+.4}  ({} splits)",
                r.performance_degradation, r.n_splits
            );
            if r.performance_degradation <= 0.0 {
                println!("                   at or below zero: ranking these configurations");
                println!("                   in sample predicted nothing out of sample");
            }
            println!("  loss out of samp {:.4}", r.probability_of_loss);
            println!("  median OOS Sharpe {:.4}", r.median_oos_sharpe);
            // Worth reading rather than skipping past. A grid of moving
            // averages over one synthetic series is a hundred variants
            // of one strategy, none of which has an edge, so the best
            // in-sample configuration should be a coin flip out of
            // sample. A PBO near 0.5 is the statistic working. A PBO
            // near 0 here would mean it was broken.
            if (v - 0.5_f64).abs() < 0.15 {
                println!("                   ~0.5, as it should be: this grid is a hundred");
                println!("                   variants of one strategy with no edge between them");
            }
        }
        Err(e) => println!("  PBO              unavailable: {e}"),
    }
    if !report.unscorable.is_empty() {
        println!(
            "  unscorable       {} configuration(s)",
            report.unscorable.len()
        );
    }
    println!();

    let mut ok = true;
    if report.results.len() != CONFIGS {
        println!(
            "FAIL: the gate names {CONFIGS} configurations, this ran {}",
            report.results.len()
        );
        ok = false;
    }
    if elapsed > BUDGET_SECS {
        println!("FAIL: {elapsed:.2} s exceeds the {BUDGET_SECS:.0} s budget");
        ok = false;
    }
    // "with DSR and PBO emitted automatically" is half the gate, so a
    // run that finishes in time without them has not met it.
    if report.deflated_sharpe.is_err() || report.pbo.is_err() {
        println!("FAIL: the gate requires both statistics, and one is unavailable");
        ok = false;
    }
    // FR-RESEARCH-3: the statistics are computed and then *acted on*.
    // Marking a number and leaving it there is what every tool already
    // does — the refusal is the part that changes what happens next.
    println!();
    let thresholds = Thresholds::default();
    let refusals = report.refusals(thresholds);
    if refusals.is_empty() {
        println!("  strict mode      would package this sweep");
    } else {
        println!("  strict mode      would REFUSE to package this sweep:");
        for r in &refusals {
            println!("    - {r}");
        }
        println!();
        println!("  That is the intended outcome here, not a failure of the gate. This");
        println!("  grid is a hundred variants of one strategy with no edge between");
        println!("  them, so a search over it should not produce something deployable.");
        println!("  A strict mode that passed this would be decoration.");
    }

    if ok {
        println!();
        println!("G4 met: {CONFIGS} configurations with both statistics in {elapsed:.2} s");
    } else {
        std::process::exit(1);
    }
}
