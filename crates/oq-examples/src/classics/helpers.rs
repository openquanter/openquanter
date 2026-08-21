//! The two lines every strategy in the catalogue would otherwise repeat.
//!
//! Order ids and the open/close offset, in one place. Not a base class:
//! `FR-STRAT-6` says the building blocks ship as components rather than
//! as an inheritance hierarchy a strategy must join, and this is a field
//! a strategy holds, not a parent it inherits.

use oq_backtest::{Context, Intent};
use oq_types::{Offset, OrderId, QtyLots, Side};

/// Issues order ids and the right offset.
#[derive(Debug, Clone, Default)]
pub struct Trader {
    next: u64,
}

impl Trader {
    /// A fresh id sequence.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// The last id issued.
    #[must_use]
    pub const fn last_id(&self) -> OrderId {
        OrderId(self.next)
    }

    /// Open exposure.
    pub fn open(&mut self, ctx: &Context, out: &mut Vec<Intent>, side: Side, qty: QtyLots) {
        self.next += 1;
        out.push(Intent::Market {
            instrument: ctx.instrument,
            id: OrderId(self.next),
            side,
            qty,
            offset: Offset::Open,
        });
    }

    /// Reduce exposure.
    ///
    /// Takes the quantity to close rather than assuming one lot: a
    /// strategy that scaled in and closes one lot leaves a position it
    /// believes it exited.
    pub fn close(&mut self, ctx: &Context, out: &mut Vec<Intent>, side: Side, qty: QtyLots) {
        if qty.0 <= 0 {
            return;
        }
        self.next += 1;
        out.push(Intent::Market {
            instrument: ctx.instrument,
            id: OrderId(self.next),
            side,
            qty: QtyLots(qty.0.abs()),
            offset: Offset::Close,
        });
    }
}
