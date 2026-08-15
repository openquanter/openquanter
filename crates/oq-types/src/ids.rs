//! Identifiers.
//!
//! All of them are newtypes over integers rather than strings. Two
//! reasons, in order of importance:
//!
//! 1. **A `u64` cannot be confused with another `u64` of a different
//!    kind once it is wrapped.** Passing an order id where a trade id
//!    belongs is the kind of bug that survives review and shows up as
//!    a reconciliation mismatch weeks later.
//! 2. **Integers are cheap in the hot path.** Comparison, hashing, and
//!    indexing are all a single instruction, and the values fit in
//!    registers rather than pointing into the heap.
//!
//! Instrument symbols are strings at the edges — that is what venues
//! speak — and are interned into [`InstrumentId`] on the way in.

/// A position in the sequenced event stream.
///
/// Assigned by the sequencer, strictly increasing, gapless within a
/// journal. It is the coordinate every other artifact refers to:
/// a snapshot is "state as of sequence N", a parity divergence is "the
/// two runs first differed at sequence N", a replay starts at N.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SeqNo(pub u64);

impl SeqNo {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// An instrument, interned from its venue symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct InstrumentId(pub u32);

impl InstrumentId {
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

/// An order, unique within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OrderId(pub u64);

impl OrderId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// An execution, unique within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TradeId(pub u64);

impl TradeId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A strategy instance.
///
/// Distinct from the strategy *type*: a parameter sweep runs many
/// instances of one type, and their state must not be able to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct StrategyId(pub u32);

impl StrategyId {
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

/// Monotonic id allocation for a single run.
///
/// Deliberately not thread-safe and not global: ids are assigned inside
/// the deterministic core, where there is exactly one writer. A shared
/// atomic counter would make id assignment depend on thread interleaving
/// and would silently destroy replay reproducibility.
#[derive(Debug, Default)]
pub struct IdAllocator {
    next_order: u64,
    next_trade: u64,
}

impl IdAllocator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_order: 0,
            next_trade: 0,
        }
    }

    pub fn order(&mut self) -> OrderId {
        self.next_order += 1;
        OrderId(self.next_order)
    }

    pub fn trade(&mut self) -> TradeId {
        self.next_trade += 1;
        TradeId(self.next_trade)
    }

    /// Ids issued so far, for snapshotting.
    #[must_use]
    pub const fn watermark(&self) -> (u64, u64) {
        (self.next_order, self.next_trade)
    }

    /// Restore from a snapshot so that ids continue rather than repeat.
    ///
    /// Repeating an id after recovery would make two different trades
    /// indistinguishable in the journal, which defeats the audit
    /// property the journal exists for.
    pub fn restore(&mut self, watermark: (u64, u64)) {
        self.next_order = watermark.0;
        self.next_trade = watermark.1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_monotonic_and_starts_at_one() {
        let mut ids = IdAllocator::new();
        assert_eq!(ids.order(), OrderId(1));
        assert_eq!(ids.order(), OrderId(2));
        assert_eq!(ids.trade(), TradeId(1));
    }

    #[test]
    fn restore_continues_rather_than_repeats() {
        let mut ids = IdAllocator::new();
        ids.order();
        ids.order();
        let mark = ids.watermark();

        let mut recovered = IdAllocator::new();
        recovered.restore(mark);
        assert_eq!(recovered.order(), OrderId(3));
    }
}
