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

pub mod agg;
pub use agg::{Aggregator, Counts};

use oq_engine::Tick;
use oq_l2feed::depth::Scales;
use oq_l2feed::frame::{Kind, Record};
use oq_l2feed::manifest::is_gap;
use oq_l2feed::venue::Venue;

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
    /// Windows with no trade, carrying the previous price forward.
    pub quiet_windows: u64,
    /// Windows that closed before this symbol had ever traded.
    ///
    /// These publish nothing: there is no price to carry, and a tick
    /// whose price is zero becomes a mark price of zero in the kernel.
    pub windows_before_first_trade: u64,
    /// Events whose exchange timestamp went backwards.
    ///
    /// Large here means the streams are reordering against each other,
    /// which is worth knowing before the numbers are believed.
    ///
    /// Always zero on the conversion path, and that is a fact rather
    /// than an omission: [`to_ticks`] sorts its events before folding
    /// them. It is copied rather than left unset, so that if the sort
    /// ever goes away the counter starts reporting instead of going on
    /// reading zero.
    pub out_of_order: u64,
}

impl Report {
    /// Take the aggregator's counters, and the tick count, as one act.
    ///
    /// Two callers reach this: [`to_ticks`], and the binary, which folds
    /// an hour at a time and so cannot use it. They copied the fields
    /// one by one in two places, and the second time a counter was added
    /// the binary's copy was missed — so `oq-ingest` printed nothing for
    /// it, and would have gone on printing nothing, because **a field
    /// that is never assigned reads zero rather than failing**.
    ///
    /// A new counter is now copied in exactly one place.
    pub fn absorb(&mut self, counts: crate::agg::Counts, ticks: usize) {
        self.depth_applied = counts.depth_applied;
        self.trades = counts.trades;
        self.quiet_windows = counts.quiet_windows;
        self.windows_before_first_trade = counts.windows_before_first_trade;
        self.out_of_order = counts.out_of_order;
        self.ticks = ticks as u64;
    }
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
    let mut agg = Aggregator::new(window_ns)?;
    let mut report = Report::default();
    let mut ticks = fold_into(venue, sources, scales, &mut agg, &mut report);
    ticks.extend(agg.flush());
    report.absorb(agg.counts(), ticks.len());
    Ok((ticks, report))
}

/// Fold one batch of sources into an aggregator that outlives the call.
///
/// The batch exists because memory does. A day of one instrument's depth
/// is millions of records and the parsed form is larger than the bytes on
/// disk; loading a whole day at once cost more than the machine holding
/// the data had — measured on the capture host as a process killed by the
/// kernel after it had reported 2,114,759 depth records, on 1 GiB of RAM.
///
/// The archive is already written one file per hour, so an hour is the
/// batch the data offers. Carrying the aggregator across batches is what
/// makes that safe: the order book, the cumulative volume and the open
/// window are state that spans hours, and per-hour calls that each
/// started fresh would report an unknown quote at the top of every hour
/// and restart the volume counter twenty-four times a day.
///
/// Ordering inside the batch is by exchange time, as before. Across
/// batches it comes from the archive's own layout, which files a record
/// under the event time it carries — so an hour's records belong to that
/// hour and the boundary needs no reordering.
pub fn fold_into(
    venue: &dyn Venue,
    sources: &[Source<'_>],
    scales: Scales,
    agg: &mut Aggregator,
    report: &mut Report,
) -> Vec<Tick> {
    let mut events: Vec<Event> = Vec::new();

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
    for event in &events {
        let closed = match &event.kind {
            EventKind::Gap => agg.on_gap(event.at, event.local),
            EventKind::Depth(update) => agg.on_depth(event.at, event.local, update),
            EventKind::Trade(t) => agg.on_trade(event.at, event.local, t),
        };
        ticks.extend(closed);
    }
    ticks
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

#[cfg(test)]
mod tests;
