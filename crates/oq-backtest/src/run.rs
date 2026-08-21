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

use oq_core::matcher::{DepthOutcome, Matcher};
use oq_core::{Event, Kernel, Output, State};
pub use oq_engine::Observation;
use oq_engine::{L1Engine, L2Engine, Policy, Tick};
use oq_margin::{Contract, FundingSchedule, TierTable};
use oq_strategy::{Context, Ending, Intent, Strategy};
use oq_types::{Cash, Fill, InstrumentId, Nanos, OrderId, PriceTicks, QtyLots, Stamp};

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
    /// Whether to measure how close the account came to liquidation.
    ///
    /// Off by default. It costs about a fifth of the engine's
    /// throughput, and it cannot be sampled the way the equity curve is
    /// — the number wanted is an extreme, and a closest approach that
    /// missed the closest approach is worse than none.
    pub track_margin: bool,
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
    /// Which matcher the run uses.
    ///
    /// L0 by default, the frozen anchor. A higher tier answers
    /// identically until it is also given the data it exists to read —
    /// depth for L2 — so raising this alone changes nothing, which is
    /// what makes it safe to expose as an option rather than a rewrite.
    pub tier: Tier,
}

/// Which matcher a run uses, as configuration rather than as a type.
///
/// Named for the fidelity ladder it selects from. Distinct from
/// `fidelity::Fidelity`, which is about how faithfully a *margin* model
/// tracks a real account -- a different question with the same adjective
/// attached to it.
///
/// Carries the policy with the tier because they are one decision: an
/// L1 whose policy models nothing is L0 wearing L1's name, and a report
/// that says "L1" without saying which policy has told the reader
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    /// Fills at the observation's prices. No queue, latency or impact.
    L0,
    /// Queue, latency and impact as policy — the caller's claim about
    /// their own market, since a tick carries no depth to measure from.
    L1(Policy),
    /// The same, with queue and taker cost measured from the venue's
    /// book where the run supplies depth, and the policy where it does
    /// not.
    L2(Policy),
}

impl Tier {
    fn matcher(&self, instrument: InstrumentId) -> Matcher {
        match self {
            Self::L0 => Matcher::l0(instrument),
            Self::L1(p) => Matcher::L1(Box::new(L1Engine::new(instrument, *p))),
            Self::L2(p) => Matcher::L2(Box::new(L2Engine::new(L1Engine::new(instrument, *p)))),
        }
    }
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
            track_margin: false,
            instrument,
            contract,
            table,
            starting_balance,
            margin: MarginMode::Enforced,
            funding: FundingSchedule::default(),
            fees: oq_core::Fees::none(),
            position_mode: oq_core::PositionMode::OneWay,
            tier: Tier::L0,
        }
    }

    /// Match with a different fidelity tier.
    #[must_use]
    pub fn at_tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }

    /// Measure how close the account came to its maintenance
    /// requirement, for the fidelity report.
    #[must_use]
    pub const fn tracking_margin(mut self) -> Self {
        self.track_margin = true;
        self
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
    /// How close the account came to its maintenance requirement.
    pub margin_usage: MarginUsage,
    /// Which matcher produced these fills.
    ///
    /// Reported because fills without a named matcher are numbers with
    /// no provenance, and the tiers disagree by design.
    pub tier: &'static str,
    /// Depth updates the matcher read and applied.
    pub depth_applied: u64,
    /// Depth updates the book refused, in sequence order.
    ///
    /// Messages were lost between two updates. The book is left as it
    /// was rather than guessing the missing state, and the count says
    /// how often — a reconstruction with holes in it produces plausible
    /// queues that are wrong, and nothing downstream can tell.
    pub depth_refused: u64,
    /// Depth updates handed to a matcher that does not read one.
    ///
    /// **Non-zero is a wrong run, not a slow one.** It means the caller
    /// converted an archive, fed the book, and matched at a tier that
    /// ignored it — producing L0's or L1's answer under whatever name
    /// the report carries. It is counted rather than refused because
    /// refusing would break the run that deliberately feeds one stream
    /// to several tiers to compare them.
    pub depth_unused: u64,
}

/// One liquidation event, kept for the report.
/// How much margin a run used, or why that is not known.
///
/// Three states rather than a pair of numbers, because "nobody
/// measured", "there was never a position to measure" and "the closest
/// approach was zero" are three different facts and the last one means
/// the account stood exactly on the liquidation line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginUsage {
    /// The run did not track it. See [`RunConfig::tracking_margin`].
    ///
    /// Not tracked by default: measuring it costs about a fifth of the
    /// engine's throughput, and a run nobody wants the number from
    /// should not pay for it — the same reason the equity curve is
    /// opt-in.
    NotTracked,
    /// Tracked, and no position was ever open.
    NoPosition,
    /// Tracked, with a position.
    Tracked {
        /// The largest maintenance requirement the account carried.
        peak_maintenance: Cash,
        /// The smallest gap between equity and that requirement.
        ///
        /// Zero means the account stood exactly on the line; negative
        /// means it was past it, which is what a liquidation is.
        min_headroom: Cash,
    },
}

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
    run_observations(config, strategy, ticks.into_iter().map(Observation::Tick))
}

/// Run `strategy` over ticks and depth interleaved.
///
/// The entry L2 needs. Depth reaches the matcher and never the
/// strategy, so a strategy written for [`run_stream`] runs here
/// unchanged and sees the same ticks in the same order.
///
/// Handing depth to a tier that cannot read it is **counted, not
/// ignored** — see [`RunResult::depth_unused`]. A run that converted an
/// archive, fed the book, and matched as L0 anyway is the failure this
/// exists to make visible.
pub fn run_observations<S, I>(config: &RunConfig, strategy: &mut S, stream: I) -> RunResult
where
    S: Strategy,
    I: IntoIterator<Item = Observation>,
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
    .with_mode(config.position_mode)
    .matching_with(config.tier.matcher(config.instrument));
    let mut kernel = Kernel::new(match config.margin {
        MarginMode::Enforced => state,
        MarginMode::Ignored => state.without_liquidation(),
    });

    let mut fills = Vec::new();
    let mut liquidations = Vec::new();
    let mut intents = Vec::new();
    let mut min_equity = config.starting_balance;
    let mut peak_maintenance = Cash::ZERO;
    let mut min_headroom: Option<Cash> = None;
    let track_margin = config.track_margin;
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

    // Orders the kernel has finished with, waiting to be reported to the
    // strategy. An order ending is a different event from its last fill
    // and cannot be inferred from one: a limit order fills in pieces,
    // and only the matcher knows which piece was the last.
    let mut ended: Vec<(OrderId, Ending)> = Vec::new();

    let tier = kernel.state().engine.tier();
    let mut tick_count = 0usize;
    let mut depth_applied = 0u64;
    let mut depth_refused = 0u64;
    let mut depth_unused = 0u64;
    for observation in stream {
        let tick = match observation {
            Observation::Tick(t) => t,
            Observation::Depth(u) => {
                match kernel.apply_depth(&u) {
                    DepthOutcome::Applied => depth_applied += 1,
                    DepthOutcome::Refused(_) => depth_refused += 1,
                    DepthOutcome::NotRead => depth_unused += 1,
                }
                continue;
            }
            Observation::Snapshot {
                update_id,
                bids,
                asks,
            } => {
                if !kernel.install_snapshot(update_id, &bids, &asks) {
                    depth_unused += 1;
                }
                continue;
            }
        };
        tick_count += 1;
        let event = Event::Tick(tick);
        let mut tick_fills: Vec<Fill> = Vec::new();
        let outputs: Vec<Output> = kernel.apply(&event).to_vec();
        note_endings(&outputs, kernel.working(), &mut ended);
        for out in &outputs {
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
        // Only while a position is open. Flat means no requirement, and
        // the branch below already tests for it — so the common case of
        // a strategy that is out of the market pays nothing for this.
        if track_margin && (!summary.qty.is_zero() || !summary.short_qty.is_zero()) {
            let maintenance = kernel.state().maintenance(summary.mark);
            if maintenance.0 > peak_maintenance.0 {
                peak_maintenance = maintenance;
            }
            let headroom = Cash(summary.equity.0 - maintenance.0);
            if min_headroom.is_none_or(|m| headroom.0 < m.0) {
                min_headroom = Some(headroom);
            }
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
        // Then the orders that ended, in the order a live host reports
        // them: the fill first, the end of the order after it.
        for (id, ending) in ended.drain(..) {
            strategy.on_ended(id, ending, &mut intents);
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
                        let outputs: Vec<Output> = kernel
                            .apply(&Event::Cancel {
                                id,
                                stamp: tick.stamp,
                            })
                            .to_vec();
                        note_endings(&outputs, kernel.working(), &mut ended);
                    }
                    continue;
                }
            };
            next_order_id += 1;
            // The kernel's answer, reported back the way a live host
            // reports the venue's. A strategy written against
            // `on_placed` therefore behaves the same in a backtest and
            // live, which is the reason the callback is in the trait
            // rather than in the live host.
            let submitted = matches!(event, Event::Submit { .. });
            let outputs: Vec<oq_core::Output> = kernel.apply(&event).to_vec();
            if submitted {
                if let Event::Submit { id, .. } = event {
                    let refused = outputs.iter().any(|o| {
                        matches!(o, oq_core::Output::Rejected { id: rejected, .. } if *rejected == id)
                    });
                    strategy.on_placed(id, !refused);
                }
            }
            note_endings(&outputs, kernel.working(), &mut ended);
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
        margin_usage: if !track_margin {
            MarginUsage::NotTracked
        } else if let Some(min_headroom) = min_headroom {
            MarginUsage::Tracked {
                peak_maintenance,
                min_headroom,
            }
        } else {
            MarginUsage::NoPosition
        },
        equity_curve,
        max_adverse_ticks: max_adverse,
        tier,
        depth_applied,
        depth_refused,
        depth_unused,
    }
}

/// A tick built from a price, for tests and simple data adapters.
#[must_use]
pub fn tick_at(ns: i64, last: i64, high: i64, low: i64) -> Tick {
    Tick::trades_only(Stamp::synthetic(ns), last, high, low)
}

/// Orders the kernel has just finished with.
///
/// An order that the kernel reported on and that is no longer resting is
/// over. Asking the working set rather than reading the report is what
/// makes a partial fill — a report about an order that is still there —
/// come out as nothing at all, which is exactly right.
fn note_endings(outputs: &[Output], working: &[OrderId], ended: &mut Vec<(OrderId, Ending)>) {
    for out in outputs {
        let (id, ending) = match out {
            Output::Filled(f) => (f.order, Ending::Filled),
            Output::Cancelled(id) => (*id, Ending::Cancelled),
            _ => continue,
        };
        if !working.contains(&id) && !ended.iter().any(|(seen, _)| *seen == id) {
            ended.push((id, ending));
        }
    }
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

    /// Records what it is told about the end of its own orders.
    ///
    /// Places two on the first tick — one that fills at once and one
    /// that rests far below the market — and withdraws the resting one
    /// later, so both ways an order can end are exercised by one run.
    struct EndingWatcher {
        placed: bool,
        ticks: usize,
        cancel_at: usize,
        ended: Vec<(OrderId, Ending)>,
    }

    impl Strategy for EndingWatcher {
        fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
            self.ticks += 1;
            if !self.placed {
                self.placed = true;
                out.push(Intent::Market {
                    id: OrderId::new(1),
                    side: Side::Buy,
                    qty: QtyLots(1),
                    offset: oq_types::Offset::Open,
                });
                out.push(Intent::Limit {
                    id: OrderId::new(2),
                    side: Side::Buy,
                    price: PriceTicks(1),
                    qty: QtyLots(1),
                    offset: oq_types::Offset::Open,
                });
            }
            if self.ticks == self.cancel_at {
                out.push(Intent::Cancel(OrderId::new(2)));
            }
        }
        fn on_ended(&mut self, id: OrderId, ending: Ending, _out: &mut Vec<Intent>) {
            self.ended.push((id, ending));
        }
        fn name(&self) -> &str {
            "ending-watcher"
        }
    }

    /// Both ways an order ends reach the strategy, and neither reaches
    /// it twice.
    ///
    /// A strategy that has to infer this from fills gets it wrong in
    /// both directions: it never learns about an order that was
    /// cancelled, and it decides a partially filled one is over. The
    /// live host reports the venue's answer; this is the same answer
    /// from the matcher, so a strategy written against it behaves the
    /// same in both.
    #[test]
    fn a_strategy_is_told_when_each_of_its_orders_ends() {
        let mut watcher = EndingWatcher {
            placed: false,
            ticks: 0,
            cancel_at: 50,
            ended: Vec::new(),
        };
        run(&config(10_000), &mut watcher, &falling_market());

        assert_eq!(
            watcher.ended,
            vec![
                (OrderId::new(1), Ending::Filled),
                (OrderId::new(2), Ending::Cancelled),
            ],
            "both endings, once each, in the order they happened"
        );
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

#[cfg(test)]
mod depth_path_tests {
    use super::*;
    use oq_engine::{Delay, DepthUpdate, Impact, Latency, QueueAhead};
    use oq_margin::{Contract, TierTable};
    use oq_strategy::Intent;
    use oq_types::{Cash, InstrumentId, OrderId, PriceTicks, QtyLots, Side, Stamp};

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

    /// Rests one limit buy at a price the fixture trades through, then
    /// holds. Enough to make the queue the only thing that decides
    /// whether it fills.
    struct RestOnce {
        placed: bool,
        price: i64,
    }

    impl Strategy for RestOnce {
        fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
            if !self.placed {
                self.placed = true;
                out.push(Intent::Limit {
                    id: OrderId(1),
                    side: Side::Buy,
                    price: PriceTicks(self.price),
                    qty: QtyLots(1),
                    offset: oq_types::Offset::Open,
                });
            }
        }
        fn name(&self) -> &str {
            "rest-once"
        }
    }

    fn flat_ticks(n: i64, price: i64) -> Vec<Tick> {
        (1..=n)
            .map(|i| Tick {
                stamp: Stamp::new(i * 1_000_000, i * 1_000_000),
                last: PriceTicks(price),
                high: PriceTicks(price),
                low: PriceTicks(price),
                bid: PriceTicks(price),
                ask: PriceTicks(price),
                volume: QtyLots(10 * i),
            })
            .collect()
    }

    fn depth_at(id: u64, price: i64, qty: i64) -> Observation {
        Observation::Depth(Box::new(DepthUpdate {
            event_ms: 0,
            first_id: id,
            final_id: id,
            prev_final_id: if id > 1 { Some(id - 1) } else { None },
            bids: vec![oq_engine::Level { price, qty }],
            asks: Vec::new(),
        }))
    }

    /// The whole point of the path: depth reaches the matcher and
    /// changes when an order fills.
    ///
    /// Same strategy, same ticks. The only difference is that one run
    /// was told the level it joined already had 5000 lots displayed on
    /// it — so it waits, and the other does not.
    #[test]
    fn depth_reaches_the_matcher_and_delays_a_fill() {
        let ticks = flat_ticks(5, 100);
        let policy = Policy {
            queue: QueueAhead::None,
            latency: Latency {
                entry: Delay::Fixed(Nanos(0)),
                response: Delay::Fixed(Nanos(0)),
            },
            impact: Impact { coefficient: 0 },
        };

        let cfg = config(1_000_000).at_tier(Tier::L2(policy));
        let mut with_book = RestOnce {
            placed: false,
            price: 100,
        };
        // The snapshot first: an incremental stream says what changed,
        // and a book with nothing to change refuses every update.
        let stream = [
            Observation::Snapshot {
                update_id: 0,
                bids: vec![oq_engine::Level {
                    price: 100,
                    qty: 5_000,
                }],
                asks: Vec::new(),
            },
            depth_at(1, 100, 5_000),
        ]
        .into_iter()
        .chain(ticks.iter().copied().map(Observation::Tick));
        let deep = run_observations(&cfg, &mut with_book, stream);

        let mut no_book = RestOnce {
            placed: false,
            price: 100,
        };
        let empty = run_stream(&cfg, &mut no_book, ticks.iter().copied());

        assert_eq!(deep.tier, "L2");
        assert_eq!(deep.depth_applied, 1, "the update reached the matcher");
        assert_eq!(deep.depth_refused, 0, "and was in sequence");
        assert_eq!(deep.depth_unused, 0);
        assert!(
            !empty.fills.is_empty(),
            "with nothing displayed the order is first in the queue and fills"
        );
        assert!(
            deep.fills.is_empty(),
            "behind 5000 lots it must not fill in {} ticks; got {:?}",
            ticks.len(),
            deep.fills
        );
    }

    /// Depth handed to a tier that cannot read it is counted, not
    /// ignored.
    ///
    /// A run that converted an archive, fed the book, and matched as L0
    /// anyway reports L0's answer under whatever name the caller gave
    /// it. The count is what lets a reader see that happened.
    #[test]
    fn depth_given_to_a_lower_tier_is_reported_unused() {
        let ticks = flat_ticks(3, 100);
        let mut s = RestOnce {
            placed: false,
            price: 100,
        };
        let out = run_observations(
            &config(1_000_000),
            &mut s,
            core::iter::once(depth_at(1, 100, 5_000))
                .chain(ticks.iter().copied().map(Observation::Tick)),
        );
        assert_eq!(out.tier, "L0");
        assert_eq!(out.depth_applied, 0);
        assert_eq!(out.depth_unused, 1, "an ignored update must be visible");
    }

    /// Raising the tier without giving it anything new must not change
    /// the answer. Otherwise the tiers are a menu rather than a claim
    /// about fidelity.
    #[test]
    fn a_higher_tier_given_nothing_extra_matches_l0() {
        let ticks = flat_ticks(5, 100);
        let mut a = RestOnce {
            placed: false,
            price: 100,
        };
        let l0 = run_stream(&config(1_000_000), &mut a, ticks.iter().copied());

        let mut b = RestOnce {
            placed: false,
            price: 100,
        };
        let l2 = run_stream(
            &config(1_000_000).at_tier(Tier::L2(Policy::TRANSPARENT)),
            &mut b,
            ticks.iter().copied(),
        );

        assert!(!l0.fills.is_empty(), "the fixture must fill");
        assert_eq!(l2.fills, l0.fills, "an unfed L2 must reproduce L0");
        assert_eq!(l2.tier, "L2", "while still reporting what it was");
    }
}
