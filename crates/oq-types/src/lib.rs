//! Core domain types for OpenQuanter.
//!
//! Design constraints (see AGENTS.md at the workspace root):
//! - Prices and quantities are fixed-point integers on the hot path.
//! - Order state transitions are encoded so illegal transitions do not
//!   compile (typestate / exhaustive enums).
//! - All event types carry dual timestamps: `exch_ts` and `local_ts`
//!   (nanoseconds). Serialized layouts are append-only; fields are never
//!   repurposed.

/// Fixed-point price expressed in integer ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PriceTicks(pub i64);

/// Fixed-point quantity expressed in integer lots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QtyLots(pub i64);

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_ticks_ord() {
        assert!(PriceTicks(2) > PriceTicks(1));
    }
}
