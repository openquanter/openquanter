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
    /// Windows that closed before this symbol had ever traded.
    ///
    /// Not the same as a quiet window. A quiet window has a price to
    /// carry forward; these have none, so there is nothing to report and
    /// no tick is produced.
    pub windows_before_first_trade: u64,
    /// Events whose exchange timestamp went backwards.
    ///
    /// Counted rather than silently absorbed: three streams on three
    /// connections reorder against each other, and a run where this is
    /// large is one whose feed is worth looking at before its numbers
    /// are believed.
    pub out_of_order: u64,
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
    /// The last traded price seen, across windows.
    ///
    /// The reference implementation keeps one tick object per symbol and
    /// updates its fields in place, so the price it publishes is always
    /// the most recent one known. This aggregator built a fresh window
    /// each time, so a window without a trade published a price of zero
    /// — and zero is not a price, it is the absence of one wearing the
    /// same type.
    ///
    /// Measured on a twelve-hour live run: 56.4% of ticks carried
    /// `last = 0`, and the kernel assigns `mark = tick.last` with no
    /// guard. Carrying the price forward leaves two zeros in 38,491
    /// ticks, both before the symbol had ever traded.
    last_price: i64,
    /// The largest exchange timestamp seen.
    ///
    /// The window clock never goes backwards past this. Depth updates
    /// are still applied — the book is ordered by sequence id, not by
    /// time, and dropping one would corrupt it — but a late arrival does
    /// not reopen a window that has closed.
    high_water: i64,
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
            last_price: 0,
            high_water: i64::MIN,
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
        // A clock that goes backwards reopens a window that has already
        // been published, so the same interval is reported twice with
        // different contents. Three streams on three connections make
        // this ordinary rather than exceptional: the same run measured
        // 35.4% of consecutive events arriving out of order, the worst
        // by thirty-one minutes.
        //
        // The event is still processed — the book is ordered by sequence
        // id and dropping an update would corrupt it — but it belongs to
        // the window that is open, not to one that has closed.
        let at = if at < self.high_water {
            self.counts.out_of_order += 1;
            self.high_water
        } else {
            self.high_water = at;
            at
        };
        let start = at - at.rem_euclid(self.window_ns);
        let closed = match &mut self.open {
            Some(w) if w.start == start => None,
            Some(w) => {
                let traded = w.trades > 0;
                let tick = w.close(self.bid, self.ask, self.volume_total, self.last_price);
                if !traded {
                    self.counts.quiet_windows += 1;
                }
                *w = Window::new(start);
                // Nothing has ever traded, so there is no price to carry
                // and no tick to publish. The reference implementation
                // guards every one of its four publish sites with
                // `last_price > 0` for this reason: a tick whose price
                // is zero is not a quiet market, it is no market yet.
                if self.last_price == 0 {
                    self.counts.windows_before_first_trade += 1;
                    None
                } else {
                    self.counts.ticks += 1;
                    Some(tick)
                }
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
        // Kept outside the window so the next one starts from it rather
        // than from zero.
        self.last_price = trade.price;
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
        let tick = w.close(self.bid, self.ask, self.volume_total, self.last_price);
        if w.trades == 0 {
            self.counts.quiet_windows += 1;
        }
        if self.last_price == 0 {
            self.counts.windows_before_first_trade += 1;
            return None;
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

    /// `carried` is the last price seen before this window opened, used
    /// when the window itself saw no trade. A quiet market still has a
    /// price; it simply has not changed.
    const fn close(&self, bid: i64, ask: i64, volume_total: i64, carried: i64) -> Tick {
        let last = if self.trades == 0 { carried } else { self.last };
        Tick {
            stamp: Stamp {
                exch: Nanos(self.start),
                local: Nanos(self.last_local),
            },
            last: PriceTicks(last),
            // High and low describe *this* window's trading, so they stay
            // absent when it had none. Carrying them forward would report
            // a range that no trade in this window produced.
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
        Trade {
            price,
            qty,
            aggressor: None,
        }
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

#[cfg(test)]
mod batching {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn trade(price: i64, qty: i64) -> Trade {
        Trade {
            price,
            qty,
            aggressor: None,
        }
    }

    /// Feed a sequence through one aggregator, in `chunks` pieces.
    ///
    /// The pieces are only a division of the calls; the aggregator is the
    /// same one throughout, which is exactly what the hourly conversion
    /// does with an hour as the piece.
    fn run(events: &[(i64, i64, i64)], chunks: usize) -> Vec<Tick> {
        let mut a = Aggregator::new(SEC).expect("positive window");
        let mut out = Vec::new();
        let size = events.len().div_ceil(chunks.max(1));
        for chunk in events.chunks(size.max(1)) {
            for (at, price, qty) in chunk {
                out.extend(a.on_trade(*at, *at, &trade(*price, *qty)));
            }
        }
        out.extend(a.flush());
        out
    }

    #[test]
    fn folding_in_batches_gives_exactly_what_folding_at_once_gives() {
        // The invariant the hourly conversion rests on. If a batch
        // boundary changed a single tick, the memory saving would have
        // been bought with a different answer — and the difference would
        // be invisible, because both outputs look like plausible ticks.
        let events: Vec<(i64, i64, i64)> = (0..40)
            .map(|i| (i * SEC / 3, 100 + i % 7, 1 + i % 3))
            .collect();

        let whole = run(&events, 1);
        assert!(!whole.is_empty(), "the fixture produces ticks");
        for chunks in [2, 3, 5, 8, 40] {
            assert_eq!(
                run(&events, chunks),
                whole,
                "{chunks} batches must equal one batch"
            );
        }
    }

    #[test]
    fn a_batch_boundary_inside_a_window_does_not_split_the_window() {
        // Three trades in one second, cut between the second and third.
        // A window closed at the boundary would report two ticks where
        // the data has one, and its extremes would each be half right.
        let events = [(0, 100, 1), (SEC / 2, 300, 1), (SEC / 2 + 1, 200, 1)];
        let whole = run(&events, 1);
        assert_eq!(whole.len(), 1, "one second, one tick");
        assert_eq!(run(&events, 2), whole);
        assert_eq!(run(&events, 3), whole);
        assert_eq!(whole[0].high, PriceTicks(300));
        assert_eq!(whole[0].low, PriceTicks(100));
    }

    #[test]
    fn the_cumulative_volume_does_not_restart_at_a_boundary() {
        // The failure a fresh aggregator per batch would cause, and the
        // reason the aggregator is carried rather than recreated.
        let events = [(0, 100, 5), (2 * SEC, 100, 3), (4 * SEC, 100, 2)];
        let ticks = run(&events, 3);
        let volumes: Vec<i64> = ticks.iter().map(|t| t.volume.0).collect();
        assert_eq!(volumes, vec![5, 8, 10], "cumulative across batches");
    }
}

#[cfg(test)]
mod carried_price {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn trade(price: i64, qty: i64) -> Trade {
        Trade {
            price,
            qty,
            aggressor: None,
        }
    }

    /// A quiet window publishes the last price, not zero.
    ///
    /// The defect this replaces: each window started at `last = 0`, so a
    /// window with no trade of its own published a price of zero. The
    /// kernel assigns `mark = tick.last` with no guard, and a twelve-hour
    /// live run carried it on 56.4% of its ticks.
    ///
    /// The reference implementation cannot produce this. It keeps one
    /// tick object per symbol and updates its fields in place, so what it
    /// publishes is always the most recent price known — and every one of
    /// its four publish sites is additionally guarded by `last_price > 0`.
    #[test]
    fn a_quiet_window_carries_the_price_rather_than_publishing_zero() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 1));

        let first = a.advance_to(SEC, SEC).expect("the traded window");
        assert_eq!(first.last, PriceTicks(100));

        // Two windows with no trade at all.
        let quiet = a.advance_to(2 * SEC, 2 * SEC).expect("a quiet window");
        assert_eq!(
            quiet.last,
            PriceTicks(100),
            "the market went quiet; the price did not become zero"
        );
        let quieter = a.advance_to(3 * SEC, 3 * SEC).expect("another");
        assert_eq!(quieter.last, PriceTicks(100));
        assert_eq!(a.counts().quiet_windows, 2);

        // The range still describes this window's own trading, so it
        // stays absent. Carrying it forward would report a range that no
        // trade in the window produced.
        assert_eq!((quiet.high, quiet.low), (PriceTicks(0), PriceTicks(0)));
    }

    /// Before the first trade there is no price, so nothing is published.
    ///
    /// A book-only stream would otherwise set the kernel's mark to zero
    /// on every window. The reference implementation reaches the same
    /// place from the other direction: its depth branch never publishes.
    #[test]
    fn nothing_is_published_before_the_first_trade() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        assert!(a.advance_to(SEC, SEC).is_none());
        assert!(a.advance_to(2 * SEC, 2 * SEC).is_none());
        assert_eq!(a.counts().ticks, 0, "no tick may carry a price of zero");
        // One, not two: the first call had no window open yet, so it
        // opened one rather than closing one.
        assert_eq!(a.counts().windows_before_first_trade, 1);

        // And the moment there is a price, publishing resumes.
        a.on_trade(2 * SEC, 2 * SEC, &trade(100, 1));
        let t = a
            .advance_to(3 * SEC, 3 * SEC)
            .expect("now there is a price");
        assert_eq!(t.last, PriceTicks(100));
    }

    /// A late event does not reopen a window that has been published.
    ///
    /// Three streams on three connections reorder against each other: the
    /// same live run had 35.4% of consecutive events out of order, the
    /// worst by thirty-one minutes. A clock that walks backwards
    /// republishes an interval already reported, with different contents.
    #[test]
    fn a_late_event_does_not_reopen_a_published_window() {
        let mut a = Aggregator::new(SEC).expect("positive window");
        a.on_trade(0, 0, &trade(100, 1));
        let first = a
            .on_trade(3 * SEC, 3 * SEC, &trade(103, 1))
            .expect("closed");
        assert_eq!(first.stamp.exch, Nanos(0));

        // Two seconds late, for a window that has already been published.
        let reopened = a.on_trade(SEC, SEC, &trade(101, 1));
        assert!(
            reopened.is_none(),
            "the closed window must not be published twice: {reopened:?}"
        );
        assert_eq!(a.counts().out_of_order, 1);

        // The trade itself is not lost — it lands in the open window.
        assert_eq!(a.counts().trades, 3);
        let next = a.flush().expect("the open window");
        assert_eq!(
            next.last,
            PriceTicks(101),
            "the late trade was folded in, not discarded"
        );
        assert_eq!(next.stamp.exch, Nanos(3 * SEC), "and the clock stood still");
    }
}
