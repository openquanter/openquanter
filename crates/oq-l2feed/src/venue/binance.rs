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
        Transport {
            url: format!("{}/ws/{}", self.stream_host(), spec.topic),
            subscribe: Vec::new(),
            ack,
            // This venue pings us, and answering in place is enough.
            keepalive: None,
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
    find_int_field(payload, b"\"E\":")?.checked_mul(1_000_000)
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
