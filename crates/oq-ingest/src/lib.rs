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

    // One aggregator, and the live feed uses the same one. Two
    // implementations of this would agree until they did not, and the
    // disagreement would be invisible: both sides produce things that
    // look like plausible ticks.
    let mut agg = Aggregator::new(window_ns)?;
    let mut ticks = Vec::new();

    for event in &events {
        let closed = match &event.kind {
            EventKind::Gap => agg.on_gap(event.at, event.local),
            EventKind::Depth(update) => agg.on_depth(event.at, event.local, update),
            EventKind::Trade(t) => agg.on_trade(event.at, event.local, t),
        };
        ticks.extend(closed);
    }
    ticks.extend(agg.flush());

    let counts = agg.counts();
    report.depth_applied = counts.depth_applied;
    report.trades = counts.trades;
    report.quiet_windows = counts.quiet_windows;
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

#[cfg(test)]
mod tests;
