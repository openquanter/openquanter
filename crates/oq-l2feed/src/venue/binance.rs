//! Binance USD-M futures.
//!
//! A worked example of [`Venue`](super::Venue): the whole venue is the
//! list of stream names, a URL shape, one timestamp field and a table of
//! precisions.

use core::time::Duration;

use super::{AckPolicy, Deployment, Instrument, PollSpec, StreamSpec, Trade, Transport, Venue};
use crate::depth::{DepthUpdate, ParseError, Scales, parse_fixed};

/// Binance USD-M perpetual futures.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinancePerp {
    deployment: Deployment,
}

impl BinancePerp {
    /// The mainnet adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            deployment: Deployment::Live,
        }
    }

    /// An adapter for a named deployment.
    #[must_use]
    pub const fn at(deployment: Deployment) -> Self {
        Self { deployment }
    }

    /// Websocket host for market data.
    const fn stream_host(&self) -> &'static str {
        match self.deployment {
            Deployment::Live => "wss://fstream.binance.com",
            Deployment::Testnet => "wss://stream.binancefuture.com",
        }
    }

    /// REST host for the polled streams.
    const fn rest_host(&self) -> &'static str {
        match self.deployment {
            Deployment::Live => "https://fapi.binance.com",
            Deployment::Testnet => "https://testnet.binancefuture.com",
        }
    }
}

/// How long a stream may stay silent after subscribing before the
/// subscription is treated as failed.
///
/// Long enough that a genuinely quiet market — a liquidation feed at
/// four in the morning — is not mistaken for a dead subscription, short
/// enough that a dead one is caught in the same session rather than
/// discovered days later in the archive.
const ACK_DEADLINE: Duration = Duration::from_secs(120);

/// How often to prove a silent connection is still alive.
///
/// Only used on streams whose silence is ordinary. One minute is the
/// same order as the capture loop's read timeout, so a ping goes out
/// roughly whenever that timeout would otherwise have declared the
/// connection dead, and an unanswered one still ends the connection
/// within a few minutes.
const KEEPALIVE: Duration = Duration::from_secs(60);

impl Venue for BinancePerp {
    fn id(&self) -> &'static str {
        "binance-perp"
    }

    /// Incremental depth and best bid/offer give the book and the queue
    /// model; trades give what consumes the queue ahead of you; forced
    /// liquidations give scarce tail data.
    ///
    /// **`trade`, not `aggTrade`.** A live probe on 2026-08-16 found the
    /// venue's aggregated and derived streams — `aggTrade`, `kline_*`,
    /// `ticker`, `miniTicker` and the `!…@arr` fan-outs — silently
    /// delivering nothing while the raw streams worked. `trade` is the
    /// finer-grained source anyway: individual fills rather than fills
    /// pre-aggregated by price and time, which is what a queue model
    /// wants.
    fn streams(&self, symbol: &str) -> Vec<StreamSpec> {
        let lower = symbol.to_lowercase();
        vec![
            StreamSpec::new("depth", format!("{lower}@depth@0ms")),
            // The same book, coalesced by the venue every 100ms.
            //
            // For capture, `depth` is right: the finest resolution the
            // venue will give is the point. For a consumer that folds
            // events into fixed windows anyway, it is a firehose whose
            // extra resolution is discarded on arrival — and a consumer
            // that cannot drain it fast enough is dropped by the venue
            // for being slow, which costs the whole connection rather
            // than the resolution it never used.
            StreamSpec::new("depth100", format!("{lower}@depth@100ms")),
            StreamSpec::new("bookTicker", format!("{lower}@bookTicker")),
            StreamSpec::new("trade", format!("{lower}@trade")),
            StreamSpec::new("forceOrder", format!("{lower}@forceOrder")),
        ]
    }

    /// `markPrice` carries the mark price, index price and funding rate
    /// — the margin engine's inputs, since liquidation is computed
    /// against mark price rather than the last trade. It has no working
    /// stream here, so it is polled.
    fn polls(&self, symbol: &str) -> Vec<PollSpec> {
        vec![PollSpec {
            name: "markPrice".to_string(),
            url: format!(
                "{}/fapi/v1/premiumIndex?symbol={}",
                self.rest_host(),
                symbol.to_uppercase()
            ),
            interval_secs: 1,
        }]
    }

    /// The subscription is the URL path, so nothing is sent after
    /// connecting and the first message is the only confirmation
    /// available.
    ///
    /// The acknowledgement is per stream, because silence means
    /// different things on different ones. Depth, best bid/offer and
    /// trades are continuously busy on a liquid contract, so a minute of
    /// nothing is a dead subscription. Liquidations are not: a quiet
    /// hour is an ordinary hour, and holding them to the same rule would
    /// reconnect them forever.
    fn transport(&self, spec: &StreamSpec) -> Transport {
        let ack = match spec.name.as_str() {
            "depth" | "bookTicker" | "trade" => AckPolicy::FirstDataIsAck {
                deadline: ACK_DEADLINE,
            },
            _ => AckPolicy::None,
        };
        // A stream that is allowed to be silent needs a way to prove the
        // socket is still there, because the capture loop's read timeout
        // cannot tell a quiet contract from a dead connection and treats
        // both as a disconnect. Without this, a liquidation feed
        // reconnects every read timeout for as long as it stays quiet:
        // measured on this venue, 22,931 markers and not one liquidation
        // in 21 days on BTCUSDT. The venue's own pings arrive every few
        // minutes, far too rarely to keep a one-minute read alive.
        //
        // The busy streams keep `None`: data arriving is their
        // keepalive, and a minute of nothing there really is a dead
        // subscription.
        let keepalive = match &ack {
            AckPolicy::None => Some(KEEPALIVE),
            _ => None,
        };
        Transport {
            url: format!("{}/ws/{}", self.stream_host(), spec.topic),
            subscribe: Vec::new(),
            ack,
            keepalive,
        }
    }

    /// The scan is deliberately crude — find the `"E":` key and read the
    /// integer after it — because the payload is stored verbatim
    /// regardless and this value only decides which file the record
    /// lands in. A parser sophisticated enough to be wrong in
    /// interesting ways would be a worse trade: getting the day wrong is
    /// recoverable by re-sorting the archive, getting the bytes wrong is
    /// not.
    fn event_time_ns(&self, payload: &[u8]) -> Option<i64> {
        event_time_ns(payload)
    }

    fn event_time_reader(&self) -> fn(&[u8]) -> Option<i64> {
        event_time_ns
    }

    /// A bare integer after `"t":`. This venue sends one trade per
    /// message, so there is normally one; the scan does not assume it.
    fn trade_ids(&self, payload: &[u8]) -> Vec<u64> {
        super::ids_after(payload, br#""t":"#, false)
    }

    /// Precisions are per-contract and there is no way to guess them:
    /// BTCUSDT quotes two decimals, HYPEUSDT five. A wrong value does
    /// not fail loudly — it reports the archive as unreadable, which is
    /// the most misleading answer available.
    ///
    /// The table is generated from the venue rather than hand-written,
    /// because the venue lists hundreds of contracts and a hand-kept
    /// list is a list that is wrong for whichever contract nobody
    /// remembered. See [`super::binance_instruments`].
    fn instrument(&self, symbol: &str) -> Option<Instrument> {
        let (price_scale, qty_scale) =
            super::binance_instruments::precision(&symbol.to_uppercase())?;
        // USD-M perpetuals quote an amount of the underlying: a
        // quantity of 1 on BTCUSDT is one bitcoin, so a contract is the
        // asset itself.
        Some(Instrument::linear(price_scale, qty_scale))
    }

    /// Price and size sit at the top level as `"p"` and `"q"`.
    /// A zero price on this stream is a placeholder, not a trade.
    ///
    /// Binance publishes records shaped
    /// `{"e":"trade",...,"p":"0","q":"0","X":"NA",...}` among the real
    /// ones -- 6312 in one hour of BTCUSDT against 1.45 million real
    /// trades. They carry trade ids and sit in the id chain, so
    /// completeness checks are right to count them; they are not
    /// trades, and anything that treats them as one takes a price of
    /// zero.
    ///
    /// That is not a small error. A window's low is the minimum of the
    /// prices in it, and one zero makes it zero -- 1355 of 1409 minutes
    /// of real BTCUSDT, every one of them reporting a low of 0.00 while
    /// its high was right. A resting buy is triggered by the low, so
    /// the backtest that reads that file fills orders no venue would
    /// have filled. The same parse runs in the live loop.
    fn parse_trade(&self, payload: &[u8], scales: Scales) -> Option<Trade> {
        let price = string_field(payload, br#""p":"#)?;
        let qty = string_field(payload, br#""q":"#)?;
        // `"m"` says whether the *buyer* was the maker. So `true` means
        // the buyer was resting and the seller crossed: the aggressor is
        // the seller. Reading it as the trade's side would invert every
        // one of them.
        let aggressor = match string_or_bool(payload, br#""m":"#) {
            Some(true) => Some(oq_types::Side::Sell),
            Some(false) => Some(oq_types::Side::Buy),
            None => None,
        };
        let trade = Trade {
            price: parse_fixed(&price, scales.price).ok()?,
            qty: parse_fixed(&qty, scales.qty).ok()?,
            aggressor,
        };
        (trade.price > 0 && trade.qty > 0).then_some(trade)
    }

    fn parse_depth(&self, payload: &[u8], scales: Scales) -> Result<DepthUpdate, ParseError> {
        crate::depth::parse_depth(payload, scales)
    }
}

/// Read a bare JSON `true`/`false` following `key`.
///
/// Separate from [`string_field`] because this value is not quoted, and
/// a reader looking for quotes finds the next field's instead — which
/// parses, and inverts the aggressor on every trade that happens to be
/// followed by the right shape.
fn string_or_bool(payload: &[u8], key: &[u8]) -> Option<bool> {
    let pos = payload.windows(key.len()).position(|w| w == key)?;
    let rest = &payload[pos + key.len()..];
    if rest.starts_with(b"true") {
        Some(true)
    } else if rest.starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

/// Extract a JSON string value that follows `key`.
fn string_field(payload: &[u8], key: &[u8]) -> Option<String> {
    let pos = payload.windows(key.len()).position(|w| w == key)?;
    let rest = &payload[pos + key.len()..];
    let start = rest.iter().position(|b| *b == b'"')? + 1;
    let end = start + rest[start..].iter().position(|b| *b == b'"')?;
    core::str::from_utf8(&rest[start..end])
        .ok()
        .map(str::to_owned)
}

fn event_time_ns(payload: &[u8]) -> Option<i64> {
    // `E` on the streams, `time` on the REST payloads this venue is
    // polled for: `premiumIndex` carries no `E`, so a poll archived
    // without this reads back as a file with no exchange time at all --
    // which is what every markPrice manifest said before it was added.
    find_int_field(payload, b"\"E\":")
        .or_else(|| find_int_field(payload, b"\"time\":"))?
        .checked_mul(1_000_000)
}

/// Find `key` in `payload` and parse the integer that follows it.
fn find_int_field(payload: &[u8], key: &[u8]) -> Option<i64> {
    let mut from = 0usize;
    while let Some(pos) = find(&payload[from..], key) {
        let start = from + pos + key.len();
        let digits: Vec<u8> = payload[start..]
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return core::str::from_utf8(&digits).ok()?.parse().ok();
        }
        from = start;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The REST snapshot that re-establishes book state after a reconnect.
#[must_use]
pub fn snapshot_url(symbol: &str, limit: u32) -> String {
    format!(
        "https://fapi.binance.com/fapi/v1/depth?symbol={}&limit={limit}",
        symbol.to_uppercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_time_is_read_from_the_e_field() {
        let payload = br#"{"e":"depthUpdate","E":1786780800123,"s":"BTCUSDT"}"#;
        assert_eq!(
            BinancePerp::new().event_time_ns(payload),
            Some(1_786_780_800_123_000_000)
        );
    }

    #[test]
    fn a_payload_without_an_event_time_yields_none() {
        assert_eq!(
            BinancePerp::new().event_time_ns(br#"{"result":null}"#),
            None
        );
    }

    #[test]
    fn the_subscription_is_the_url_so_nothing_is_sent() {
        let spec = &BinancePerp::new().streams("BTCUSDT")[0];
        let t = BinancePerp::new().transport(spec);
        assert!(t.url.starts_with("wss://"));
        assert!(t.url.ends_with("btcusdt@depth@0ms"));
        assert!(
            t.subscribe.is_empty(),
            "this venue subscribes by URL; sending a frame would be wrong"
        );
        assert!(matches!(t.ack, AckPolicy::FirstDataIsAck { .. }));
    }

    #[test]
    fn a_confirmed_but_dead_stream_is_caught_by_the_deadline() {
        // The failure this whole mechanism exists for: the venue accepts
        // any stream name without validating it, so a retired stream
        // subscribes successfully and then says nothing forever. The
        // policy has to be one that treats that as an error.
        let spec = StreamSpec::new("depth", "btcusdt@aggTrade");
        match BinancePerp::new().transport(&spec).ack {
            AckPolicy::FirstDataIsAck { deadline } => {
                assert!(deadline.as_secs() > 0 && deadline.as_secs() <= 300);
            }
            other => panic!("a busy stream must be held to first data, got {other:?}"),
        }
    }

    #[test]
    fn a_silent_stream_carries_a_keepalive_and_a_busy_one_does_not() {
        // A stream allowed to be silent has to prove the socket is alive
        // some other way, or the capture loop's read timeout tears it
        // down every minute it says nothing -- which is how 21 days of
        // liquidation capture came back holding only gap markers.
        let specs = BinancePerp::new().streams("BTCUSDT");
        let force = specs
            .iter()
            .find(|s| s.name == "forceOrder")
            .expect("forceOrder");
        let keepalive = BinancePerp::new()
            .transport(force)
            .keepalive
            .expect("a silent stream needs a keepalive");
        assert!(keepalive.as_secs() > 0 && keepalive.as_secs() <= 120);

        let depth = specs.iter().find(|s| s.name == "depth").expect("depth");
        assert_eq!(
            BinancePerp::new().transport(depth).keepalive,
            None,
            "on a busy stream the data is the keepalive"
        );
    }

    #[test]
    fn a_polled_payload_carries_its_own_event_time() {
        // `premiumIndex` has no `E`. Without the fallback every markPrice
        // manifest reported no exchange time at all.
        let poll = br#"{"symbol":"BTCUSDT","markPrice":"79695.60","nextFundingTime":1788537600000,"time":1788523201736}"#;
        assert_eq!(
            super::event_time_ns(poll),
            Some(1_788_523_201_736_000_000),
            "the poll's own timestamp is the event time"
        );
        let stream = br#"{"e":"markPriceUpdate","E":1788523201736,"s":"BTCUSDT"}"#;
        assert_eq!(
            super::event_time_ns(stream),
            Some(1_788_523_201_736_000_000)
        );
    }

    #[test]
    fn a_liquidation_feed_is_allowed_to_be_silent() {
        // Holding forceOrder to "first data confirms" would tear the
        // connection down every deadline through any quiet hour, which
        // is a worse failure than the dead subscription it detects.
        let specs = BinancePerp::new().streams("BTCUSDT");
        let force = specs
            .iter()
            .find(|s| s.name == "forceOrder")
            .expect("forceOrder");
        assert_eq!(BinancePerp::new().transport(force).ack, AckPolicy::None);

        let depth = specs.iter().find(|s| s.name == "depth").expect("depth");
        assert!(matches!(
            BinancePerp::new().transport(depth).ack,
            AckPolicy::FirstDataIsAck { .. }
        ));
    }

    #[test]
    fn precisions_differ_between_contracts() {
        // The pair that made this necessary: replaying HYPEUSDT with
        // BTCUSDT's scale reported the archive as unreadable.
        assert_eq!(
            BinancePerp::new()
                .instrument("BTCUSDT")
                .unwrap()
                .price_scale,
            2
        );
        assert_eq!(
            BinancePerp::new()
                .instrument("HYPEUSDT")
                .unwrap()
                .price_scale,
            5
        );
        assert!(BinancePerp::new().instrument("NOTLISTED").is_none());
    }

    #[test]
    fn the_id_matches_the_registry_key() {
        // The archive path and the venue selector must be the same
        // string, or data lands under a name that cannot select it back.
        assert_eq!(BinancePerp::new().id(), "binance-perp");
        assert!(super::super::by_id("binance-perp").is_some());
        assert_eq!(
            super::super::by_id("binance-perp").unwrap().id(),
            "binance-perp"
        );
    }
}

#[cfg(test)]
mod cadence {
    use super::*;

    /// Both books are published: the venue's every change, and its own
    /// hundred-millisecond coalescing of the same.
    ///
    /// Capture wants the first — the finest thing the venue will give is
    /// the point of capturing. A consumer that folds into windows wants
    /// the second, and taking the first anyway is what made one such
    /// consumer a slow reader of its own feed: the events queued, the
    /// venue dropped it for not keeping up, and the resolution it paid
    /// the connection for had been discarded on arrival.
    #[test]
    fn the_book_is_offered_at_two_cadences() {
        let v = BinancePerp::new();
        let s = v.streams("BTCUSDT");
        let full = s.iter().find(|s| s.name == "depth").expect("depth");
        let coarse = s.iter().find(|s| s.name == "depth100").expect("depth100");
        assert_eq!(full.topic, "btcusdt@depth@0ms");
        assert_eq!(coarse.topic, "btcusdt@depth@100ms");
    }
}
