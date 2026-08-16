//! Turn captured venue archives into the tick format the engine replays.
//!
//! Capture writes what the venue said, byte for byte. The engine reads
//! [`Tick`]s: one aggregated observation per window, with trade extremes
//! and top of book. This crate is the only place those two meet, and it
//! is a separate crate so that they meet in exactly one place — folding
//! it into `oq-data` would drag a websocket client and its dependency
//! tree into the engine, which is the thing the engine is careful not to
//! have.
//!
//! Conversion loses information on purpose. A window of L2 depth becomes
//! a best bid and a best ask; the book behind them is dropped. That is
//! the right trade only because the archive is kept: the raw capture is
//! the record, this is a projection of it for the strategies whose
//! decisions a projection can carry. Strategies that need the book need
//! a higher rung of the fidelity ladder, not a richer tick.
//!
//! # Two conventions that are easy to get backwards
//!
//! **Extremes belong to their own window.** `high` and `low` are the
//! highest and lowest trades *inside* this window, never a running
//! maximum carried forward. The engine's own documentation records what
//! the other choice cost: a feed supplying running extremes was replayed
//! and a take-profit filled 1506 points away from the market, because a
//! running maximum in a falling market keeps the number it set minutes
//! ago. Every later decision descended from that fill.
//!
//! **Volume accumulates.** Consumers read volume by differencing
//! consecutive ticks, so this emits a running total rather than a
//! per-window amount. Emitting per-window volume would look more natural
//! and would silently halve every difference.

use oq_engine::Tick;
use oq_l2feed::book::Book;
use oq_l2feed::depth::Scales;
use oq_l2feed::frame::{Kind, Record};
use oq_l2feed::manifest::is_gap;
use oq_l2feed::venue::Venue;
use oq_types::{Nanos, PriceTicks, QtyLots, Stamp};

/// How the conversion went, so a caller can tell a thin market from a
/// broken read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Windows emitted.
    pub ticks: u64,
    /// Depth updates applied to the book.
    pub depth_applied: u64,
    /// Trades folded into extremes and volume.
    pub trades: u64,
    /// Payloads this build could not read.
    pub unparseable: u64,
    /// Gap markers seen. The book is dropped at each one.
    pub gaps: u64,
    /// Windows with no trade, carrying only book state.
    pub quiet_windows: u64,
}

/// One source file, and what kind of records it holds.
pub struct Source<'a> {
    /// Records decoded from a capture file.
    pub records: &'a [Record],
    /// Which stream they came from, as named in the archive path.
    pub stream: &'a str,
}

/// Fold captured records into ticks of `window_ns`.
///
/// The venue supplies the parsing, because payload shapes have nothing
/// in common between venues: one sends `"p"` and `"q"` at the top level,
/// another `"px"` and `"sz"` nested under `"data"`. A reader written for
/// either finds nothing in the other, which is not an error — just an
/// empty result, so the conversion yields no ticks and reports an empty
/// archive.
///
/// Depth and trade records for one instrument are supplied together;
/// each contributes what only it knows. Depth alone yields ticks with
/// book state and no trades, which is a legitimate and visibly
/// incomplete answer rather than a silent one — `Report::trades` will be
/// zero.
///
/// # Errors
///
/// Never fails on unreadable payloads; they are counted in the report so
/// that a partial read is visible rather than fatal. Returns `Err` only
/// for a non-positive window, which has no meaning to fall back on.
pub fn to_ticks(
    venue: &dyn Venue,
    sources: &[Source<'_>],
    scales: Scales,
    window_ns: i64,
) -> Result<(Vec<Tick>, Report), String> {
    if window_ns <= 0 {
        return Err("window must be positive".to_string());
    }

    let mut events: Vec<Event> = Vec::new();
    let mut report = Report::default();

    for source in sources {
        for record in source.records {
            if record.kind == Kind::Control {
                if is_gap(record) {
                    report.gaps += 1;
                    events.push(Event {
                        at: record.day_ts(),
                        local: record.local_ts,
                        kind: EventKind::Gap,
                    });
                }
                continue;
            }
            match source.stream {
                "depth" => match venue.parse_depth(&record.payload, scales) {
                    Ok(update) => events.push(Event {
                        at: record.day_ts(),
                        local: record.local_ts,
                        kind: EventKind::Depth(Box::new(update)),
                    }),
                    Err(_) => report.unparseable += 1,
                },
                "trade" => match venue.parse_trade(&record.payload, scales) {
                    Some(t) => events.push(Event {
                        at: record.day_ts(),
                        local: record.local_ts,
                        kind: EventKind::Trade(t),
                    }),
                    None => report.unparseable += 1,
                },
                _ => {}
            }
        }
    }

    // Two streams recorded by two sockets arrive interleaved only by
    // accident. Ordering by exchange time puts them back into the order
    // the venue produced them, which is the order the book and the
    // extremes both assume.
    events.sort_by_key(|e| (e.at, e.local));

    let mut ticks = Vec::new();
    let mut book = Book::new();
    let mut bootstrapped = false;
    let mut volume_total: i64 = 0;
    let mut bid: i64 = 0;
    let mut ask: i64 = 0;
    let mut open: Option<Window> = None;

    for event in &events {
        let start = event.at - event.at.rem_euclid(window_ns);
        match &mut open {
            Some(w) if w.start == start => {}
            Some(w) => {
                ticks.push(w.close(bid, ask, volume_total));
                if w.trades == 0 {
                    report.quiet_windows += 1;
                }
                *w = Window::new(start);
            }
            None => open = Some(Window::new(start)),
        }
        let window = open.as_mut().expect("a window is open");
        window.last_local = event.local;

        match &event.kind {
            EventKind::Gap => {
                // The capture declared that it stopped listening, so the
                // book cannot span this. Dropping it means the next
                // windows carry no top of book until a fresh update
                // rebuilds one, which is the honest answer: a stale best
                // bid is worse than an absent one, because the engine
                // treats zero as "unknown" and falls back to trades.
                book = Book::new();
                bootstrapped = false;
                // A dropped book has no quote to report, and reporting
                // the one from before the gap would be worse than
                // reporting none.
                bid = 0;
                ask = 0;
            }
            EventKind::Depth(update) => {
                if !bootstrapped {
                    book.install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
                    bootstrapped = true;
                }
                if book.apply(update).is_ok() {
                    report.depth_applied += 1;
                } else {
                    // A break the capture did not declare. Resynchronise
                    // the way a live consumer would rather than carrying
                    // a book that is now wrong.
                    book = Book::new();
                    book.install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
                    let _ = book.apply(update);
                    report.depth_applied += 1;
                }
                bid = book.bids().best().map_or(0, |l| l.price);
                ask = book.asks().best().map_or(0, |l| l.price);
            }
            EventKind::Trade(t) => {
                report.trades += 1;
                window.observe_trade(t.price);
                volume_total = volume_total.saturating_add(t.qty);
            }
        }
    }

    if let Some(w) = open {
        ticks.push(w.close(bid, ask, volume_total));
        if w.trades == 0 {
            report.quiet_windows += 1;
        }
    }

    report.ticks = ticks.len() as u64;
    Ok((ticks, report))
}

struct Event {
    at: i64,
    local: i64,
    kind: EventKind,
}

enum EventKind {
    Depth(Box<oq_l2feed::depth::DepthUpdate>),
    Trade(oq_l2feed::venue::Trade),
    Gap,
}

struct Window {
    start: i64,
    last_local: i64,
    last: i64,
    high: i64,
    low: i64,
    trades: u64,
}

impl Window {
    fn new(start: i64) -> Self {
        Self {
            start,
            last_local: start,
            last: 0,
            high: 0,
            low: 0,
            trades: 0,
        }
    }

    /// Fold a trade into this window's extremes.
    ///
    /// `low` starts at zero meaning "unset" rather than "zero price", so
    /// the first trade seeds it instead of losing to it.
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

    /// Close the window, reading top of book at the moment it ends.
    ///
    /// The book is passed in rather than accumulated in the window
    /// because it is not a property of the window: it is state that
    /// persists across them. Recording it only when a depth update
    /// happened to land inside a window reported `bid = ask = 0` for
    /// every other one, and the engine reads zero as "unknown" and falls
    /// back to trade prices — so a window with trades and no depth
    /// update quietly lost the quote it could have had.
    fn close(&self, bid: i64, ask: i64, volume_total: i64) -> Tick {
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
mod tests;
