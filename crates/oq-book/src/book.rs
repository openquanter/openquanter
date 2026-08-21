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

    /// What taking `qty` from this side would cost, best price first.
    ///
    /// The side is not modified: this answers a question, it does not
    /// execute anything. A caller filling several orders against one
    /// book snapshot must consume its own copy, or every order after
    /// the first is priced as though the ones before it never traded.
    ///
    /// A buy sweeps the asks and a sell sweeps the bids; which side to
    /// pass is the caller's to know, because this crate has no concept
    /// of a buy.
    #[must_use]
    pub fn sweep(&self, qty: i64) -> Sweep {
        let mut want = qty.max(0);
        let mut swept = Sweep {
            taken: 0,
            cost: 0,
            exhausted: want > 0,
        };
        for level in &self.levels {
            if want == 0 {
                swept.exhausted = false;
                break;
            }
            let take = want.min(level.qty.max(0));
            if take == 0 {
                continue;
            }
            // i128 because price times quantity leaves i64 at plausible
            // sizes: a tick price near 1e9 against a lot count near 1e10
            // is 1e19, and i64 stops at 9.2e18. An overflow here would
            // wrap into a profitable fill.
            swept.cost += i128::from(level.price) * i128::from(take);
            swept.taken += take;
            want -= take;
        }
        swept.exhausted = want > 0;
        swept
    }

    /// Take `qty` off this side, removing what was consumed.
    ///
    /// The consuming half of [`sweep`](Self::sweep), for a caller
    /// filling several orders against one snapshot of the book: each
    /// takes what the ones before it left. Pricing them all against the
    /// untouched book would say a hundred lots and ten thousand lots
    /// cost the same per lot, which is the assumption the whole tier
    /// exists to stop making.
    ///
    /// This is a caller's private copy of the venue's depth, never the
    /// venue's own. The book belongs to the feed: what it holds is what
    /// the venue displayed, and an order of ours has no business
    /// editing that.
    pub fn take(&mut self, qty: i64) -> Sweep {
        let swept = self.sweep(qty);
        let mut want = swept.taken;
        let mut drained = 0;
        for level in &mut self.levels {
            if want == 0 {
                break;
            }
            let take = want.min(level.qty.max(0));
            level.qty -= take;
            want -= take;
            // Levels are held only while they carry quantity -- `apply`
            // removes a level a zero reaches -- so the emptied ones are
            // a prefix, and the walk above starts at the front.
            if level.qty == 0 {
                drained += 1;
            }
        }
        self.levels.drain(..drained);
        swept
    }
}

/// The result of taking size off one side of a book.
///
/// Prices are not averaged here. Rounding a volume-weighted price is a
/// choice about who absorbs the fraction, and only a caller that knows
/// which way it is trading can make it against itself rather than for
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sweep {
    /// Quantity actually taken, at most the quantity asked for.
    pub taken: i64,
    /// Total price times quantity over every level touched.
    ///
    /// Not a price. Dividing by `taken` gives one, and which way to
    /// round it is the caller's to decide -- see the type's own note.
    pub cost: i128,
    /// The side ran out before the requested quantity was filled.
    ///
    /// This is not "the order would be rejected" — it is "this book
    /// cannot answer". Reconstructed depth is finite, and an order
    /// larger than every level held is exactly the order whose impact
    /// matters most. Reporting a price for it, computed from the levels
    /// that happen to be present, would be least trustworthy where it
    /// is most consequential.
    pub exhausted: bool,
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

    #[test]
    fn a_sweep_walks_levels_in_order_and_stops_when_filled() {
        let mut asks = Side::asks();
        for l in [level(102, 400), level(100, 100), level(101, 200)] {
            asks.apply(l);
        }

        // 100 at 100, 200 at 101, 200 of the 400 at 102.
        let swept = asks.sweep(500);
        assert_eq!(swept.taken, 500);
        assert_eq!(swept.cost, 100 * 100 + 200 * 101 + 200 * 102);
        assert!(!swept.exhausted);

        // The book is a question, not an execution: asking twice gives
        // the same answer.
        assert_eq!(asks.sweep(500), swept);
    }

    #[test]
    fn a_sweep_that_fits_in_the_best_level_never_touches_a_worse_price() {
        let mut asks = Side::asks();
        asks.apply(level(100, 100));
        asks.apply(level(101, 100));

        let swept = asks.sweep(60);
        assert_eq!(swept.taken, 60);
        assert_eq!(swept.cost, 60 * 100);
        assert!(!swept.exhausted);
    }

    /// Running out of book is reported, never papered over with the
    /// last price held. The quantity that could not be taken is the
    /// part whose cost this book does not know, and pricing it at the
    /// deepest level present would understate it -- which is the
    /// direction that flatters a backtest.
    #[test]
    fn a_sweep_deeper_than_the_book_says_so() {
        let mut asks = Side::asks();
        asks.apply(level(100, 100));
        asks.apply(level(101, 50));

        let swept = asks.sweep(1_000);
        assert!(swept.exhausted);
        assert_eq!(swept.taken, 150);
        assert_eq!(swept.cost, 100 * 100 + 50 * 101);
    }

    #[test]
    fn an_empty_side_takes_nothing_and_is_exhausted() {
        let swept = Side::asks().sweep(10);
        assert_eq!(swept.taken, 0);
        assert_eq!(swept.cost, 0);
        assert!(swept.exhausted);

        // Asking for nothing is answerable by an empty book: nothing
        // is what it costs, and it is not a shortfall.
        let none = Side::asks().sweep(0);
        assert_eq!(none.taken, 0);
        assert!(!none.exhausted);
    }

    #[test]
    fn bids_sweep_from_the_highest_price_down() {
        let mut bids = Side::bids();
        for l in [level(98, 100), level(100, 100), level(99, 100)] {
            bids.apply(l);
        }

        let swept = bids.sweep(150);
        assert_eq!(swept.cost, 100 * 100 + 50 * 99);
    }

    /// A tick price near 1e9 against a lot count near 1e10 overflows
    /// i64 at the second level. In i64 that wraps negative, and a
    /// negative cost is a fill that paid the taker.
    /// The point of the consuming half: two orders against one book
    /// must not both be priced as though they were first.
    #[test]
    fn taking_leaves_the_next_taker_a_worse_book() {
        let mut asks = Side::asks();
        asks.apply(level(100, 100));
        asks.apply(level(101, 100));
        asks.apply(level(102, 100));

        let first = asks.take(150);
        assert_eq!(first.cost, 100 * 100 + 50 * 101);

        // 50 left at 101, then into 102 -- not back at the touch.
        let second = asks.take(100);
        assert_eq!(second.cost, 50 * 101 + 50 * 102);

        assert_eq!(asks.depth(), 1);
        assert_eq!(asks.best(), Some(level(102, 50)));
    }

    #[test]
    fn taking_a_whole_level_removes_it() {
        let mut asks = Side::asks();
        asks.apply(level(100, 100));
        asks.apply(level(101, 100));

        asks.take(100);
        assert_eq!(asks.depth(), 1);
        assert_eq!(asks.best(), Some(level(101, 100)));
    }

    #[test]
    fn taking_more_than_the_book_holds_empties_it_and_says_so() {
        let mut asks = Side::asks();
        asks.apply(level(100, 100));

        let swept = asks.take(500);
        assert!(swept.exhausted);
        assert_eq!(swept.taken, 100);
        assert_eq!(asks.depth(), 0);

        // And an empty book still answers, rather than panicking on the
        // drain range.
        let again = asks.take(500);
        assert_eq!(again.taken, 0);
        assert!(again.exhausted);
    }

    #[test]
    fn a_sweep_large_enough_to_overflow_i64_does_not() {
        let mut asks = Side::asks();
        asks.apply(level(1_000_000_000, 10_000_000_000));
        asks.apply(level(1_000_000_001, 10_000_000_000));

        let swept = asks.sweep(20_000_000_000);
        assert!(swept.cost > i128::from(i64::MAX));
        assert_eq!(
            swept.cost,
            1_000_000_000_i128 * 10_000_000_000 + 1_000_000_001_i128 * 10_000_000_000
        );
    }
}
