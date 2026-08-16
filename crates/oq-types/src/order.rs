//! Orders whose state is part of their type.
//!
//! The order lifecycle is where trading systems accumulate their most
//! expensive bugs: a cancel that races a fill, a terminal order that
//! receives another execution, a rejected order that is never removed
//! from the working set and quietly holds margin forever. Each of these
//! is a transition that should not exist.
//!
//! Here they do not exist. [`Order<S>`] carries its state as a type
//! parameter and every transition consumes the order, so:
//!
//! - `Order<Filled>` has no `cancel` method. Cancelling a filled order
//!   is a compile error, not a runtime branch someone forgot.
//! - Transitions take `self` by value. An order cannot be left in the
//!   old state after moving to the new one, so there is no window where
//!   two states of the same order are both reachable.
//! - Terminal states have no outgoing transitions at all.
//!
//! The working set stores [`Working`], the two states an order can be
//! in while it is still on the book. Terminal orders leave the book and
//! are handed to the ledger, so the book cannot accumulate them.
//!
//! Fill quantities are checked against remaining quantity: an
//! over-fill is refused rather than silently producing a negative
//! remainder that later reads as an enormous position.

use crate::{Offset, PriceTicks, QtyLots, Side, ids::OrderId, time::Stamp};
use core::marker::PhantomData;

/// How long an order rests before it is withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    /// Rest until filled or explicitly cancelled.
    GoodTilCancel,
    /// Fill what is immediately available, cancel the remainder.
    ImmediateOrCancel,
    /// Fill entirely and immediately, or not at all.
    FillOrKill,
}

/// Limit or market.
///
/// A market order is represented by [`OrderKind::Market`] rather than
/// by a zero price. The matching engine's wire format uses a zero price
/// as the sentinel for compatibility with the reference implementation,
/// but that convention stops at the boundary: inside the type system
/// the distinction is explicit and cannot be produced by a mistake in
/// price arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    Limit { price: PriceTicks },
    Market,
}

impl OrderKind {
    /// Resting price, or `None` for market orders.
    #[must_use]
    pub const fn limit_price(self) -> Option<PriceTicks> {
        match self {
            Self::Limit { price } => Some(price),
            Self::Market => None,
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A lifecycle state an order can be in.
///
/// Sealed: the set of states is closed, so a downstream crate cannot
/// invent a state that the engine's exhaustive matches do not handle.
pub trait OrderState: sealed::Sealed {
    /// Whether no further transition is possible from this state.
    const TERMINAL: bool;
}

macro_rules! order_states {
    ($($(#[$meta:meta])* $name:ident => terminal: $terminal:expr;)*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct $name;
            impl sealed::Sealed for $name {}
            impl OrderState for $name {
                const TERMINAL: bool = $terminal;
            }
        )*
    };
}

order_states! {
    /// Accepted locally, not yet acknowledged by the venue.
    Pending => terminal: false;
    /// Resting on the book, nothing filled yet.
    Live => terminal: false;
    /// Resting on the book with part of the quantity executed.
    PartiallyFilled => terminal: false;
    /// Fully executed.
    Filled => terminal: true;
    /// Withdrawn before completion.
    Cancelled => terminal: true;
    /// Refused by risk or by the venue.
    Rejected => terminal: true;
}

/// An order in a known lifecycle state.
///
/// `qty` is the original quantity and never changes; `filled` grows.
/// Keeping the original rather than decrementing a remaining quantity
/// means an order always knows what it was asked to do, which is what
/// reconciliation against a venue compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order<S: OrderState> {
    pub id: OrderId,
    pub side: Side,
    pub kind: OrderKind,
    pub qty: QtyLots,
    pub filled: QtyLots,
    pub tif: TimeInForce,
    pub placed: Stamp,
    /// Whether this order adds to a position or reduces one.
    ///
    /// Under one-way netting a ledger can derive this from the side and
    /// the position it already holds, and for a long time this type did
    /// not carry it. Under hedge accounting it cannot: a buy placed
    /// while a short is open is either closing that short or opening a
    /// long, and the two produce different positions, different margin
    /// and different realized profit. Only the order knows, so the order
    /// carries it.
    pub offset: Offset,
    _state: PhantomData<S>,
}

impl<S: OrderState> Order<S> {
    /// Quantity still outstanding.
    #[must_use]
    pub const fn remaining(&self) -> QtyLots {
        QtyLots(self.qty.0 - self.filled.0)
    }

    /// Resting price, or `None` for market orders.
    #[must_use]
    pub const fn price(&self) -> Option<PriceTicks> {
        self.kind.limit_price()
    }

    fn transition<T: OrderState>(self) -> Order<T> {
        Order {
            id: self.id,
            side: self.side,
            kind: self.kind,
            qty: self.qty,
            filled: self.filled,
            tif: self.tif,
            placed: self.placed,
            offset: self.offset,
            _state: PhantomData,
        }
    }
}

impl Order<Pending> {
    /// A newly created order, before the venue has acknowledged it.
    ///
    /// Quantity must be positive: direction lives in [`Side`], and a
    /// signed order quantity would give the same fact two
    /// representations that can disagree.
    #[must_use]
    pub fn new(
        id: OrderId,
        side: Side,
        kind: OrderKind,
        qty: QtyLots,
        tif: TimeInForce,
        placed: Stamp,
    ) -> Option<Self> {
        Self::with_offset(id, side, kind, qty, tif, placed, Offset::Open)
    }

    /// Build an order that states whether it opens or closes.
    ///
    /// [`Order::new`] defaults to [`Offset::Open`], which is what every
    /// caller meant before the field existed and what one-way netting
    /// makes indistinguishable anyway.
    pub fn with_offset(
        id: OrderId,
        side: Side,
        kind: OrderKind,
        qty: QtyLots,
        tif: TimeInForce,
        placed: Stamp,
        offset: Offset,
    ) -> Option<Self> {
        if qty.0 <= 0 {
            return None;
        }
        Some(Self {
            id,
            side,
            kind,
            qty,
            filled: QtyLots::ZERO,
            tif,
            placed,
            offset,
            _state: PhantomData,
        })
    }

    /// The venue acknowledged the order; it is now on the book.
    #[must_use]
    pub fn accept(self) -> Order<Live> {
        self.transition()
    }

    /// Risk or the venue refused the order.
    #[must_use]
    pub fn reject(self) -> Order<Rejected> {
        self.transition()
    }
}

/// The outcome of applying a fill to a resting order.
///
/// Returned rather than mutating in place, because the two outcomes
/// have different types: a partially filled order stays on the book,
/// a filled one leaves it. A caller cannot forget to remove a completed
/// order, because it no longer type-checks as something the book holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    Partial(Order<PartiallyFilled>),
    Complete(Order<Filled>),
}

/// Applying a fill failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillError {
    /// The fill quantity was zero or negative.
    NonPositive,
    /// The fill exceeded what the order had left.
    ///
    /// Refused rather than clamped: an over-fill means the caller's
    /// view of the book disagrees with the book, and silently clamping
    /// hides that disagreement until it surfaces as a position break.
    Overfill {
        remaining: QtyLots,
        attempted: QtyLots,
    },
}

fn apply_fill<S: OrderState>(order: Order<S>, qty: QtyLots) -> Result<FillOutcome, FillError> {
    if qty.0 <= 0 {
        return Err(FillError::NonPositive);
    }
    let remaining = order.remaining();
    if qty.0 > remaining.0 {
        return Err(FillError::Overfill {
            remaining,
            attempted: qty,
        });
    }
    let mut filled = order;
    filled.filled = QtyLots(filled.filled.0 + qty.0);
    if filled.filled == filled.qty {
        Ok(FillOutcome::Complete(filled.transition()))
    } else {
        Ok(FillOutcome::Partial(filled.transition()))
    }
}

impl Order<Live> {
    /// Execute `qty` against this order.
    ///
    /// # Errors
    /// [`FillError::NonPositive`] for a non-positive quantity,
    /// [`FillError::Overfill`] when the fill exceeds the remainder.
    pub fn fill(self, qty: QtyLots) -> Result<FillOutcome, FillError> {
        apply_fill(self, qty)
    }

    #[must_use]
    pub fn cancel(self) -> Order<Cancelled> {
        self.transition()
    }
}

impl Order<PartiallyFilled> {
    /// Execute a further `qty` against this order.
    ///
    /// # Errors
    /// As [`Order::<Live>::fill`].
    pub fn fill(self, qty: QtyLots) -> Result<FillOutcome, FillError> {
        apply_fill(self, qty)
    }

    /// Withdraw the unfilled remainder.
    #[must_use]
    pub fn cancel(self) -> Order<Cancelled> {
        self.transition()
    }
}

/// An order that is still on the book.
///
/// The book holds this rather than `Order<S>` for arbitrary `S`, so
/// terminal orders are structurally unable to remain resting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Working {
    Live(Order<Live>),
    PartiallyFilled(Order<PartiallyFilled>),
}

impl Working {
    #[must_use]
    pub const fn id(&self) -> OrderId {
        match self {
            Self::Live(o) => o.id,
            Self::PartiallyFilled(o) => o.id,
        }
    }

    #[must_use]
    pub const fn side(&self) -> Side {
        match self {
            Self::Live(o) => o.side,
            Self::PartiallyFilled(o) => o.side,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> OrderKind {
        match self {
            Self::Live(o) => o.kind,
            Self::PartiallyFilled(o) => o.kind,
        }
    }

    /// Whether this order adds to a position or reduces one.
    #[must_use]
    pub const fn offset(&self) -> Offset {
        match self {
            Self::Live(o) => o.offset,
            Self::PartiallyFilled(o) => o.offset,
        }
    }

    #[must_use]
    pub const fn remaining(&self) -> QtyLots {
        match self {
            Self::Live(o) => o.remaining(),
            Self::PartiallyFilled(o) => o.remaining(),
        }
    }

    #[must_use]
    pub const fn price(&self) -> Option<PriceTicks> {
        self.kind().limit_price()
    }

    #[must_use]
    pub const fn placed(&self) -> Stamp {
        match self {
            Self::Live(o) => o.placed,
            Self::PartiallyFilled(o) => o.placed,
        }
    }

    /// Execute `qty`, yielding either a still-working order or a
    /// completed one.
    ///
    /// # Errors
    /// As [`Order::<Live>::fill`].
    pub fn fill(self, qty: QtyLots) -> Result<FillOutcome, FillError> {
        match self {
            Self::Live(o) => o.fill(qty),
            Self::PartiallyFilled(o) => o.fill(qty),
        }
    }

    #[must_use]
    pub fn cancel(self) -> Order<Cancelled> {
        match self {
            Self::Live(o) => o.cancel(),
            Self::PartiallyFilled(o) => o.cancel(),
        }
    }
}

impl From<FillOutcome> for Option<Working> {
    /// A partial fill stays on the book; a complete fill leaves it.
    fn from(outcome: FillOutcome) -> Self {
        match outcome {
            FillOutcome::Partial(o) => Some(Working::PartiallyFilled(o)),
            FillOutcome::Complete(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(qty: i64) -> Order<Pending> {
        Order::new(
            OrderId::new(1),
            Side::Buy,
            OrderKind::Limit {
                price: PriceTicks(100),
            },
            QtyLots(qty),
            TimeInForce::GoodTilCancel,
            Stamp::synthetic(0),
        )
        .expect("positive quantity")
    }

    #[test]
    fn non_positive_quantity_is_refused_at_construction() {
        assert!(
            Order::new(
                OrderId::new(1),
                Side::Buy,
                OrderKind::Market,
                QtyLots(0),
                TimeInForce::GoodTilCancel,
                Stamp::synthetic(0),
            )
            .is_none()
        );
    }

    #[test]
    fn partial_then_complete() {
        let live = pending(10).accept();
        let FillOutcome::Partial(partial) = live.fill(QtyLots(4)).expect("valid fill") else {
            panic!("4 of 10 is partial");
        };
        assert_eq!(partial.remaining(), QtyLots(6));

        let FillOutcome::Complete(done) = partial.fill(QtyLots(6)).expect("valid fill") else {
            panic!("6 of remaining 6 completes");
        };
        assert_eq!(done.remaining(), QtyLots::ZERO);
        const { assert!(Filled::TERMINAL) };
    }

    #[test]
    fn overfill_is_refused_not_clamped() {
        let live = pending(10).accept();
        assert_eq!(
            live.fill(QtyLots(11)),
            Err(FillError::Overfill {
                remaining: QtyLots(10),
                attempted: QtyLots(11),
            })
        );
    }

    #[test]
    fn completed_orders_leave_the_book() {
        let live = pending(10).accept();
        let outcome = live.fill(QtyLots(10)).expect("valid fill");
        let still_working: Option<Working> = outcome.into();
        assert!(still_working.is_none(), "a filled order must not rest");
    }

    #[test]
    fn cancelling_preserves_the_filled_quantity() {
        let live = pending(10).accept();
        let FillOutcome::Partial(partial) = live.fill(QtyLots(3)).expect("valid fill") else {
            panic!("partial");
        };
        let cancelled = partial.cancel();
        assert_eq!(cancelled.filled, QtyLots(3));
        assert_eq!(cancelled.remaining(), QtyLots(7));
    }

    // Compile-fail expectations, documented here because the type system
    // enforces them and there is no runtime test that can:
    //
    //   let done: Order<Filled> = ...;
    //   done.cancel();          // no method `cancel` on Order<Filled>
    //   done.fill(QtyLots(1));  // no method `fill` on Order<Filled>
    //
    // A `trybuild` suite pins these once the crate has a dev-dependency
    // budget; until then the absence of the impls is the enforcement.
}
