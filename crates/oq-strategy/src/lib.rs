//! What a strategy is, from the host's point of view.
//!
//! A strategy sees market data and returns *intents*. It does not place
//! orders, hold a reference to the engine, or observe the account
//! except through what it is given. Three consequences:
//!
//! - It cannot reach around the risk layer, because it has nothing to
//!   reach with. Hard limits are enforced between the intent and the
//!   book, not by asking the strategy to behave.
//! - It cannot introduce non-determinism, because it has no clock and
//!   no I/O. Whatever it does is a function of the events it saw.
//! - It can be tested without a venue, a journal, or a host: feed it a
//!   sequence of observations and read the intents back.
//!
//! The trait is deliberately small. Everything a richer strategy API
//! would add — indicators, position sizing, schedulers — is a library
//! on top of this, not a widening of the boundary the engine has to
//! trust.
//!
//! # Why this is its own crate
//!
//! It lived in `oq-backtest` until it had a second caller in sight. A
//! strategy that can only be defined inside the backtest host is a
//! strategy the live host cannot run without depending on the backtest
//! host, and the alternative — a second trait for live — is the exact
//! shape that makes "the same strategy, unchanged, in both modes"
//! unprovable. Moving it below both hosts is cheap while the only
//! implementations are in this repository and gets steadily more
//! expensive afterwards, so it happened before the live host existed
//! rather than during it.
//!
//! The dependencies are the floor: `oq-types` for the domain types and
//! `oq-engine` for the observation a strategy is handed. Both are plain
//! std Rust, so writing a strategy compiles three small crates and
//! nothing else — no journal, no margin tables, no venue client.

pub mod indicator;

pub use indicator::{Ema, Macd, MacdValue, Rsi, Sma, Warmup, Window};

use oq_types::{Fill, Offset, OrderId, PriceTicks, QtyLots, Side};

/// What a strategy wants to happen next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Rest a limit order.
    Limit {
        id: OrderId,
        side: Side,
        price: PriceTicks,
        qty: QtyLots,
        /// Whether this adds to a position or reduces one. Only matters
        /// under hedge accounting, where a buy while short is ambiguous
        /// without it; [`Intent::limit`] defaults it to `Open`.
        offset: Offset,
    },
    /// Cross the spread now.
    Market {
        id: OrderId,
        side: Side,
        qty: QtyLots,
        offset: Offset,
    },
    /// Withdraw a resting order.
    Cancel(OrderId),
    /// Withdraw everything.
    CancelAll,
}

/// What the strategy is told before it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// The observation that triggered this call.
    pub tick: oq_engine::Tick,
    /// Signed position: positive long, negative short.
    ///
    /// Under hedge accounting this is the long leg; the short is in
    /// `short_position`. Summing them gives the net, which is what a
    /// one-way account would have held.
    pub position: QtyLots,
    /// Average entry price, or zero when flat.
    pub entry: PriceTicks,
    /// The short leg under hedge accounting; zero under one-way netting,
    /// where opposing fills have already offset into `position`.
    pub short_position: QtyLots,
    /// Average entry of the short leg.
    pub short_entry: PriceTicks,
    /// Account equity at the current mark.
    pub equity: oq_types::Cash,
    /// Orders currently resting.
    pub working: usize,
}

impl Intent {
    /// A limit order that adds to a position.
    ///
    /// The common case, and the only one that exists under one-way
    /// netting. Use the variant directly to close a specific leg.
    #[must_use]
    pub const fn limit(id: OrderId, side: Side, price: PriceTicks, qty: QtyLots) -> Self {
        Self::Limit {
            id,
            side,
            price,
            qty,
            offset: Offset::Open,
        }
    }

    /// A market order that adds to a position.
    #[must_use]
    pub const fn market(id: OrderId, side: Side, qty: QtyLots) -> Self {
        Self::Market {
            id,
            side,
            qty,
            offset: Offset::Open,
        }
    }

    /// The same order, marked as reducing a position rather than adding.
    #[must_use]
    pub const fn closing(self) -> Self {
        match self {
            Self::Limit {
                id,
                side,
                price,
                qty,
                ..
            } => Self::Limit {
                id,
                side,
                price,
                qty,
                offset: Offset::Close,
            },
            Self::Market { id, side, qty, .. } => Self::Market {
                id,
                side,
                qty,
                offset: Offset::Close,
            },
            other => other,
        }
    }
}

/// A strategy: observations in, intents out.
pub trait Strategy {
    /// Called once per observation.
    ///
    /// `out` is cleared by the host before the call and drained after
    /// it, so an implementation appends and does not need to manage the
    /// buffer's lifetime. Returning intents rather than a `Vec` keeps
    /// the per-tick path free of allocation.
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>);

    /// Called once per fill, before [`Strategy::on_tick`] for the tick
    /// that produced it.
    ///
    /// Position management lives here for most strategies — a ladder
    /// that extends when an entry fills, a take-profit that re-prices
    /// as the average moves. A strategy that could not observe its own
    /// executions would be able to open positions and never manage
    /// them.
    ///
    /// The ordering matters and is fixed: every fill from a tick is
    /// delivered before the tick itself, and `ctx` already reflects the
    /// fill. A strategy therefore never sees a stale position, and
    /// never has to reconstruct one.
    fn on_fill(&mut self, _fill: &Fill, _ctx: &Context, _out: &mut Vec<Intent>) {}

    /// Called when the venue has answered a submission — and only then.
    ///
    /// A strategy that treats "I asked" as "it is resting" believes it
    /// holds exposure it does not have. That is not a hypothetical: a
    /// teaching example in this repository did exactly that, set its
    /// `placed` flag before the answer arrived, and went on to cancel an
    /// order the risk gate had refused:
    ///
    /// ```text
    /// refused        OrderId(1): OrderTooLarge { qty: 11, limit: 1 }
    /// unknown order  OrderId(1) — not in this run's map
    /// ```
    ///
    /// `accepted` is the venue's answer as a boolean, which is the one
    /// place in this project it is reduced to two values — and it is safe
    /// here precisely because it is not the whole answer. An unresolved
    /// placement is **not reported through this callback at all**: nobody
    /// knows yet, and telling a strategy `false` would be telling it the
    /// order does not exist. The host resolves it and calls back when
    /// there is an answer, or the strategy times out on its own, which
    /// is what `Placed::Unknown` obliges every caller to do.
    ///
    /// In a backtest this fires immediately after a submission the kernel
    /// accepted, so a strategy written against it behaves the same in
    /// both — which is the point of having it in the trait rather than in
    /// the live host.
    fn on_placed(&mut self, _id: OrderId, _accepted: bool) {}

    /// One historical observation, replayed before the run begins.
    ///
    /// **No intents can be produced here, and that is the point.** The
    /// reference implementation warms its indicators from a day of
    /// history and guards the whole load with a `preheating` flag that
    /// every trading path has to remember to check. A callback that
    /// cannot emit is the same rule enforced by the compiler instead.
    ///
    /// History arrives as ticks, in order, exactly as live data does, so
    /// a strategy folds them with the code it already has — its bar
    /// generator, its indicator windows — rather than a second path that
    /// has to agree with the first.
    ///
    /// Called before [`Strategy::on_tick`] ever is, and never again.
    ///
    /// The default ignores history. A strategy that needs none — one
    /// that decides from the current book — is not made to say so.
    fn on_history(&mut self, _ctx: &Context) {}

    /// What this strategy is waiting for, named, for the record.
    ///
    /// A run that does nothing is the hardest one to explain, because
    /// doing nothing leaves no trace: every record in the journal is
    /// something that happened. A twelve-hour run placed no orders and
    /// the reason — a gate whose threshold that deployment never
    /// reached — was reachable only by reading the strategy's source,
    /// which is the worst tool to reach for while something is going
    /// wrong.
    ///
    /// Return the conditions between this strategy and its next action,
    /// as `(name, value)`. Counters and thresholds, not prose: they are
    /// journalled and compared across runs, and a sentence cannot be.
    /// Typical entries are progress towards a warm-up (`bars`, `200`) or
    /// whether a gate is currently armed (`volume_gate`, `1`).
    ///
    /// Called on a timer rather than per tick, so the cost is a snapshot
    /// every thirty seconds and not a per-observation allocation.
    ///
    /// The default is empty: a strategy that says nothing is as
    /// unexplainable as before, which is a choice its author makes
    /// rather than one the framework makes for them.
    fn waiting_on(&self) -> Vec<(&'static str, i64)> {
        Vec::new()
    }

    /// A name for reports.
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::Stamp;

    /// The smallest strategy that does anything, used to check the
    /// boundary rather than any trading idea.
    struct BuyOnce {
        done: bool,
    }

    impl Strategy for BuyOnce {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            if !self.done && ctx.position.is_zero() {
                self.done = true;
                out.push(Intent::Market {
                    id: OrderId::new(1),
                    side: Side::Buy,
                    qty: QtyLots(1),
                    offset: oq_types::Offset::Open,
                });
            }
        }

        fn name(&self) -> &str {
            "buy-once"
        }
    }

    #[test]
    fn a_strategy_is_testable_without_a_host() {
        let mut s = BuyOnce { done: false };
        let ctx = Context {
            tick: oq_engine::Tick::trades_only(Stamp::synthetic(0), 100, 100, 100),
            position: QtyLots::ZERO,
            entry: PriceTicks::ZERO,
            short_position: QtyLots::ZERO,
            short_entry: PriceTicks::ZERO,
            equity: oq_types::Cash::from_units(1_000),
            working: 0,
        };
        let mut out = Vec::new();
        s.on_tick(&ctx, &mut out);
        assert_eq!(out.len(), 1);
        out.clear();
        s.on_tick(&ctx, &mut out);
        assert!(out.is_empty(), "it only buys once");
    }
}
