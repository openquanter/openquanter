//! What a venue has to tell the capture path, and nothing more.
//!
//! The capture loop stores payloads verbatim. It never needs to
//! understand a message — only to reach the venue, subscribe, and decide
//! which file a record belongs in. That is the whole contract here, and
//! keeping it that small is deliberate: every field parsed at capture
//! time is a field that can be parsed wrong once and be wrong forever,
//! whereas a field left in the bytes can be re-read by a consumer that
//! knows better later.
//!
//! Larger systems that solved this before landed in the same place.
//! Tardis records exchange-native feeds and derives its normalised form
//! from them rather than the other way round; NautilusTrader's adapter
//! guide puts it as preserving "the wire format, not an imagined stable
//! subset", converting at one auditable boundary. Normalising during
//! capture discards fields, and capture is the one step that cannot be
//! repeated.
//!
//! What this module adds is the seam that was missing. Before it, the
//! capture binary imported `binance_perp_*` functions directly, so a
//! second venue meant editing the binary. The archive path already had a
//! venue label, but it was only a label — passing `--venue okx` filed
//! Binance data under `okx/`, which is worse than not offering the flag.
//!
//! # Adding a venue
//!
//! Implement [`Venue`], register it in [`by_id`], and nothing else in
//! the crate needs to change. [`binance`] and [`okx`] are worked
//! examples, and they were chosen to differ: one puts the subscription
//! in the URL and can only be confirmed by its first message, the other
//! sends a JSON frame and acknowledges explicitly, and their timestamps
//! are a bare integer and a quoted string respectively. An abstraction
//! that fits only one of them is not an abstraction.

pub mod binance;
pub mod binance_instruments;
pub mod okx;
pub mod okx_instruments;

use core::time::Duration;

/// A stream to subscribe to, and the name it is archived under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSpec {
    /// Name used in the archive path, e.g. `depth`.
    pub name: String,
    /// The venue's own topic or channel identifier.
    pub topic: String,
}

impl StreamSpec {
    /// A stream specification.
    #[must_use]
    pub fn new(name: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            topic: topic.into(),
        }
    }
}

/// An endpoint captured by polling, because no stream carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollSpec {
    /// Name used in the archive path.
    pub name: String,
    /// URL to poll.
    pub url: String,
    /// Seconds between polls.
    pub interval_secs: u64,
}

/// How a venue confirms that a subscription is live.
///
/// This is not bookkeeping. A subscription that is accepted and then
/// delivers nothing is indistinguishable from a market with nothing to
/// say, and the difference is only noticed when someone wonders why a
/// file never grew. That is not hypothetical: a live probe of Binance
/// USD-M on 2026-08-16 found `aggTrade`, `kline_*`, `ticker`,
/// `miniTicker` and the `!…@arr` fan-outs all confirmed and all silent,
/// while the raw streams worked. The protocol acknowledges any name
/// without validating it.
///
/// Making the acknowledgement explicit turns that from an unobservable
/// condition into a timeout. NautilusTrader arrives at the same shape
/// for the same reason: venues without an explicit ack are treated as
/// confirming on their first data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckPolicy {
    /// Nothing confirms this subscription, and silence proves nothing.
    ///
    /// For streams that are legitimately empty for long stretches — a
    /// liquidation feed can go hours without an event — treating silence
    /// as failure would tear the connection down and rebuild it forever,
    /// which is a worse failure than the one being detected. The dead
    /// stream those streams could hide is accepted knowingly.
    None,
    /// The first message received is the acknowledgement. If none
    /// arrives within the deadline the subscription is considered
    /// failed, however healthy the socket looks.
    ///
    /// Only for streams expected to be continuously busy.
    FirstDataIsAck {
        /// How long to wait before calling the subscription dead.
        deadline: Duration,
    },
    /// The venue replies with its own acknowledgement, which contains
    /// this marker. Anything else arriving first is data, not an ack.
    Explicit {
        /// Byte sequence that identifies a successful acknowledgement.
        marker: Vec<u8>,
        /// Byte sequence that identifies a refusal.
        ///
        /// A venue that answers explicitly answers both ways, and the
        /// two answers are worth telling apart: a refused subscription
        /// will never succeed, while a silent one may just be a quiet
        /// market. Without this the refusal falls through as data — it
        /// gets written into the archive as though it were a book
        /// update — and the wait ends at the deadline reporting that
        /// the subscription "was accepted but delivered nothing", which
        /// is the opposite of what happened.
        ///
        /// Empty means the venue has no distinct refusal to look for.
        reject_marker: Vec<u8>,
        /// How long to wait for it.
        deadline: Duration,
    },
}

/// Everything needed to open one stream: where to connect, what to say
/// once connected, and how to know it worked.
///
/// `subscribe` carries raw frames rather than a venue-specific handshake
/// type, which is what lets the transport stay ignorant of the venue.
/// Binance encodes the subscription in the URL path and sends nothing;
/// OKX and Coinbase connect to a single endpoint and then send JSON. The
/// difference is data here, not a second code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// WebSocket URL to connect to.
    pub url: String,
    /// Frames to send after connecting. Empty when the URL is the
    /// subscription.
    pub subscribe: Vec<Vec<u8>>,
    /// How the subscription is confirmed.
    pub ack: AckPolicy,
    /// How long this venue tolerates hearing nothing from us before it
    /// hangs up, if it hangs up at all.
    ///
    /// `None` where the venue drives the keepalive itself: Binance sends
    /// a ping and a pong answers it, so nothing has to be scheduled.
    /// OKX is the other kind — it closes a connection with `4004 No data
    /// received in 30s` — and on a busy channel the market hides that,
    /// because data arriving *is* the keepalive. It only appears on a
    /// quiet one: a funding-rate subscription reconnects every thirty
    /// seconds and lands a gap in the archive each time, which reads as
    /// a flaky network rather than a missing ping.
    ///
    /// A protocol-level ping frame is what gets sent, deliberately.
    /// OKX also documents a text `ping` that draws a text `pong`, and
    /// that `pong` would be captured as a record — noise in an archive
    /// whose whole promise is that it holds what the venue said.
    pub keepalive: Option<Duration>,
}

/// The precision a venue quotes an instrument in.
///
/// Absent this, a consumer has to be told the scale by hand and will
/// eventually be told wrong. Replaying a HYPEUSDT capture with the
/// default of two decimals reported eleven thousand unparseable
/// messages, for prices like `57.45300` that are perfectly valid at five
/// — a data-quality alarm raised entirely by a missing definition.
/// Re-exported rather than defined here.
///
/// This crate used to carry its own two-field version, and `oq-margin`
/// carried the economics under a different name. Two definitions of one
/// thing meet nowhere and drift everywhere; worse, neither of them said
/// what a quantity counts, which is the difference between a size on a
/// venue that quotes contracts and one that quotes the asset.
pub use oq_types::Instrument;

/// What the capture path needs from a venue.
///
/// Implementations are expected to be thin. Anything that can be left in
/// the payload should be.
pub trait Venue {
    /// Identifier used in the archive path, e.g. `binance-perp`.
    ///
    /// This is the venue's identity, not a cosmetic label: it is what
    /// selects the implementation and what the archive is filed under,
    /// so the two can never disagree.
    fn id(&self) -> &'static str;

    /// Streams available for a symbol.
    fn streams(&self, symbol: &str) -> Vec<StreamSpec>;

    /// Endpoints polled for a symbol, for data no stream carries.
    fn polls(&self, symbol: &str) -> Vec<PollSpec>;

    /// How to open one of this venue's streams.
    fn transport(&self, spec: &StreamSpec) -> Transport;

    /// The exchange event time inside a payload, in nanoseconds.
    ///
    /// Only used to decide which file a record belongs in. `None` means
    /// the caller falls back to local time.
    fn event_time_ns(&self, payload: &[u8]) -> Option<i64>;

    /// The same reader as a plain function, for the capture loop.
    ///
    /// The loop holds a `fn` rather than a trait object so that its
    /// configuration stays `Copy` and free of lifetimes. Implementations
    /// are stateless, so handing out a function pointer costs nothing
    /// and keeps the loop from having to name a venue to get one.
    fn event_time_reader(&self) -> fn(&[u8]) -> Option<i64>;

    /// Quoting precision for a symbol, when the venue is known to
    /// publish it.
    fn instrument(&self, symbol: &str) -> Option<Instrument>;

    /// Read a trade out of a payload: price and size, in ticks and lots.
    ///
    /// Only what a tick needs. Everything else the venue said is still
    /// in the archive for whoever wants it, and parsing more here would
    /// be more that can be parsed wrongly once and be wrong forever.
    ///
    /// This belongs to the venue for the same reason the subscription
    /// does. The shapes have nothing in common: one venue sends
    /// `"p"` and `"q"` at the top level, the other `"px"` and `"sz"`
    /// nested under `"data"`. A reader written for either finds nothing
    /// in the other — which is not an error, just an empty result, so
    /// the conversion produces no ticks and says the archive was empty.
    fn parse_trade(&self, payload: &[u8], scales: crate::depth::Scales) -> Option<Trade>;

    /// Every trade id carried by a payload, in the order they appear.
    ///
    /// Completeness is checked by following the ids the venue issued, so
    /// this is what makes that check possible — and it has to be the
    /// venue's, because the shapes share nothing: a bare `"t":12345` on
    /// one, a quoted `"tradeId":"12345"` on the other. A reader written
    /// for either finds no ids at all in the other, and no ids means no
    /// gaps among them, which is an empty check wearing the shape of a
    /// passing one.
    ///
    /// A `Vec` rather than an `Option` because a venue may put several
    /// trades in one frame. Taking only the first would report every
    /// other trade in that frame as missing.
    fn trade_ids(&self, payload: &[u8]) -> Vec<u64>;

    /// Read an order book update out of a payload.
    ///
    /// # Errors
    ///
    /// When the payload is not a book update for this venue, or a price
    /// or size does not fit the given scale.
    fn parse_depth(
        &self,
        payload: &[u8],
        scales: crate::depth::Scales,
    ) -> Result<crate::depth::DepthUpdate, crate::depth::ParseError>;

    /// Which archive window a record at `ts` belongs to.
    ///
    /// The default divides the clock: one file per UTC day, or per UTC
    /// hour. That is right for a market that never closes and wrong for
    /// every market that does. A US equities session is six and a half
    /// hours inside a UTC day, mostly empty; a futures session opens the
    /// evening before and crosses UTC midnight, so one trading day
    /// becomes two files under this rule.
    ///
    /// The archive's central invariant is that a file holds one whole
    /// period — `oq-merge`, `oq-book-check` and `oq-ingest` all lean on
    /// it — so a venue whose day is not the clock's day overrides this
    /// rather than having the invariant bent around it.
    ///
    /// It lives on the venue because a session belongs to a market, not
    /// to a capture process, and because the alternative is a global
    /// enum that grows a variant per exchange calendar.
    fn window_of(&self, ts: i64, rotation: crate::day::Rotation) -> crate::day::Window {
        crate::day::Window::from_nanos(ts, rotation)
    }
}

/// One trade, reduced to what a tick is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// Price in instrument ticks.
    pub price: i64,
    /// Size in instrument lots.
    pub qty: i64,
}

/// Look up a venue by the identifier used on the command line and in
/// the archive path.
///
/// A registry rather than a match in the binary, so that adding a venue
/// touches one file.
#[must_use]
pub fn by_id(id: &str) -> Option<Box<dyn Venue>> {
    match id {
        "binance-perp" => Some(Box::new(binance::BinancePerp)),
        "okx-swap" => Some(Box::new(okx::OkxSwap)),
        _ => None,
    }
}

/// Every integer following `key`, optionally past an opening quote.
///
/// Shared because the difference between the venues here is one quote,
/// and two near-identical scans would be two places for the same bug.
/// Deliberately a scan rather than a parse: the payload is stored
/// verbatim regardless, and this value only has to identify a trade.
pub(crate) fn ids_after(payload: &[u8], key: &[u8], quoted: bool) -> Vec<u64> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + key.len() <= payload.len() {
        let Some(found) = payload[at..]
            .windows(key.len())
            .position(|w| w == key)
            .map(|p| at + p)
        else {
            break;
        };
        let mut i = found + key.len();
        if quoted {
            // A quoted value must actually be quoted. Skipping this
            // would read the digits of whatever followed instead.
            if payload.get(i) != Some(&b'"') {
                at = found + key.len();
                continue;
            }
            i += 1;
        }
        let digits: Vec<u8> = payload[i..]
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if let Ok(id) = core::str::from_utf8(&digits).unwrap_or("").parse::<u64>() {
            out.push(id);
        }
        at = found + key.len();
    }
    out
}

/// Every registered venue identifier, for error messages and `--help`.
#[must_use]
pub fn known_ids() -> &'static [&'static str] {
    &["binance-perp", "okx-swap"]
}
