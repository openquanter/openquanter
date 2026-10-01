//! Remembering which trades were booked, for as long as it matters.
//!
//! Both sets of books deduplicate fills by the venue's trade id, and
//! until this module both kept every id they had ever seen. The set
//! holds this process's own fills, so it grows with the number of
//! trades the process makes rather than with the market — slowly, but
//! without end, in a process whose whole point is to keep running.
//!
//! # The key stays composite
//!
//! A trade id alone does not identify a fill. When two systems on one
//! account trade against each other, both sides of the match carry the
//! same trade id, and a set keyed on the id alone books the first side
//! and discards the second as a redelivery. So each caller keeps its
//! own second component — the side, or the client id — and this window
//! bounds the set without touching what an entry means. A bitmap over
//! trade ids would have been smaller and would have reintroduced that
//! defect.
//!
//! # Bounded by trade id, not by count or by time
//!
//! The venues with a user stream — Binance, Aster and OKX — number
//! trades per symbol with an integer that only increases. So "old" can
//! be said in the venue's own units: an entry whose id is more than
//! [`WINDOW`] below the highest id seen is old. Counting entries would
//! forget a recent trade during a burst; a wall-clock age would need a
//! clock this module does not otherwise read, and the event core's rule
//! is that time arrives as an event.
//!
//! # What falls out of the window is refused, not forgotten
//!
//! Once an entry is pruned, a redelivery of it can no longer be told
//! from a first delivery. Booking it would double a position on the one
//! path that is supposed to make doubling impossible, so an id below
//! the floor is answered [`Seen::Stale`] and not booked — whether or not
//! it was ever in the set. The floor only rises, and pruning removes
//! exactly the entries below it, so nothing that was booked can come
//! back as new.
//!
//! The cost is a genuine fill delivered for the first time more than a
//! window late. A stream redelivers after a reconnect, minutes or hours
//! later, not weeks; and if it ever did happen, the books would be
//! smaller than the account, which reconciliation is there to catch.
//! The opposite failure — a stale copy booked twice — reconciliation
//! would catch too, but only after the strategy had traded against a
//! position that never existed.

use std::collections::HashSet;
use std::hash::Hash;

/// How far below the highest trade id an entry is still remembered,
/// in trade ids.
///
/// The busiest symbol measured, BTCUSDT perpetual on Binance, did
/// 3,602,361 trades on 2026-09-01 (README, capture against Tardis). A
/// window of fifty million is about two weeks of that symbol and longer
/// for every quieter one — far beyond any redelivery a reconnect
/// produces. A million, the obvious round number, would have been
/// about seven hours.
pub(crate) const WINDOW: u64 = 50_000_000;

/// Size below which the set is never pruned. Pruning a small set saves
/// nothing and costs a pass over it.
const MIN_PRUNE_AT: usize = 4_096;

/// What a trade id and its second component amount to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// Not seen before, and recent enough to be judged.
    New,
    /// Already booked.
    Duplicate,
    /// Below the window's floor: whether it was booked can no longer be
    /// known, so it is treated as booked.
    Stale,
}

/// A set of `(trade id, discriminator)` pairs that forgets ids far below
/// the highest it has seen. One window per trade id space, which on
/// every venue with a user stream means one per symbol.
#[derive(Debug)]
pub(crate) struct TradeWindow<D> {
    seen: HashSet<(u64, D)>,
    /// Highest trade id inserted. The floor is derived from it.
    highest: u64,
    /// Width in trade ids. [`WINDOW`] except in tests.
    width: u64,
    /// Size past which the next insert prunes. Twice the size left by
    /// the last prune, so each pass over the set is paid for by as many
    /// inserts as it examines: amortised constant time per fill.
    prune_at: usize,
}

impl<D> Default for TradeWindow<D> {
    fn default() -> Self {
        Self::with_width(WINDOW)
    }
}

impl<D> TradeWindow<D> {
    pub(crate) fn with_width(width: u64) -> Self {
        Self {
            seen: HashSet::new(),
            highest: 0,
            width,
            prune_at: MIN_PRUNE_AT,
        }
    }

    /// The lowest trade id still judged. Anything below is stale.
    fn floor(&self) -> u64 {
        self.highest.saturating_sub(self.width)
    }

    /// Entries held. Bounded by the trades inside one window, not by the
    /// trades in the run.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.seen.len()
    }
}

impl<D: Eq + Hash> TradeWindow<D> {
    /// Whether this pair is already accounted for: booked, or too old to
    /// tell, which is answered the same way for the reason in the module
    /// docs.
    pub(crate) fn contains(&self, trade: u64, d: D) -> bool {
        trade < self.floor() || self.seen.contains(&(trade, d))
    }

    /// Record a pair, and say whether it was new.
    ///
    /// Only [`Seen::New`] changes the set.
    pub(crate) fn insert(&mut self, trade: u64, d: D) -> Seen {
        if trade < self.floor() {
            return Seen::Stale;
        }
        if !self.seen.insert((trade, d)) {
            return Seen::Duplicate;
        }
        self.highest = self.highest.max(trade);
        if self.seen.len() > self.prune_at {
            let floor = self.floor();
            self.seen.retain(|(t, _)| *t >= floor);
            self.prune_at = (self.seen.len() * 2).max(MIN_PRUNE_AT);
        }
        Seen::New
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both sides of a self-trade share one id; pruning must keep both,
    /// and must keep refusing a redelivery of either.
    #[test]
    fn pruning_keeps_both_sides_of_a_match_inside_the_window() {
        let mut w = TradeWindow::with_width(1_000_000);
        assert_eq!(w.insert(500, 'b'), Seen::New);
        assert_eq!(w.insert(500, 's'), Seen::New, "the other side of the match");
        // Enough later trades to force several prunes, all within a
        // window of id 500.
        for t in 1_000..(1_000 + 3 * MIN_PRUNE_AT as u64) {
            w.insert(t, 'b');
        }
        assert!(w.prune_at > MIN_PRUNE_AT, "a prune actually ran");
        assert!(w.contains(500, 'b') && w.contains(500, 's'));
        assert_eq!(w.insert(500, 'b'), Seen::Duplicate);
        assert_eq!(w.insert(500, 's'), Seen::Duplicate);
    }

    #[test]
    fn an_id_below_the_floor_is_stale_and_never_new() {
        let mut w = TradeWindow::with_width(100);
        assert_eq!(w.insert(10, 'b'), Seen::New);
        assert_eq!(w.insert(1_000, 'b'), Seen::New);
        // Never seen, and still refused: once the window has moved past
        // it there is no way to know it was not seen.
        assert_eq!(w.insert(5, 's'), Seen::Stale);
        assert_eq!(w.insert(10, 'b'), Seen::Stale);
        assert!(w.contains(5, 's'), "answered as accounted for");
        // The floor itself is still inside.
        assert_eq!(w.insert(900, 's'), Seen::New);
    }

    #[test]
    fn a_redelivery_inside_the_window_is_a_duplicate() {
        let mut w = TradeWindow::default();
        assert_eq!(w.insert(7, 'b'), Seen::New);
        assert_eq!(w.insert(WINDOW, 'b'), Seen::New);
        assert_eq!(w.insert(7, 'b'), Seen::Duplicate, "7 is still in");
    }

    /// The defect: a long run's set grew with every fill. Five million
    /// of this process's fills, one in every thousand of the symbol's
    /// trades, span five billion ids, far more than one window.
    #[test]
    fn the_set_stays_bounded_over_a_long_run() {
        let mut w = TradeWindow::default();
        let step = 1_000;
        let per_window = (WINDOW / step) as usize;
        let mut largest = 0;
        for i in 1..=5_000_000_u64 {
            assert_eq!(w.insert(i * step, 'b'), Seen::New);
            largest = largest.max(w.len());
        }
        assert!(
            largest <= 2 * per_window + MIN_PRUNE_AT + 1,
            "held {largest} entries for a window of {per_window}"
        );
    }
}
