//! How much of this result depended on never being liquidated?
//!
//! A backtest without a margin model is not a pessimistic backtest or a
//! simplified one. It is a backtest of a *different account* — one with
//! unlimited collateral, which no venue offers. The difference between
//! that account and a real one is invisible in a summary statistic and
//! decisive in the tail, and it grows with leverage and with any
//! strategy that adds to losing positions.
//!
//! This module runs the same strategy over the same data twice, with
//! liquidation enforced and with it disabled, and reports what changed.
//! The answer is not a correction factor. It is a statement about which
//! of the two numbers describes an account that could have existed.
//!
//! ## Reading the report
//!
//! - **No liquidations in either arm** — the margin model made no
//!   difference on this data, and the margin-free result stands. This
//!   is the outcome for a well-collateralized strategy, and reporting
//!   it plainly is what makes the other outcomes credible.
//! - **Liquidations in the enforced arm only** — the margin-free result
//!   is not a slightly optimistic version of the truth. It describes
//!   paths that end the account, and everything after the first
//!   liquidation is fiction.
//! - **Equity gap** — how much of the reported profit came from those
//!   paths.
//!
//! The report deliberately does not produce a single "adjusted" number.
//! Blending a real result with an impossible one yields a number that
//! describes neither.

use crate::run::{MarginMode, RunConfig, RunResult, run};
use oq_engine::Tick;
use oq_strategy::Strategy;
use oq_types::{Cash, Nanos};

/// The two arms and what separates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviationReport {
    pub strategy: String,
    pub ticks: usize,
    /// The run a real account would have had.
    pub enforced: RunResult,
    /// The run a margin-free backtest reports.
    pub ignored: RunResult,
}

/// What the comparison concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Margin never bound; the margin-free result is the result.
    NoDifference,
    /// The account was closed out at least once. The margin-free
    /// result describes paths that could not have happened.
    Liquidated {
        count: usize,
        first_at: Nanos,
        /// Equity the margin-free arm reports minus what a real account
        /// would have ended with.
        overstated_by: Cash,
    },
}

impl DeviationReport {
    /// Run both arms and compare.
    ///
    /// `make_strategy` is called once per arm because a strategy holds
    /// state; sharing one instance between the arms would let the first
    /// run's history leak into the second and silently invalidate the
    /// comparison.
    pub fn compare<S: Strategy, F: FnMut() -> S>(
        config: &RunConfig,
        mut make_strategy: F,
        ticks: &[Tick],
    ) -> Self {
        let enforced = run(
            &config.clone().with_margin(MarginMode::Enforced),
            &mut make_strategy(),
            ticks,
        );
        let ignored = run(
            &config.clone().with_margin(MarginMode::Ignored),
            &mut make_strategy(),
            ticks,
        );
        Self {
            strategy: enforced.strategy.clone(),
            ticks: ticks.len(),
            enforced,
            ignored,
        }
    }

    /// What the comparison concluded.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self.enforced.liquidations.first() {
            None => Verdict::NoDifference,
            Some(first) => Verdict::Liquidated {
                count: self.enforced.liquidations.len(),
                first_at: first.at,
                overstated_by: self.ignored.final_equity.sub(self.enforced.final_equity),
            },
        }
    }

    /// Whether the margin-free result can be reported as-is.
    #[must_use]
    pub fn margin_free_result_is_honest(&self) -> bool {
        matches!(self.verdict(), Verdict::NoDifference)
    }

    /// Fills the margin-free arm recorded after the account would
    /// already have been closed.
    ///
    /// These are the trades that make the two equity curves diverge:
    /// not a modelling subtlety, but positions taken by an account that
    /// no longer existed.
    #[must_use]
    pub fn fills_after_first_liquidation(&self) -> usize {
        let Some(first) = self.enforced.liquidations.first() else {
            return 0;
        };
        self.ignored
            .fills
            .iter()
            .filter(|f| f.stamp.exch > first.at)
            .count()
    }

    /// A human-readable summary.
    ///
    /// Plain text rather than a formatted table: this line ends up in
    /// commit messages, review comments, and chat, and every one of
    /// those renders plain text correctly.
    #[must_use]
    pub fn summary_line(&self) -> String {
        match self.verdict() {
            Verdict::NoDifference => format!(
                "{}: margin never bound over {} ticks; the margin-free result stands \
                 (final equity {:.2})",
                self.strategy,
                self.ticks,
                self.enforced.final_equity.as_f64()
            ),
            Verdict::Liquidated {
                count,
                first_at,
                overstated_by,
            } => format!(
                "{}: LIQUIDATED {count}x, first at t={}; margin-free equity {:.2} vs real {:.2} \
                 (overstated by {:.2}); {} fills in the margin-free run happened after the \
                 account was already closed",
                self.strategy,
                first_at.0,
                self.ignored.final_equity.as_f64(),
                self.enforced.final_equity.as_f64(),
                overstated_by.as_f64(),
                self.fills_after_first_liquidation(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RunConfig, tick_at};
    use oq_margin::{Contract, MarginTier, TierTable};
    use oq_strategy::{Context, Intent};
    use oq_types::{InstrumentId, OrderId, QtyLots, Ratio, Side};

    const BTC: Contract = Contract::new(10_000);

    fn table() -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    fn config(balance: i64) -> RunConfig {
        RunConfig::new(
            InstrumentId::new(1),
            BTC,
            table(),
            Cash::from_units(balance),
        )
    }

    /// Adds to a losing position on a fixed ladder, then takes profit.
    ///
    /// A generic teaching example of the family of strategies this
    /// report exists for: averaging down looks excellent without a
    /// margin model, because the model of the account it is tested
    /// against cannot run out of collateral.
    struct CoverLadder {
        step_ticks: i64,
        max_covers: usize,
        covers: usize,
        next_id: u64,
    }

    impl CoverLadder {
        fn new() -> Self {
            Self {
                step_ticks: 20_000,
                max_covers: 6,
                covers: 0,
                next_id: 1,
            }
        }
        fn id(&mut self) -> OrderId {
            self.next_id += 1;
            OrderId::new(self.next_id)
        }
    }

    impl Strategy for CoverLadder {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            if ctx.position.is_zero() {
                if self.covers == 0 && ctx.working == 0 {
                    let id = self.id();
                    out.push(Intent::Market {
                        instrument: oq_types::InstrumentId::new(1),
                        id,
                        side: Side::Buy,
                        qty: QtyLots(2),
                        offset: oq_types::Offset::Open,
                    });
                    self.covers = 1;
                }
                return;
            }
            // Add another rung while the market keeps falling.
            if self.covers <= self.max_covers && ctx.working == 0 {
                let next = ctx.entry.0 - self.step_ticks * self.covers as i64;
                if next > 0 && ctx.tick.last.0 <= next + self.step_ticks {
                    let id = self.id();
                    let qty = QtyLots(2 << self.covers.min(5));
                    out.push(Intent::Limit {
                        instrument: oq_types::InstrumentId::new(1),
                        id,
                        side: Side::Buy,
                        price: oq_types::PriceTicks(next),
                        qty,
                        offset: oq_types::Offset::Open,
                    });
                    self.covers += 1;
                }
            }
        }
        fn name(&self) -> &str {
            "cover-ladder"
        }
    }

    fn crash() -> Vec<Tick> {
        let mut ticks = Vec::new();
        for i in 0..400 {
            let p = 1_200_000 - i * 1_500;
            ticks.push(tick_at(i, p, p, p));
        }
        for i in 0..400 {
            let p = 600_000 + i * 1_500;
            ticks.push(tick_at(400 + i, p, p, p));
        }
        ticks
    }

    #[test]
    fn a_well_collateralized_run_reports_no_difference() {
        let report = DeviationReport::compare(&config(10_000_000), CoverLadder::new, &crash());
        assert_eq!(report.verdict(), Verdict::NoDifference);
        assert!(report.margin_free_result_is_honest());
        assert!(report.summary_line().contains("margin never bound"));
    }

    #[test]
    fn a_thin_account_shows_the_gap_the_report_exists_to_show() {
        let report = DeviationReport::compare(&config(500), CoverLadder::new, &crash());
        match report.verdict() {
            Verdict::Liquidated {
                count,
                overstated_by,
                ..
            } => {
                assert!(count >= 1);
                assert!(
                    overstated_by.0 > 0,
                    "the margin-free arm must report more equity than the real one"
                );
                assert!(!report.margin_free_result_is_honest());
            }
            Verdict::NoDifference => {
                panic!("a 500 USDT account running a cover ladder into a 50% crash must be closed")
            }
        }
    }

    #[test]
    fn fills_after_the_first_liquidation_are_counted() {
        let report = DeviationReport::compare(&config(500), CoverLadder::new, &crash());
        assert!(
            report.fills_after_first_liquidation() > 0,
            "the margin-free arm kept trading an account that no longer existed"
        );
    }

    #[test]
    fn both_arms_see_identical_market_data() {
        // Guards the experiment itself: if the arms differed in their
        // inputs, the comparison would measure the difference in inputs.
        let ticks = crash();
        let report = DeviationReport::compare(&config(10_000_000), CoverLadder::new, &ticks);
        assert_eq!(report.enforced.ticks, report.ignored.ticks);
        assert_eq!(report.enforced.ticks, ticks.len());
        assert_eq!(
            report.enforced.fills, report.ignored.fills,
            "with margin never binding, the two arms must trade identically"
        );
    }

    #[test]
    fn the_comparison_is_reproducible() {
        let ticks = crash();
        let a = DeviationReport::compare(&config(500), CoverLadder::new, &ticks);
        let b = DeviationReport::compare(&config(500), CoverLadder::new, &ticks);
        assert_eq!(a, b);
    }
}
