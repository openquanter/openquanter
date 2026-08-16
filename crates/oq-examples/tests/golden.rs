//! Golden tests over the examples.
//!
//! The examples print numbers, the documentation quotes them, and a
//! reader who runs the commands expects to see the same thing. That
//! makes the printed values part of the public surface, so they are
//! pinned here.
//!
//! A failure means one of two things and the difference matters:
//!
//! - The engine's behaviour changed. Investigate before updating.
//! - The change was intended. Update the constants *and* the quoted
//!   numbers in the docs, in the same commit.
//!
//! Never relax an assertion to make it pass. These exist precisely to
//! notice when matching, margin or accounting drift.

use oq_backtest::{Context, DeviationReport, Intent, MarginMode, RunConfig, Strategy, run};
use oq_examples::{MarketShape, crash_series, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

fn config(balance: i64) -> RunConfig {
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
}

struct BuyAndHold {
    bought: bool,
}

impl Strategy for BuyAndHold {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        if !self.bought {
            self.bought = true;
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(10),
            });
        }
    }

    fn name(&self) -> &str {
        "buy-and-hold"
    }
}

#[test]
fn hello_produces_the_documented_numbers() {
    let ticks = series(MarketShape::trending(2_000), 1);
    let result = run(&config(10_000), &mut BuyAndHold { bought: false }, &ticks);

    assert_eq!(result.ticks, 2_000);
    assert_eq!(result.fills.len(), 1);
    assert_eq!(result.liquidations.len(), 0);
    assert_eq!(
        result.final_equity,
        Cash(1_086_194_000_000),
        "final equity moved; the README and the example output quote this"
    );
    assert_eq!(result.min_equity, Cash(999_337_900_000));
}

/// The ladder from the flagship example, kept in step with it.
struct MartingaleLadder {
    step: f64,
    base_qty: i64,
    rungs: u32,
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
            });
            return;
        }
        if self.rungs >= self.max_rungs || ctx.position.0 <= 0 {
            return;
        }
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
            });
        }
    }

    fn name(&self) -> &str {
        "martingale-ladder"
    }
}

#[test]
fn the_margin_free_arm_reports_an_account_that_did_not_survive() {
    // The claim the project is built on, asserted rather than described.
    let ticks = crash_series(11, 400, 200, 0.5);
    let report = DeviationReport::compare(
        &config(2_000).with_margin(MarginMode::Enforced),
        MartingaleLadder::new,
        &ticks,
    );

    assert_eq!(
        report.enforced.liquidations.len(),
        1,
        "the enforced arm must be liquidated, or the example teaches nothing"
    );
    assert_eq!(report.ignored.liquidations.len(), 0);

    assert!(
        report.ignored.min_equity.0 < 0,
        "the margin-free arm must go below zero: {:?}",
        report.ignored.min_equity
    );
    assert!(
        report.ignored.final_equity.0 > report.enforced.final_equity.0 * 100,
        "the overstatement must be large enough to be undeniable: {:?} vs {:?}",
        report.ignored.final_equity,
        report.enforced.final_equity
    );
    assert!(
        !report.margin_free_result_is_honest(),
        "a run whose equity went negative is not an honest result"
    );
    assert!(
        report.fills_after_first_liquidation() > 0,
        "fills placed by a closed account are the concrete evidence"
    );

    // Pinned exactly: the documentation quotes these.
    assert_eq!(report.enforced.final_equity, Cash(6_153_200_000));
    assert_eq!(report.ignored.final_equity, Cash(2_090_811_440_000));
    assert_eq!(report.ignored.min_equity, Cash(-3_030_214_120_000));
}

#[test]
fn the_generated_market_is_stable_across_runs_and_machines() {
    // Everything above depends on this. If the generator ever changes,
    // every golden number here is meaningless rather than merely wrong.
    let ticks = crash_series(11, 400, 200, 0.5);
    assert_eq!(ticks.len(), 800);
    assert_eq!(ticks[0].last.0, 5_999_752);
    assert_eq!(ticks[799].last.0, 4_990_551);
    assert_eq!(
        ticks.iter().map(|t| t.last.0).min(),
        Some(2_958_398),
        "the low of the crash"
    );
}
