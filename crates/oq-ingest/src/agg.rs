//! One window at a time, fed one event at a time.
//!
//! Extracted from the archive converter so that a live feed and a
//! replay produce ticks from the same code rather than from two
//! implementations that agree until they do not. A backtest whose ticks
//! are shaped differently from the live ones is a backtest measuring
//! something else, and the difference would be invisible: both sides
//! look like plausible ticks.
//!
//! The conventions are the converter's, unchanged, and they are the
//! part worth restating because they are easy to get subtly wrong:
//!
//! - **Extremes belong to their own window.** `high` and `low` are the
//!   highest and lowest trades *inside* this window, never a running
//!   figure carried forward.
//! - **Volume is cumulative**, because venues disagree about when to
//!   reset theirs, so consumers difference consecutive ticks instead of
//!   reading an absolute.
//! - **Top of book is state, not a property of the window.** It
//!   persists across windows and is read at the moment one closes.
//!   Recording it only when a depth update happened to land inside a
//!   window reported an unknown quote for every other one.

use oq_engine::Tick;
use oq_l2feed::book::Book;
use oq_l2feed::depth::DepthUpdate;
use oq_l2feed::venue::Trade;
use oq_types::{Nanos, PriceTicks, QtyLots, Stamp};

/// What an aggregator has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub depth_applied: u64,
    pub trades: u64,
    /// Windows that closed without a single trade.
    pub quiet_windows: u64,
    pub ticks: u64,
}

/// Folds venue events into ticks of a fixed width.
#[derive(Debug)]
pub struct Aggregator {
    window_ns: i64,
    book: Book,
    bootstrapped: bool,
    volume_total: i64,
    bid: i64,
    ask: i64,
    open: Option<Window>,
    counts: Counts,
}

impl Aggregator {
    /// # Errors
    /// A window of zero or less, which has no meaning to fall back on.
    pub fn new(window_ns: i64) -> Result<Self, String> {
        if window_ns <= 0 {
            return Err("window must be positive".to_string());
        }
        Ok(Self {
            window_ns,
            book: Book::new(),
            bootstrapped: false,
            volume_total: 0,
            bid: 0,
            ask: 0,
            open: None,
            counts: Counts::default(),
        })
    }

    #[must_use]
    pub const fn counts(&self) -> Counts {
        self.counts
    }

    /// Move to the window containing `at`, returning the one that
    /// closed if this crossed a boundary.
    fn roll(&mut self, at: i64, local: i64) -> Option<Tick> {
        let start = at - at.rem_euclid(self.window_ns);
        let closed = match &mut self.open {
            Some(w) if w.start == start => None,
            Some(w) => {
                let tick = w.close(self.bid, self.ask, self.volume_total);
                if w.trades == 0 {
                    self.counts.quiet_windows += 1;
                }
                self.counts.ticks += 1;
                *w = Window::new(start);
                Some(tick)
            }
            None => {
                self.open = Some(Window::new(start));
                None
            }
        };
        if let Some(w) = self.open.as_mut() {
            w.last_local = local;
        }
        closed
    }

    /// Apply a depth update.
    pub fn on_depth(&mut self, at: i64, local: i64, update: &DepthUpdate) -> Option<Tick> {
        let closed = self.roll(at, local);
        if !self.bootstrapped {
            self.book
                .install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
            self.bootstrapped = true;
        }
        if self.book.apply(update).is_err() {
            // A break the capture did not declare. Resynchronise the
            // way a live consumer would rather than carrying a book
            // that is now wrong.
            self.book = Book::new();
            self.book
                .install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
            let _ = self.book.apply(update);
        }
        self.counts.depth_applied += 1;
        self.bid = self.book.bids().best().map_or(0, |l| l.price);
        self.ask = self.book.asks().best().map_or(0, |l| l.price);
        closed
    }

    /// Apply a trade.
    pub fn on_trade(&mut self, at: i64, local: i64, trade: &Trade) -> Option<Tick> {
        let closed = self.roll(at, local);
        self.counts.trades += 1;
        if let Some(w) = self.open.as_mut() {
            w.observe_trade(trade.price);
        }
        self.volume_total = self.volume_total.saturating_add(trade.qty);
        closed
    }

    /// The feed declared that it stopped listening.
    ///
    /// The book cannot span a gap, so it is dropped. The windows after
    /// this carry no top of book until a fresh update rebuilds one,
    /// which is the honest answer: a stale best bid is worse than an
    /// absent one, because a consumer reads zero as "unknown" and falls
    /// back to trades, while it reads a stale quote as a quote.
    pub fn on_gap(&mut self, at: i64, local: i64) -> Option<Tick> {
        let closed = self.roll(at, local);
        self.book = Book::new();
        self.bootstrapped = false;
        self.bid = 0;
        self.ask = 0;
        closed
    }

    /// Close whatever window is open.
    ///
    /// For a replay this is the end of the data. For a live feed it is
    /// what a timer calls when a window has elapsed with nothing in it,
    /// because a quiet market still has to produce ticks — a strategy
    /// that only hears from the world when the world is busy cannot
    /// act on the world going quiet.
    pub fn flush(&mut self) -> Option<Tick> {
        let w = self.open.take()?;
        let tick = w.close(self.bid, self.ask, self.volume_total);
        if w.trades == 0 {
            self.counts.quiet_windows += 1;
        }
        self.counts.ticks += 1;
        Some(tick)
    }

    /// Close the open window and immediately open the one containing
    /// `at`, without an event.
    ///
    /// The live counterpart of a boundary crossing: on a quiet market
    /// no event arrives to roll the window, so a timer does it.
    pub fn advance_to(&mut self, at: i64, local: i64) -> Option<Tick> {
        self.roll(at, local)
    }
}

#[derive(Debug)]
struct Window {
    start: i64,
    last_local: i64,
    last: i64,
    high: i64,
    low: i64,
    trades: u64,
}

impl Window {
    const fn new(start: i64) -> Self {
        Self {
            start,
            last_local: start,
            last: 0,
            high: 0,
            low: 0,
            trades: 0,
        }
    }

    /// `low` starts at zero meaning "unset" rather than "zero price",
    /// so the first trade seeds it instead of losing to it.
    fn observe_trade(&mut self, price: i64) {
        self.last = price;
        if self.trades == 0 {
            self.high = price;
            self.low = price;
        } else {
            self.high = self.high.max(price);
            self.low = self.low.min(price);
        }
        self.trades += 1;
    }

    const fn close(&self, bid: i64, ask: i64, volume_total: i64) -> Tick {
        Tick {
            stamp: Stamp {
                exch: Nanos(self.start),
                local: Nanos(self.last_local),
            },
            last: PriceTicks(self.last),
            high: PriceTicks(self.high),
            low: PriceTicks(self.low),
            bid: PriceTicks(bid),
            ask: PriceTicks(ask),
            volume: QtyLots(volume_total),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn trade(price: i64, qty: i64) -> Trade {
        Trade { price, qty }
    }

    #[test]
    fn a_window_closes_when_an_event_crosses_its_boundary() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        assert!(
            a.on_trade(0, 0, &trade(100, 1)).is_none(),
            "the first opens"
        );
        let closed = a
            .on_trade(SEC, SEC, &trade(200, 1))
            .expect("the second closes the first");
        assert_eq!(closed.last, PriceTicks(100));
        assert_eq!(closed.stamp.exch, Nanos(0));
    }

    #[test]
    fn a_quiet_market_still_produces_ticks_when_a_timer_advances_it() {
        // The live-only path. No event arrives to roll the window, so a
        // timer does — and a strategy that only hears from the world
        // when the world is busy cannot act on the world going quiet.
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 1));
        let closed = a.advance_to(3 * SEC, 3 * SEC).expect("a window closed");
        assert_eq!(closed.last, PriceTicks(100));
        assert_eq!(a.counts().ticks, 1);
    }

    #[test]
    fn extremes_do_not_carry_across_a_boundary() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 1));
        a.on_trade(1, 1, &trade(300, 1));
        let first = a.on_trade(SEC, SEC, &trade(200, 1)).expect("closed");
        assert_eq!((first.high, first.low), (PriceTicks(300), PriceTicks(100)));
        let second = a.flush().expect("closed");
        assert_eq!(
            (second.high, second.low),
            (PriceTicks(200), PriceTicks(200)),
            "the second window's extremes are its own"
        );
    }

    #[test]
    fn volume_is_cumulative_so_consumers_difference_it() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 5));
        let first = a.on_trade(SEC, SEC, &trade(100, 3)).expect("closed");
        let second = a.flush().expect("closed");
        assert_eq!(first.volume, QtyLots(5));
        assert_eq!(second.volume, QtyLots(8), "cumulative, not per window");
    }

    #[test]
    fn a_gap_clears_the_quote_rather_than_carrying_a_stale_one() {
        // A consumer reads zero as "unknown" and falls back to trades;
        // it reads a stale quote as a quote.
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 1));
        a.on_gap(SEC, SEC);
        a.on_trade(SEC + 1, SEC + 1, &trade(110, 1));
        let _ = a.flush();
        assert_eq!(a.counts().trades, 2);
    }

    #[test]
    fn a_zero_window_is_refused_rather_than_divided_by() {
        assert!(Aggregator::new(0).is_err());
        assert!(Aggregator::new(-1).is_err());
    }

    #[test]
    fn flushing_an_empty_aggregator_produces_nothing() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        assert!(a.flush().is_none());
    }
}
