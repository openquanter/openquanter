//! What a parity run compares.
//!
//! Fills are the unit of comparison, not equity curves: two runs whose
//! final P&L agrees can still have taken different trades, and a summary
//! statistic hides exactly the divergence a port needs to find. Prices
//! and quantities are fixed-point integers, so their comparison is exact
//! — a tolerance is only ever applied to derived monetary values.

use oq_types::{PriceTicks, QtyLots, Side};

/// Nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nanos(pub i64);

/// One fill produced by a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// Exchange timestamp of the fill.
    pub ts: Nanos,
    /// Instrument symbol as the run reported it.
    pub symbol: String,
    /// Buy or sell.
    pub side: Side,
    /// Fill price in ticks. Exact by construction.
    pub price: PriceTicks,
    /// Filled quantity in lots. Exact by construction.
    pub qty: QtyLots,
    /// Identifier the strategy or engine assigned, when there is one.
    pub tag: Option<String>,
}

impl Fill {
    /// A fill with no tag.
    #[must_use]
    pub fn new(ts: i64, symbol: impl Into<String>, side: Side, price: i64, qty: i64) -> Self {
        Self {
            ts: Nanos(ts),
            symbol: symbol.into(),
            side,
            price: PriceTicks(price),
            qty: QtyLots(qty),
            tag: None,
        }
    }

    /// The same fill, tagged.
    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Whether two fills are the same event: everything except the tag,
    /// which is metadata rather than behavior.
    #[must_use]
    pub fn same_event(&self, other: &Self) -> bool {
        self.ts == other.ts
            && self.symbol == other.symbol
            && self.side == other.side
            && self.price == other.price
            && self.qty == other.qty
    }
}

/// The output of one run: its fills and its realized profit and loss.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutput {
    /// Fills in the order the run produced them.
    pub fills: Vec<Fill>,
    /// Realized P&L in account currency units.
    pub pnl: f64,
}

impl RunOutput {
    /// A run output.
    #[must_use]
    pub fn new(fills: Vec<Fill>, pnl: f64) -> Self {
        Self { fills, pnl }
    }
}
