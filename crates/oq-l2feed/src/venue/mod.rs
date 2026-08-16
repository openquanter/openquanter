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
//! the crate needs to change. See [`binance`] for a worked example; it
//! is about eighty lines, most of them the list of stream names.

pub mod binance;

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
    /// The first message received is the acknowledgement. If none
    /// arrives within the deadline the subscription is considered
    /// failed, however healthy the socket looks.
    FirstDataIsAck {
        /// How long to wait before calling the subscription dead.
        deadline: Duration,
    },
    /// The venue replies with its own acknowledgement, which contains
    /// this marker. Anything else arriving first is data, not an ack.
    Explicit {
        /// Byte sequence that identifies a successful acknowledgement.
        marker: Vec<u8>,
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
}

/// The precision a venue quotes an instrument in.
///
/// Absent this, a consumer has to be told the scale by hand and will
/// eventually be told wrong. Replaying a HYPEUSDT capture with the
/// default of two decimals reported eleven thousand unparseable
/// messages, for prices like `57.45300` that are perfectly valid at five
/// — a data-quality alarm raised entirely by a missing definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrument {
    /// Decimal places in a price.
    pub price_scale: u8,
    /// Decimal places in a quantity.
    pub qty_scale: u8,
}

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

    /// Quoting precision for a symbol, when the venue is known to
    /// publish it.
    fn instrument(&self, symbol: &str) -> Option<Instrument>;
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
        _ => None,
    }
}

/// Every registered venue identifier, for error messages and `--help`.
#[must_use]
pub fn known_ids() -> &'static [&'static str] {
    &["binance-perp"]
}
