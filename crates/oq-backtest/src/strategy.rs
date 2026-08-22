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
