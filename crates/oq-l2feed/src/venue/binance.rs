//! Binance USD-M futures.
//!
//! A worked example of [`Venue`](super::Venue): the whole venue is the
//! list of stream names, a URL shape, one timestamp field and a table of
//! precisions.

use core::time::Duration;

use super::{AckPolicy, Instrument, PollSpec, StreamSpec, Transport, Venue};

/// Binance USD-M perpetual futures.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinancePerp;

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
                "https://fapi.binance.com/fapi/v1/premiumIndex?symbol={}",
                symbol.to_uppercase()
            ),
            interval_secs: 1,
        }]
    }

    /// The subscription is the URL path, so nothing is sent after
    /// connecting and the first message is the only confirmation
    /// available.
    fn transport(&self, spec: &StreamSpec) -> Transport {
        Transport {
            url: format!("wss://fstream.binance.com/ws/{}", spec.topic),
            subscribe: Vec::new(),
            ack: AckPolicy::FirstDataIsAck {
                deadline: ACK_DEADLINE,
            },
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
        find_int_field(payload, b"\"E\":")?.checked_mul(1_000_000)
    }

    /// Precisions are per-contract on this venue and there is no way to
    /// guess them: BTCUSDT quotes two decimals, HYPEUSDT five. A wrong
    /// value does not fail loudly — it reports the archive as
    /// unreadable, which is the most misleading answer available.
    fn instrument(&self, symbol: &str) -> Option<Instrument> {
        let (price_scale, qty_scale) = match symbol.to_uppercase().as_str() {
            "BTCUSDT" => (2, 3),
            "ETHUSDT" => (2, 3),
            "BNBUSDT" => (2, 2),
            "HYPEUSDT" => (5, 1),
            _ => return None,
        };
        Some(Instrument {
            price_scale,
            qty_scale,
        })
    }
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
            BinancePerp.event_time_ns(payload),
            Some(1_786_780_800_123_000_000)
        );
    }

    #[test]
    fn a_payload_without_an_event_time_yields_none() {
        assert_eq!(BinancePerp.event_time_ns(br#"{"result":null}"#), None);
    }

    #[test]
    fn the_subscription_is_the_url_so_nothing_is_sent() {
        let spec = &BinancePerp.streams("BTCUSDT")[0];
        let t = BinancePerp.transport(spec);
        assert!(t.url.starts_with("wss://"));
        assert!(t.url.ends_with("btcusdt@depth@0ms"));
        assert!(
            t.subscribe.is_empty(),
            "this venue subscribes by URL; sending a frame would be wrong"
        );
        assert!(matches!(t.ack, AckPolicy::FirstDataIsAck { .. }));
    }

    #[test]
    fn precisions_differ_between_contracts() {
        // The pair that made this necessary: replaying HYPEUSDT with
        // BTCUSDT's scale reported the archive as unreadable.
        assert_eq!(BinancePerp.instrument("BTCUSDT").unwrap().price_scale, 2);
        assert_eq!(BinancePerp.instrument("HYPEUSDT").unwrap().price_scale, 5);
        assert!(BinancePerp.instrument("NOTLISTED").is_none());
    }

    #[test]
    fn the_id_matches_the_registry_key() {
        // The archive path and the venue selector must be the same
        // string, or data lands under a name that cannot select it back.
        assert_eq!(BinancePerp.id(), "binance-perp");
        assert!(super::super::by_id("binance-perp").is_some());
        assert_eq!(super::super::by_id("binance-perp").unwrap().id(), "binance-perp");
    }
}
