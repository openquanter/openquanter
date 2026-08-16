//! Rebuilding an order book from incremental depth.
//!
//! This is the tool that decides whether a capture archive is worth
//! anything. Writing depth updates to disk proves they arrived; only
//! replaying them into a book proves they can be *used*. A capture with
//! a subtle defect — a mishandled gap, a misread sequence field —
//! looks perfectly healthy on disk and is discovered months later, when
//! the archive is large and the mistake is unrepeatable.
//!
//! So the reconstruction is deliberately strict. It refuses to apply an
//! update it cannot place in sequence, and it says which rule was
//! violated. A book that quietly absorbs an out-of-order message is
//! worse than one that stops: the first produces plausible prices that
//! are wrong, and nothing downstream can tell.
//!
//! ## The sequencing rules
//!
//! Every update covers a range of ids, `first_id..=final_id`, and
//! carries the previous message's `final_id` as `prev_final_id`.
//!
//! - Updates entirely older than the snapshot are dropped: they are
//!   already reflected in it.
//! - The first update applied must straddle the snapshot:
//!   `first_id <= snapshot + 1 <= final_id`.
//! - Every update after that must continue the chain, its
//!   `prev_final_id` matching the last `final_id` seen.
//!
//! A break in the chain means messages were lost, and the only correct
//! response is a fresh snapshot. Guessing the missing state is how a
//! reconstruction becomes fiction.

use crate::depth::{DepthUpdate, Level};

/// One side of a book, price-ordered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Side {
    /// Levels, best first: descending for bids, ascending for asks.
    levels: Vec<Level>,
    descending: bool,
}

impl Side {
    /// An empty bid side (best = highest price).
    #[must_use]
    pub fn bids() -> Self {
        Self {
            levels: Vec::new(),
            descending: true,
        }
    }

    /// An empty ask side (best = lowest price).
    #[must_use]
    pub fn asks() -> Self {
        Self {
            levels: Vec::new(),
            descending: false,
        }
    }

    fn position(&self, price: i64) -> Result<usize, usize> {
        self.levels.binary_search_by(|level| {
            if self.descending {
                price.cmp(&level.price)
            } else {
                level.price.cmp(&price)
            }
        })
    }

    /// Apply one level change. A zero quantity removes the level.
    pub fn apply(&mut self, level: Level) {
        match (self.position(level.price), level.qty) {
            (Ok(index), 0) => {
                self.levels.remove(index);
            }
            (Ok(index), _) => self.levels[index] = level,
            (Err(_), 0) => {
                // Removing a level that was never there is normal: the
                // venue reports the removal whether or not the snapshot
                // that started this book happened to include it.
            }
            (Err(index), _) => self.levels.insert(index, level),
        }
    }

    /// Levels, best first.
    #[must_use]
    pub fn levels(&self) -> &[Level] {
        &self.levels
    }

    /// The best price and quantity, if the side is not empty.
    #[must_use]
    pub fn best(&self) -> Option<Level> {
        self.levels.first().copied()
    }

    /// Number of levels held.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.levels.len()
    }
}

/// Why an update could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceError {
    /// The book has no snapshot yet, so there is nothing to apply to.
    NoSnapshot,
    /// The first update after a snapshot did not straddle it.
    DoesNotStraddleSnapshot {
        snapshot: u64,
        first_id: u64,
        final_id: u64,
    },
    /// The chain broke: messages were lost between these two ids.
    Gap { expected: u64, found: u64 },
}

impl core::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSnapshot => f.write_str("no snapshot: nothing to apply updates to"),
            Self::DoesNotStraddleSnapshot {
                snapshot,
                first_id,
                final_id,
            } => write!(
                f,
                "first update {first_id}..={final_id} does not straddle snapshot {snapshot}"
            ),
            Self::Gap { expected, found } => {
                write!(
                    f,
                    "sequence gap: expected to continue from {expected}, got {found}"
                )
            }
        }
    }
}

impl core::error::Error for SequenceError {}

/// What applying an update did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The book advanced.
    Updated,
    /// The update predates the snapshot and was already reflected in it.
    AlreadyInSnapshot,
}

/// A book rebuilt from a snapshot plus incremental updates.
#[derive(Debug, Clone)]
pub struct Book {
    bids: Side,
    asks: Side,
    last_id: Option<u64>,
    snapshot_id: Option<u64>,
    applied: u64,
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

impl Book {
    /// An empty book, awaiting a snapshot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bids: Side::bids(),
            asks: Side::asks(),
            last_id: None,
            snapshot_id: None,
            applied: 0,
        }
    }

    /// Install a snapshot, discarding whatever was there.
    ///
    /// Used at the start and after any gap. Resetting rather than
    /// merging is deliberate: a book that survived a gap has unknown
    /// state, and merging into unknown state produces a book that looks
    /// right and is not.
    pub fn install_snapshot(&mut self, update_id: u64, bids: &[Level], asks: &[Level]) {
        self.bids = Side::bids();
        self.asks = Side::asks();
        for level in bids {
            self.bids.apply(*level);
        }
        for level in asks {
            self.asks.apply(*level);
        }
        self.snapshot_id = Some(update_id);
        self.last_id = None;
        self.applied = 0;
    }

    /// Apply an incremental update.
    ///
    /// # Errors
    ///
    /// [`SequenceError`] when the update cannot be placed in sequence.
    /// The book is left untouched, so a caller that resynchronizes from
    /// a fresh snapshot loses nothing.
    pub fn apply(&mut self, update: &DepthUpdate) -> Result<Applied, SequenceError> {
        let snapshot = self.snapshot_id.ok_or(SequenceError::NoSnapshot)?;

        match self.last_id {
            None => {
                if update.final_id <= snapshot {
                    return Ok(Applied::AlreadyInSnapshot);
                }
                if update.first_id > snapshot + 1 {
                    return Err(SequenceError::DoesNotStraddleSnapshot {
                        snapshot,
                        first_id: update.first_id,
                        final_id: update.final_id,
                    });
                }
            }
            Some(last) => {
                // The venue supplies the previous final id; when it does
                // not, fall back to requiring contiguity. Saturating
                // rather than subtracting: `first_id` is venue data, and
                // a zero there must report a gap, not overflow.
                let previous = update
                    .prev_final_id
                    .unwrap_or(update.first_id.saturating_sub(1));
                if previous != last {
                    return Err(SequenceError::Gap {
                        expected: last,
                        found: previous,
                    });
                }
            }
        }

        for level in &update.bids {
            self.bids.apply(*level);
        }
        for level in &update.asks {
            self.asks.apply(*level);
        }
        self.last_id = Some(update.final_id);
        self.applied += 1;
        Ok(Applied::Updated)
    }

    /// The bid side.
    #[must_use]
    pub fn bids(&self) -> &Side {
        &self.bids
    }

    /// The ask side.
    #[must_use]
    pub fn asks(&self) -> &Side {
        &self.asks
    }

    /// Updates applied since the last snapshot.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Whether the book is ready to be read.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.snapshot_id.is_some() && !self.bids.levels.is_empty() && !self.asks.levels.is_empty()
    }

    /// Whether the best bid is below the best ask.
    ///
    /// A crossed book is not a market condition, it is a reconstruction
    /// bug: a level that should have been removed was kept, or an
    /// update was applied out of order. Checked rather than assumed,
    /// because it is the cheapest signal that the replay went wrong.
    #[must_use]
    pub fn is_crossed(&self) -> bool {
        match (self.bids.best(), self.asks.best()) {
            (Some(bid), Some(ask)) => bid.price >= ask.price,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: i64, qty: i64) -> Level {
        Level { price, qty }
    }

    fn update(first: u64, final_: u64, prev: Option<u64>, bids: Vec<Level>) -> DepthUpdate {
        DepthUpdate {
            event_ms: 0,
            first_id: first,
            final_id: final_,
            prev_final_id: prev,
            bids,
            asks: Vec::new(),
        }
    }

    #[test]
    fn sides_order_best_first() {
        let mut bids = Side::bids();
        for p in [100, 102, 101] {
            bids.apply(level(p, 1));
        }
        assert_eq!(
            bids.levels().iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![102, 101, 100]
        );

        let mut asks = Side::asks();
        for p in [200, 198, 199] {
            asks.apply(level(p, 1));
        }
        assert_eq!(
            asks.levels().iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![198, 199, 200]
        );
    }

    #[test]
    fn a_zero_quantity_removes_a_level() {
        let mut bids = Side::bids();
        bids.apply(level(100, 5));
        bids.apply(level(101, 5));
        assert_eq!(bids.depth(), 2);
        bids.apply(level(100, 0));
        assert_eq!(bids.levels(), &[level(101, 5)]);
    }

    #[test]
    fn removing_a_level_that_was_never_there_is_not_an_error() {
        // Normal: the venue reports the removal whether or not the
        // snapshot this book started from happened to include it.
        let mut bids = Side::bids();
        bids.apply(level(100, 0));
        assert_eq!(bids.depth(), 0);
    }

    #[test]
    fn updates_before_the_snapshot_are_already_reflected_in_it() {
        let mut book = Book::new();
        book.install_snapshot(1_000, &[level(100, 1)], &[level(101, 1)]);
        assert_eq!(
            book.apply(&update(900, 950, None, vec![level(99, 7)])),
            Ok(Applied::AlreadyInSnapshot)
        );
        // and did not touch the book
        assert_eq!(book.bids().depth(), 1);
    }

    #[test]
    fn the_first_update_must_straddle_the_snapshot() {
        let mut book = Book::new();
        book.install_snapshot(1_000, &[level(100, 1)], &[level(101, 1)]);

        // Starts after snapshot+1: messages in between were missed.
        assert_eq!(
            book.apply(&update(1_005, 1_010, None, vec![])),
            Err(SequenceError::DoesNotStraddleSnapshot {
                snapshot: 1_000,
                first_id: 1_005,
                final_id: 1_010
            })
        );

        // Straddling is accepted.
        assert_eq!(
            book.apply(&update(998, 1_002, None, vec![level(100, 3)])),
            Ok(Applied::Updated)
        );
        assert_eq!(book.bids().best(), Some(level(100, 3)));
    }

    #[test]
    fn a_broken_chain_is_refused_and_leaves_the_book_untouched() {
        let mut book = Book::new();
        book.install_snapshot(1_000, &[level(100, 1)], &[level(101, 1)]);
        book.apply(&update(1_000, 1_001, None, vec![level(100, 2)]))
            .expect("straddles");

        // pu says it follows 1_500, but the book is at 1_001.
        let err = book
            .apply(&update(1_501, 1_502, Some(1_500), vec![level(100, 9)]))
            .expect_err("gap must be refused");
        assert_eq!(
            err,
            SequenceError::Gap {
                expected: 1_001,
                found: 1_500
            }
        );
        assert_eq!(
            book.bids().best(),
            Some(level(100, 2)),
            "a refused update must not have been partially applied"
        );
    }

    #[test]
    fn a_snapshot_resets_rather_than_merges() {
        // After a gap the book's state is unknown, and merging into
        // unknown state produces a book that looks right and is not.
        let mut book = Book::new();
        book.install_snapshot(1_000, &[level(100, 1), level(99, 1)], &[level(101, 1)]);
        book.install_snapshot(2_000, &[level(50, 4)], &[level(51, 4)]);
        assert_eq!(book.bids().levels(), &[level(50, 4)]);
        assert_eq!(book.asks().levels(), &[level(51, 4)]);
    }

    #[test]
    fn a_snapshot_resets_the_applied_count() {
        // `applied()` reports updates since the last snapshot, so a
        // resynchronization has to start the count again — otherwise
        // "how far did this book get before it broke" is unanswerable.
        let mut book = Book::new();
        book.install_snapshot(1_000, &[level(100, 1)], &[level(101, 1)]);
        book.apply(&update(1_000, 1_001, None, vec![level(100, 2)]))
            .expect("straddles");
        assert_eq!(book.applied(), 1);

        book.install_snapshot(2_000, &[level(100, 1)], &[level(101, 1)]);
        assert_eq!(book.applied(), 0);
    }

    #[test]
    fn a_first_id_of_zero_reports_a_gap_rather_than_overflowing() {
        // `first_id` is venue data. Without `pu` the fallback derives
        // the previous id from it, and a zero must produce a gap report
        // rather than an underflow.
        let mut book = Book::new();
        book.install_snapshot(0, &[level(100, 1)], &[level(101, 1)]);
        book.apply(&update(0, 1, None, vec![level(100, 2)]))
            .expect("straddles");
        assert_eq!(
            book.apply(&update(0, 1, None, vec![])),
            Err(SequenceError::Gap {
                expected: 1,
                found: 0
            })
        );
    }

    #[test]
    fn updates_without_a_snapshot_are_refused() {
        let mut book = Book::new();
        assert_eq!(
            book.apply(&update(1, 2, None, vec![])),
            Err(SequenceError::NoSnapshot)
        );
    }

    #[test]
    fn a_crossed_book_is_detectable() {
        let mut book = Book::new();
        book.install_snapshot(1, &[level(100, 1)], &[level(101, 1)]);
        assert!(!book.is_crossed());
        assert!(book.is_ready());

        // An ask below the best bid: only possible through a
        // reconstruction error, and worth catching as one.
        book.install_snapshot(2, &[level(100, 1)], &[level(99, 1)]);
        assert!(book.is_crossed());
    }
}
