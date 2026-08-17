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

use oq_core::{Event, Kernel, Output, State};
use oq_engine::Tick;
use oq_margin::{Contract, FundingSchedule, TierTable};
use oq_strategy::{Context, Intent, Strategy};
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
    /// Sample equity every this many ticks, or never when zero.
    ///
    /// A sweep needs a return series and a run did not produce one, so
    /// the statistics this workspace already implements could not be fed
    /// by the runs it already had. Sampled rather than per-tick for a
    /// reason that is not memory: **a Sharpe ratio is a function of its
    /// return frequency**, and one computed from per-tick returns is not
    /// comparable to any number anyone has published. Stating the
    /// interval makes the frequency part of the result rather than an
    /// accident of the data's density.
    pub equity_every: usize,
    pub instrument: InstrumentId,
    pub contract: Contract,
    pub table: TierTable,
    pub starting_balance: Cash,
    pub margin: MarginMode,
    pub funding: FundingSchedule,
    /// What the venue charges per fill.
    ///
    /// Zero by default and set deliberately. A fee schedule nobody
    /// chose produces a result that is wrong in a way no reader can
    /// see; a run with no fees is at least obviously that.
    pub fees: oq_core::Fees,
    /// Whether opposing exposure nets or stands as two legs.
    ///
    /// One-way by default. It has to be set to match the account being
    /// modelled: the same fills mean different things under each, and
    /// the difference is a margin requirement the run either charges or
    /// does not.
    pub position_mode: oq_core::PositionMode,
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
            // Off by default: a run that nobody asked for a curve from
            // should not pay for one, and a sweep asks.
            equity_every: 0,
            instrument,
            contract,
            table,
            starting_balance,
            margin: MarginMode::Enforced,
            funding: FundingSchedule::default(),
            fees: oq_core::Fees::none(),
            position_mode: oq_core::PositionMode::OneWay,
        }
    }

    /// Sample equity every `n` ticks. Zero turns it off.
    #[must_use]
    pub const fn sampling_equity_every(mut self, n: usize) -> Self {
        self.equity_every = n;
        self
    }

    #[must_use]
    pub const fn with_fees(mut self, fees: oq_core::Fees) -> Self {
        self.fees = fees;
        self
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
    /// Trading fees charged over the run, positive.
    pub fees_paid: Cash,
    /// The lowest equity the account reached at any point.
    ///
    /// The number a drawdown statistic is computed from, and the one a
    /// margin-free run reports as survivable when it was not.
    pub min_equity: Cash,
    /// Equity sampled every [`RunConfig::equity_every`] ticks.
    ///
    /// Empty when sampling is off. The first entry is the starting
    /// balance, so a curve of length n yields n-1 returns and a run that
    /// sampled nothing yields none rather than a spurious zero.
    pub equity_curve: Vec<Cash>,
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
///
/// Takes a slice, so the caller has already decided to hold the whole
/// window. For a window large enough that this is the binding
/// constraint, [`run_stream`] consumes ticks one at a time instead and
/// produces the identical result.
pub fn run<S: Strategy>(config: &RunConfig, strategy: &mut S, ticks: &[Tick]) -> RunResult {
    run_stream(config, strategy, ticks.iter().copied())
}

/// Run `strategy` over a stream of ticks.
///
/// The core has always consumed one tick at a time — `apply(State,
/// Event)` never looks backwards — so holding the window was the
/// harness's choice, not the engine's requirement. At 64 bytes a tick,
/// two years of one instrument is 11 GB, and that number, not anything
/// about the strategy, is what decides how long a window a given machine
/// can run. The reference implementation walks a day at a time and its
/// footprint is the same for two years as for two days.
///
/// Consuming a stream makes the peak a block rather than the window, so
/// window length stops being bounded by memory. Everything else is
/// unchanged: [`run`] delegates here, and the two produce identical
/// results by construction rather than by agreement.
pub fn run_stream<S, I>(config: &RunConfig, strategy: &mut S, ticks: I) -> RunResult
where
    S: Strategy,
    I: IntoIterator<Item = Tick>,
{
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
    )
    .with_fees(config.fees)
    .with_mode(config.position_mode);
    let mut kernel = Kernel::new(match config.margin {
        MarginMode::Enforced => state,
        MarginMode::Ignored => state.without_liquidation(),
    });

    let mut fills = Vec::new();
    let mut liquidations = Vec::new();
    let mut intents = Vec::new();
    let mut min_equity = config.starting_balance;
    // Seeded with the opening balance so the first sampled interval has a
    // return, rather than the curve starting at the first sample and
    // silently discarding it.
    let mut equity_curve: Vec<Cash> = if config.equity_every > 0 {
        vec![config.starting_balance]
    } else {
        Vec::new()
    };
    let mut max_adverse: i64 = 0;
    let mut next_order_id = 1u64;
    let mut last_funding = Nanos(i64::MIN);

    let mut tick_count = 0usize;
    for tick in ticks {
        tick_count += 1;
        let event = Event::Tick(tick);
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
        if config.equity_every > 0 && tick_count % config.equity_every == 0 {
            equity_curve.push(summary.equity);
        }
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
            tick,
            position: summary.qty,
            entry: summary.entry,
            short_position: summary.short_qty,
            short_entry: summary.short_entry,
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
                    offset,
                } => Event::Submit {
                    id,
                    side,
                    price: Some(price),
                    qty,
                    offset,
                    stamp: tick.stamp,
                },
                Intent::Market {
                    id,
                    side,
                    qty,
                    offset,
                } => Event::Submit {
                    id,
                    side,
                    price: None,
                    qty,
                    offset,
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
        ticks: tick_count,
        final_equity: summary.equity,
        realized: summary.realized,
        funding_paid: summary.funding,
        fees_paid: summary.fees,
        min_equity,
        equity_curve,
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
    use oq_strategy::Intent;
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
                    offset: oq_types::Offset::Open,
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

#[cfg(test)]
mod stream_tests {
    use super::*;
    use oq_margin::{Contract, TierTable};
    use oq_types::{Cash, InstrumentId, PriceTicks, Stamp};

    /// A strategy that trades, so the comparison exercises fills and
    /// position state rather than two empty runs agreeing.
    #[derive(Default)]
    struct Pinger {
        n: usize,
    }

    impl Strategy for Pinger {
        fn name(&self) -> &str {
            "pinger"
        }
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            self.n += 1;
            if self.n % 50 == 0 {
                out.push(Intent::market(
                    oq_types::OrderId(self.n as u64),
                    if self.n % 100 == 0 {
                        oq_types::Side::Sell
                    } else {
                        oq_types::Side::Buy
                    },
                    oq_types::QtyLots(1),
                ));
            }
            let _ = ctx;
        }
    }

    fn ticks(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let i = i as i64;
                // A wandering price, so fills land at varying levels and
                // an ordering difference would show up in the result.
                let drift = (i % 97) * 13 - 600;
                Tick::trades_only(
                    Stamp::synthetic(1_700_000_000_000_000_000 + i * 250_000_000),
                    6_000_000 + drift,
                    6_000_100 + drift,
                    5_999_900 + drift,
                )
            })
            .collect()
    }

    fn config() -> RunConfig {
        RunConfig::new(
            InstrumentId::new(1),
            Contract::new(1_000),
            TierTable::example_btcusdt(),
            Cash::from_units(20_000),
        )
    }

    /// The two entry points must not merely agree on the total — they
    /// must produce the same fills in the same order, or a caller's
    /// choice between them would quietly change the answer.
    #[test]
    fn streaming_and_slice_runs_are_identical() {
        let data = ticks(5_000);
        let cfg = config();

        let mut a = Pinger::default();
        let from_slice = run(&cfg, &mut a, &data);

        let mut b = Pinger::default();
        let from_stream = run_stream(&cfg, &mut b, data.iter().copied());

        assert_eq!(from_slice.ticks, from_stream.ticks);
        assert_eq!(from_slice.fills.len(), from_stream.fills.len());
        assert_eq!(from_slice.fills, from_stream.fills);
        assert_eq!(from_slice.realized, from_stream.realized);
        assert_eq!(from_slice.fees_paid, from_stream.fees_paid);
        assert_eq!(from_slice.final_equity, from_stream.final_equity);
        assert_eq!(from_slice.min_equity, from_stream.min_equity);
        assert_eq!(from_slice.max_adverse_ticks, from_stream.max_adverse_ticks);
        assert!(
            !from_slice.fills.is_empty(),
            "a run with no fills would compare two empty sequences and prove nothing"
        );
    }

    /// A stream that yields nothing is a run over no data, not a panic.
    #[test]
    fn an_empty_stream_runs_and_reports_nothing() {
        let cfg = config();
        let mut s = Pinger::default();
        let r = run_stream(&cfg, &mut s, std::iter::empty());
        assert_eq!(r.ticks, 0);
        assert!(r.fills.is_empty());
        assert_eq!(r.final_equity, cfg.starting_balance);
        let _ = PriceTicks(0);
    }
}

#[cfg(test)]
mod hedge_mode_tests {
    use super::*;
    use oq_margin::{Contract, TierTable};
    use oq_types::{Cash, InstrumentId, Offset, OrderId, QtyLots, Side, Stamp};

    /// Opens a long and a short of the same size, then holds. Net
    /// exposure is zero throughout; under hedge accounting the account
    /// is still carrying margin for both.
    #[derive(Default)]
    struct BothSides {
        opened: bool,
    }

    impl Strategy for BothSides {
        fn name(&self) -> &str {
            "both-sides"
        }
        fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
            if self.opened {
                return;
            }
            self.opened = true;
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(200),
                offset: Offset::Open,
            });
            out.push(Intent::Market {
                id: OrderId::new(2),
                side: Side::Sell,
                qty: QtyLots(200),
                offset: Offset::Open,
            });
        }
    }

    fn ticks() -> Vec<Tick> {
        (0..200)
            .map(|i| {
                let i = i64::from(i);
                Tick::trades_only(
                    Stamp::synthetic(1_700_000_000_000_000_000 + i * 250_000_000),
                    6_000_000,
                    6_000_000,
                    6_000_000,
                )
            })
            .collect()
    }

    fn config(mode: oq_core::PositionMode, balance: i64) -> RunConfig {
        let mut c = RunConfig::new(
            InstrumentId::new(1),
            Contract::new(1_000),
            TierTable::example_btcusdt(),
            Cash::from_units(balance),
        );
        c.position_mode = mode;
        c
    }

    /// The whole point, end to end: identical fills, identical prices,
    /// and a different position because the account is accounted for
    /// differently.
    #[test]
    fn the_same_fills_leave_a_net_position_or_two_legs() {
        let data = ticks();

        let mut s = BothSides::default();
        let netted = run(
            &config(oq_core::PositionMode::OneWay, 20_000),
            &mut s,
            &data,
        );

        let mut s = BothSides::default();
        let hedged = run(&config(oq_core::PositionMode::Hedge, 20_000), &mut s, &data);

        assert_eq!(netted.fills.len(), 2);
        assert_eq!(hedged.fills.len(), netted.fills.len(), "same fills");
        assert_eq!(
            netted.fills, hedged.fills,
            "the mode changes the accounting, not the trading"
        );
    }

    /// And the difference that matters: a hedged account posts margin on
    /// both legs, so a balance that survives under netting need not
    /// survive under hedging. A netted run reporting survival here is
    /// reporting an account the venue was charging twice for.
    #[test]
    fn a_balance_that_survives_netted_can_be_liquidated_hedged() {
        let data = ticks();

        // Small enough that two legs' maintenance bites and one net
        // position of zero does not.
        let balance = 30;

        let mut s = BothSides::default();
        let netted = run(
            &config(oq_core::PositionMode::OneWay, balance),
            &mut s,
            &data,
        );

        let mut s = BothSides::default();
        let hedged = run(
            &config(oq_core::PositionMode::Hedge, balance),
            &mut s,
            &data,
        );

        assert!(
            netted.liquidations.is_empty(),
            "netted sees zero exposure and nothing to liquidate"
        );
        assert!(
            !hedged.liquidations.is_empty(),
            "hedged posts margin for both legs and cannot cover it"
        );
    }
}
