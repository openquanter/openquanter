//! The vocabulary every other crate speaks.
//!
//! Three decisions shape this crate, and each one is a decision about
//! what the compiler will refuse rather than what a reviewer will catch:
//!
//! 1. **Money is integers.** Prices are tick counts, quantities are lot
//!    counts, cash is a fixed-point integer. Floating point enters only
//!    at the reporting boundary. Two runs that agree must agree exactly,
//!    and exact agreement is not something binary floating point offers
//!    across compilers, architectures, or optimization levels.
//! 2. **Illegal order transitions do not compile.** An order is a value
//!    whose type names its state ([`order`]). "Cancel a filled order" is
//!    a type error, not a runtime branch that someone forgets to write.
//! 3. **Every market observation carries two clocks.** [`Stamp`] pairs
//!    the exchange's timestamp with the local receipt timestamp. Their
//!    difference is feed latency; a record that kept only one of them
//!    cannot be used to model latency later, and the information is
//!    unrecoverable after capture.
//!
//! Serialized layouts here are append-only. A field never changes
//! meaning between versions — reusing a field's meaning across a
//! deployment boundary is the mechanism behind some of the most
//! expensive incidents in electronic trading.

#![forbid(unsafe_code)]

pub mod currency;
pub mod fixed;
pub mod ids;
pub mod instrument;
pub mod order;
pub mod time;

pub use currency::{Balances, Currency, RATE_SCALE, Rates};
pub use fixed::{CASH_SCALE, Cash, PriceTicks, QtyLots, RATIO_SCALE, Ratio};
pub use ids::{IdAllocator, InstrumentId, OrderId, SeqNo, StrategyId, TradeId};
pub use instrument::{CONTRACT_SCALE, Instrument};
pub use order::{
    Cancelled, FillError, FillOutcome, Filled, Live, Order, OrderKind, OrderState, PartiallyFilled,
    Pending, Rejected, TimeInForce, Working,
};
pub use time::{Nanos, Stamp};

/// Which way an order or position points.
///
/// Deliberately two-valued: "flat" is a position size of zero, not a
/// third side. Encoding flat as a side invites the arithmetic bug where
/// a flat position accumulates a direction it never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// The side that closes a position opened on this side.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }

    /// `+1` for buys, `-1` for sells.
    ///
    /// The sign convention every position update in the workspace uses:
    /// signed quantity, never a separate direction flag beside a
    /// magnitude. Two representations of the same fact drift apart.
    #[must_use]
    pub const fn sign(self) -> i64 {
        match self {
            Self::Buy => 1,
            Self::Sell => -1,
        }
    }
}

/// Whether an execution added liquidity or removed it.
///
/// Fee schedules differ by an order of magnitude between the two, and
/// on a strategy that trades often the difference dominates the P&L.
/// It is recorded per fill rather than inferred later, because the
/// information is only unambiguous at the moment of the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liquidity {
    Maker,
    Taker,
}

/// Whether an order opens exposure or reduces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    Open,
    Close,
}

/// One execution: the atom of everything downstream.
///
/// Fills are what parity compares, what the ledger applies, and what
/// reports aggregate. Equity curves are derived; fills are the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub stamp: Stamp,
    pub instrument: InstrumentId,
    pub order: OrderId,
    pub trade: TradeId,
    pub side: Side,
    pub offset: Offset,
    pub price: PriceTicks,
    pub qty: QtyLots,
    pub liquidity: Liquidity,
}

impl Fill {
    /// Signed position change this fill causes.
    #[must_use]
    pub const fn position_delta(&self) -> QtyLots {
        QtyLots(self.qty.0 * self.side.sign())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_opposite_is_an_involution() {
        assert_eq!(Side::Buy.opposite().opposite(), Side::Buy);
        assert_eq!(Side::Sell.opposite().opposite(), Side::Sell);
    }

    #[test]
    fn position_delta_follows_side() {
        let fill = Fill {
            stamp: Stamp::new(1, 2),
            instrument: InstrumentId::new(7),
            order: OrderId::new(1),
            trade: TradeId::new(1),
            side: Side::Sell,
            offset: Offset::Open,
            price: PriceTicks(100),
            qty: QtyLots(5),
            liquidity: Liquidity::Maker,
        };
        assert_eq!(fill.position_delta(), QtyLots(-5));
    }
}
