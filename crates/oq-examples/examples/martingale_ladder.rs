//! The example this framework exists for: the same strategy, the same
//! market, run once with liquidation enforced and once without.
//!
//! ```text
//! cargo run --example martingale_ladder
//! ```
//!
//! Averaging down on a fixed ladder is the classic shape whose backtest
//! is systematically fake without a margin model. It wins in almost
//! every window, because almost every drawdown eventually retraces —
//! and the one that does not is the one that ends the account. A
//! simulation that never liquidates simply carries the position through
//! the hole and books the recovery.
//!
//! Watch the margin-free arm's *lowest equity*. If it went below zero,
//! the account it describes did not exist for the rest of the run, and
//! every fill after that point was placed by a corpse.
//!
//! **This is not a strategy to run.** It is here because it is the
//! clearest demonstration of what the margin overlay is for.

use oq_backtest::{Context, DeviationReport, Intent, MarginMode, RunConfig, Strategy};
use oq_examples::{crash_series, money, price};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, Fill, InstrumentId, OrderId, PriceTicks, QtyLots, Side};

/// Buys a first lot, then doubles down every time price falls another
/// step below the average entry.
struct MartingaleLadder {
    /// Fraction below the average entry at which the next rung sits.
    step: f64,
    /// Lots for the first rung; each subsequent rung doubles.
    base_qty: i64,
    /// Rungs already filled.
    rungs: u32,
    /// Cap, because a ladder without one is a countdown rather than a
    /// strategy. It still is not enough, which is the point.
    max_rungs: u32,
    next_id: u64,
}

impl MartingaleLadder {
    fn new() -> Self {
        Self {
            step: 0.04,
            base_qty: 4,
            rungs: 0,
            max_rungs: 6,
            next_id: 1,
        }
    }

    fn id(&mut self) -> OrderId {
        let id = OrderId::new(self.next_id);
        self.next_id += 1;
        id
    }
}

impl Strategy for MartingaleLadder {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if self.rungs == 0 {
            self.rungs = 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(self.base_qty),
                offset: oq_types::Offset::Open,
            });
            return;
        }

        if self.rungs >= self.max_rungs || ctx.position.0 <= 0 {
            return;
        }

        // Next rung: another `step` below the current average entry.
        #[allow(clippy::cast_possible_truncation)]
        let trigger = (f64::from(i32::try_from(ctx.entry.0).unwrap_or(i32::MAX))
            * (1.0 - self.step * f64::from(self.rungs))) as i64;

        if ctx.tick.low.0 <= trigger {
            let qty = self.base_qty * (1 << self.rungs);
            self.rungs += 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(qty),
                offset: oq_types::Offset::Open,
            });
        }
    }

    fn on_fill(&mut self, _fill: &Fill, _ctx: &Context, _out: &mut Vec<Intent>) {
        // Position management would live here. This ladder deliberately
        // has no exit: the lesson is about what happens when the market
        // does not come back in time, not about exit design.
    }

    fn name(&self) -> &str {
        "martingale-ladder"
    }
}

fn main() {
    // Calm, then a 50% fall, then a partial recovery. The recovery is
    // what makes the margin-free arm look survivable.
    let ticks = crash_series(11, 400, 200, 0.5);

    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(2_000),
    )
    .with_margin(MarginMode::Enforced);

    let report = DeviationReport::compare(&config, MartingaleLadder::new, &ticks);

    println!("market        calm, then -50%, then a partial recovery");
    println!(
        "              start {}  low {}  end {}",
        price(ticks[0].last),
        price(PriceTicks(
            ticks.iter().map(|t| t.last.0).min().unwrap_or_default()
        )),
        price(ticks.last().map(|t| t.last).unwrap_or_default())
    );
    println!();

    println!("                        enforced      margin-free");
    println!(
        "final equity      {} {}",
        money(report.enforced.final_equity),
        money(report.ignored.final_equity)
    );
    println!(
        "lowest equity     {} {}",
        money(report.enforced.min_equity),
        money(report.ignored.min_equity)
    );
    println!(
        "fills             {:>12} {:>16}",
        report.enforced.fills.len(),
        report.ignored.fills.len()
    );
    println!(
        "liquidations      {:>12} {:>16}",
        report.enforced.liquidations.len(),
        report.ignored.liquidations.len()
    );
    println!();

    println!("{}", report.summary_line());

    if !report.margin_free_result_is_honest() {
        println!();
        println!(
            "The margin-free account's equity went below zero. That account did not\n\
             exist for the rest of the run, and the {} fill(s) it made after the\n\
             first liquidation were placed by an account the venue had already closed.",
            report.fills_after_first_liquidation()
        );
    }
}
