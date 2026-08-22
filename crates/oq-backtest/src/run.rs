//! Running a strategy over a tick stream.
//!
//! The host is a loop and nothing more: for each observation, advance
//! the kernel, hand the strategy what it is allowed to see, and turn
//! its intents into events. Everything interesting — matching, margin,
//! the ledger — lives in the core, so a run and a live session differ
//! by who produces the ticks.
//!
//! ## Margin is a switch, and that is the point
//!
//! A run can be executed with margin enforced or with it disabled. The
//! two are otherwise identical: same strategy, same ticks, same
//! matching, same fees. Comparing them isolates one question —
//! *how much of this result depends on never being liquidated?* — and
//! that comparison is the whole purpose of [`crate::deviation`].
//!
//! Disabling margin is not a "simple mode" for beginners. It is the
//! control arm of an experiment, and it is what most published backtest
//! results silently are.

use crate::strategy::{Context, Intent, Strategy};
use oq_core::{Event, Kernel, Output, State};
use oq_engine::Tick;
use oq_margin::{Contract, FundingSchedule, TierTable};
use oq_types::{Cash, Fill, InstrumentId, Nanos, PriceTicks, QtyLots, Stamp};

/// Whether the venue is allowed to close the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode {
    /// Liquidation is modelled. What a real account experiences.
    Enforced,
    /// Liquidation is not modelled. The control arm, and what a
    /// backtest without a margin model silently assumes.
    Ignored,
}

/// How a run is configured.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub instrument: InstrumentId,
    pub contract: Contract,
    pub table: TierTable,
    pub starting_balance: Cash,
    pub margin: MarginMode,
    pub funding: FundingSchedule,
}

impl RunConfig {
    #[must_use]
    pub fn new(
        instrument: InstrumentId,
        contract: Contract,
        table: TierTable,
        starting_balance: Cash,
    ) -> Self {
        Self {
            instrument,
            contract,
            table,
            starting_balance,
            margin: MarginMode::Enforced,
            funding: FundingSchedule::default(),
        }
    }

    #[must_use]
    pub fn with_margin(mut self, mode: MarginMode) -> Self {
        self.margin = mode;
        self
    }

    #[must_use]
    pub fn with_funding(mut self, funding: FundingSchedule) -> Self {
        self.funding = funding;
        self
    }
}

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub strategy: String,
    pub fills: Vec<Fill>,
    /// Every liquidation, in order. Empty is the interesting case only
    /// when the other arm's is not.
    pub liquidations: Vec<Liquidation>,
    pub ticks: usize,
    pub final_equity: Cash,
    pub realized: Cash,
    pub funding_paid: Cash,
    /// The lowest equity the account reached at any point.
    ///
    /// The number a drawdown statistic is computed from, and the one a
    /// margin-free run reports as survivable when it was not.
    pub min_equity: Cash,
    /// The largest adverse excursion against the open position, in
    /// ticks. Reported in ticks rather than money because that is the
    /// unit a position sizing decision is made in.
    pub max_adverse_ticks: i64,
}

/// One liquidation event, kept for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liquidation {
    pub at: Nanos,
    pub price: PriceTicks,
    pub qty: QtyLots,
    pub equity: Cash,
}

/// Run `strategy` over `ticks`.
///
/// Deterministic: the same strategy over the same ticks with the same
/// configuration produces the same result, on any machine.
pub fn run<S: Strategy>(config: &RunConfig, strategy: &mut S, ticks: &[Tick]) -> RunResult {
    // Both arms carry the *same* margin table and differ only in
    // whether the venue is allowed to act on it. Zeroing the table
    // instead would still liquidate at zero equity, which is not the
    // control arm: a margin-free backtest holds a position through
    // arbitrary drawdown and reports the recovery.
    let state = State::new(
        config.instrument,
        config.contract,
        config.table.clone(),
        config.starting_balance,
    );
    let mut kernel = Kernel::new(match config.margin {
        MarginMode::Enforced => state,
        MarginMode::Ignored => state.without_liquidation(),
    });

    let mut fills = Vec::new();
    let mut liquidations = Vec::new();
    let mut intents = Vec::new();
    let mut min_equity = config.starting_balance;
    let mut max_adverse: i64 = 0;
    let mut next_order_id = 1u64;
    let mut last_funding = Nanos(i64::MIN);

    for tick in ticks {
        let event = Event::Tick(*tick);
        let mut tick_fills: Vec<Fill> = Vec::new();
        for out in kernel.apply(&event) {
            match out {
                Output::Filled(f) => {
                    fills.push(*f);
                    tick_fills.push(*f);
                }
                Output::Liquidated {
                    at,
                    price,
                    qty,
                    equity,
                } => liquidations.push(Liquidation {
                    at: *at,
                    price: *price,
                    qty: *qty,
                    equity: *equity,
                }),
                _ => {}
            }
        }

        // Funding settles between the previous tick and this one, on the
        // same half-open interval the schedule uses, so nothing settles
        // twice and nothing is skipped when ticks are sparse.
        let now = tick.stamp.exch;
        if !config.funding.is_empty() {
            let due: Vec<_> = config.funding.between(last_funding, now).to_vec();
            for rate in due {
                let event = Event::Funding {
                    at: rate.at,
                    rate: rate.rate,
                    mark: rate.mark,
                };
                for out in kernel.apply(&event) {
                    if let Output::Liquidated {
                        at,
                        price,
                        qty,
                        equity,
                    } = out
                    {
                        liquidations.push(Liquidation {
                            at: *at,
                            price: *price,
                            qty: *qty,
                            equity: *equity,
                        });
                    }
                }
            }
            last_funding = now;
        }

        let summary = kernel.summary();
        if summary.equity < min_equity {
            min_equity = summary.equity;
        }
        if !summary.qty.is_zero() && summary.entry.0 > 0 {
            let adverse = if summary.qty.0 > 0 {
                summary.entry.0 - tick.dn_extent().0
            } else {
                tick.up_extent().0 - summary.entry.0
            };
            if adverse > max_adverse {
                max_adverse = adverse;
            }
        }

        let ctx = Context {
            tick: *tick,
            position: summary.qty,
            entry: summary.entry,
            equity: summary.equity,
            working: kernel.working().len(),
        };

        // Fills first, then the tick. A strategy that manages a position
        // must see its executions before it decides what to do next, and
        // the context it sees already reflects them.
        intents.clear();
        for fill in &tick_fills {
            strategy.on_fill(fill, &ctx, &mut intents);
        }
        strategy.on_tick(&ctx, &mut intents);

        for intent in &intents {
            let event = match *intent {
                Intent::Limit {
                    id,
                    side,
                    price,
                    qty,
                } => Event::Submit {
                    id,
                    side,
                    price: Some(price),
                    qty,
                    stamp: tick.stamp,
                },
                Intent::Market { id, side, qty } => Event::Submit {
                    id,
                    side,
                    price: None,
                    qty,
                    stamp: tick.stamp,
                },
                Intent::Cancel(id) => Event::Cancel {
                    id,
                    stamp: tick.stamp,
                },
                Intent::CancelAll => {
                    for id in kernel.working().to_vec() {
                        kernel.apply(&Event::Cancel {
                            id,
                            stamp: tick.stamp,
                        });
                    }
                    continue;
                }
            };
            next_order_id += 1;
            kernel.apply(&event);
        }
        let _ = next_order_id;
    }

    let summary = kernel.summary();
    RunResult {
        strategy: strategy.name().to_string(),
        fills,
        liquidations,
        ticks: ticks.len(),
        final_equity: summary.equity,
        realized: summary.realized,
        funding_paid: summary.funding,
        min_equity,
        max_adverse_ticks: max_adverse,
    }
}

/// A tick built from a price, for tests and simple data adapters.
#[must_use]
pub fn tick_at(ns: i64, last: i64, high: i64, low: i64) -> Tick {
    Tick::trades_only(Stamp::synthetic(ns), last, high, low)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::Intent;
    use oq_types::{OrderId, Side};

    const BTC: Contract = Contract::new(10_000);

    fn table() -> TierTable {
        TierTable::new(vec![oq_margin::MarginTier {
            max_notional: Cash(i64::MAX),
            rate: oq_types::Ratio::from_percent(1),
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

    /// Buys once and holds. Enough to exercise the host without any
    /// trading idea getting in the way of what is being tested.
    struct BuyAndHold {
        qty: i64,
        done: bool,
    }

    impl Strategy for BuyAndHold {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            if !self.done && ctx.position.is_zero() && ctx.working == 0 {
                self.done = true;
                out.push(Intent::Market {
                    id: OrderId::new(1),
                    side: Side::Buy,
                    qty: QtyLots(self.qty),
                });
            }
        }
        fn name(&self) -> &str {
            "buy-and-hold"
        }
    }

    fn falling_market() -> Vec<Tick> {
        // 120_000 down to 100_000 over 200 ticks, then back up.
        let mut ticks = Vec::new();
        for i in 0..200 {
            let p = 1_200_000 - i * 1_000;
            ticks.push(tick_at(i, p, p, p));
        }
        for i in 0..200 {
            let p = 1_000_000 + i * 1_000;
            ticks.push(tick_at(200 + i, p, p, p));
        }
        ticks
    }

    #[test]
    fn a_run_is_deterministic() {
        let ticks = falling_market();
        let cfg = config(10_000);
        let a = run(
            &cfg,
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        let b = run(
            &cfg,
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn without_margin_a_thin_account_survives_a_move_that_would_end_it() {
        // The control arm and the real arm disagree, which is the whole
        // reason the switch exists.
        let ticks = falling_market();
        let thin = 150;

        let enforced = run(
            &config(thin).with_margin(MarginMode::Enforced),
            &mut BuyAndHold {
                qty: 10,
                done: false,
            },
            &ticks,
        );
        let ignored = run(
            &config(thin).with_margin(MarginMode::Ignored),
            &mut BuyAndHold {
                qty: 10,
                done: false,
            },
            &ticks,
        );

        assert!(
            !enforced.liquidations.is_empty(),
            "the venue should have closed this account"
        );
        assert!(
            ignored.liquidations.is_empty(),
            "the control arm never liquidates, by construction"
        );
        assert!(
            ignored.final_equity > enforced.final_equity,
            "ignoring margin flatters the result: {:?} vs {:?}",
            ignored.final_equity,
            enforced.final_equity
        );
    }

    #[test]
    fn with_enough_collateral_the_two_arms_agree() {
        // The comparison must be silent when there is nothing to say,
        // or nobody will believe it when it speaks.
        let ticks = falling_market();
        let fat = 1_000_000;
        let enforced = run(
            &config(fat).with_margin(MarginMode::Enforced),
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        let ignored = run(
            &config(fat).with_margin(MarginMode::Ignored),
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        assert!(enforced.liquidations.is_empty());
        assert_eq!(enforced.final_equity, ignored.final_equity);
        assert_eq!(enforced.fills, ignored.fills);
    }

    #[test]
    fn adverse_excursion_is_measured_against_entry() {
        let ticks = falling_market();
        let result = run(
            &config(1_000_000),
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        // Entry near 1_200_000, low at 1_000_000.
        assert!(
            result.max_adverse_ticks > 190_000,
            "expected a large adverse excursion, got {}",
            result.max_adverse_ticks
        );
    }

    #[test]
    fn min_equity_records_the_worst_point_not_the_last_one() {
        let ticks = falling_market();
        let result = run(
            &config(1_000_000),
            &mut BuyAndHold {
                qty: 1,
                done: false,
            },
            &ticks,
        );
        assert!(
            result.min_equity < result.final_equity,
            "the market recovered, so the worst point must be worse than the end"
        );
    }
}
