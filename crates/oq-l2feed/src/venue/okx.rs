//! OKX V5 public channels.
//!
//! The second venue, and the reason the adapter seam exists. Everything
//! that differs from Binance here differs as *data* — a URL, a JSON
//! frame, a marker to wait for — and nothing in the transport or the
//! capture loop knows which venue it is talking to.
//!
//! Three differences are worth naming, because each one is a place a
//! thinner abstraction would have needed a special case:
//!
//! * The subscription is a message, not a URL. One endpoint serves every
//!   channel and the client says what it wants after connecting.
//! * The venue acknowledges explicitly, so a subscription that was
//!   accepted is distinguishable from one that was not — where Binance
//!   can only be inferred from its first data.
//! * Timestamps are quoted strings (`"ts":"1786881328502"`) rather than
//!   bare integers, which is exactly the kind of shape that a parser
//!   written for one venue gets silently wrong on another.

use core::time::Duration;

use super::{AckPolicy, Instrument, PollSpec, StreamSpec, Transport, Venue};

/// OKX perpetual swaps.
#[derive(Debug, Clone, Copy, Default)]
pub struct OkxSwap;

/// How long to wait for the subscription acknowledgement.
///
/// Short, because this venue answers directly rather than leaving it to
/// be inferred from traffic: if the acknowledgement has not arrived in
/// this long, it is not coming.
const ACK_DEADLINE: Duration = Duration::from_secs(20);

impl Venue for OkxSwap {
    fn id(&self) -> &'static str {
        "okx-swap"
    }

    /// `books` is the incremental order book; `trades` is individual
    /// fills. Both carry `seqId` and `prevSeqId`, so a replay can tell a
    /// gap from a quiet market without trusting arrival order.
    fn streams(&self, symbol: &str) -> Vec<StreamSpec> {
        let inst = instrument_id(symbol);
        vec![
            StreamSpec::new("depth", format!("books:{inst}")),
            StreamSpec::new("trade", format!("trades:{inst}")),
        ]
    }

    /// Nothing is polled here. Funding and mark price have working
    /// channels on this venue, unlike the venue whose absence of one
    /// forced the polling path to exist.
    fn polls(&self, _symbol: &str) -> Vec<PollSpec> {
        Vec::new()
    }

    fn transport(&self, spec: &StreamSpec) -> Transport {
        let (channel, inst) = spec.topic.split_once(':').unwrap_or(("books", &spec.topic));
        let subscribe = format!(
            r#"{{"id":"{channel}","op":"subscribe","args":[{{"channel":"{channel}","instId":"{inst}"}}]}}"#
        );
        Transport {
            url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            subscribe: vec![subscribe.into_bytes()],
            // An explicit acknowledgement, so a rejected subscription is
            // an error rather than a silence to be interpreted.
            ack: AckPolicy::Explicit {
                marker: br#""event":"subscribe""#.to_vec(),
                deadline: ACK_DEADLINE,
            },
        }
    }

    /// This venue quotes its timestamps as strings. A reader written for
    /// a venue that sends bare integers finds nothing here and falls
    /// back to local time, which does not fail — it just files records
    /// under whichever window the capture host's clock suggests, and the
    /// error only appears as a boundary that drifts.
    fn event_time_ns(&self, payload: &[u8]) -> Option<i64> {
        event_time_ns(payload)
    }

    fn event_time_reader(&self) -> fn(&[u8]) -> Option<i64> {
        event_time_ns
    }

    /// Not yet published for this venue.
    ///
    /// `None` is the honest answer and the tools treat it as one: they
    /// stop and ask rather than assuming a scale, because a wrong
    /// precision rescales every price without failing. The generated
    /// table that Binance has comes from its `exchangeInfo`; the
    /// equivalent here is `/api/v5/public/instruments`, and until that is
    /// generated this says so instead of guessing.
    fn instrument(&self, _symbol: &str) -> Option<Instrument> {
        None
    }
}

/// Map a plain symbol to this venue's instrument id.
///
/// `BTCUSDT` is what every other venue and the archive path call it;
/// `BTC-USDT-SWAP` is what this one wants. Doing the mapping here keeps
/// the archive layout comparable across venues, which is what makes a
/// captured day from two venues line up at all.
fn instrument_id(symbol: &str) -> String {
    let upper = symbol.to_uppercase();
    if upper.contains('-') {
        return upper;
    }
    for quote in ["USDT", "USDC", "USD"] {
        if let Some(base) = upper.strip_suffix(quote) {
            return format!("{base}-{quote}-SWAP");
        }
    }
    upper
}

/// Read `"ts":"<digits>"` — quoted, unlike other venues.
fn event_time_ns(payload: &[u8]) -> Option<i64> {
    let key = br#""ts":""#;
    let pos = payload.windows(key.len()).position(|w| w == key)? + key.len();
    let digits: Vec<u8> = payload[pos..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    core::str::from_utf8(&digits)
        .ok()?
        .parse::<i64>()
        .ok()?
        .checked_mul(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_timestamp_is_read_where_a_bare_one_would_not_be() {
        // The shape that a parser written for another venue misses. It
        // does not fail there — it returns None and the record is filed
        // by local time instead, which is wrong in a way that only shows
        // up as a window boundary that drifts.
        let payload =
            br#"{"arg":{"channel":"books"},"data":[{"ts":"1786881328502","checksum":0}]}"#;
        assert_eq!(
            OkxSwap.event_time_ns(payload),
            Some(1_786_881_328_502_000_000)
        );

        assert_eq!(
            super::super::binance::BinancePerp.event_time_ns(payload),
            None,
            "the other venue's reader must not silently half-work on this shape"
        );
    }

    #[test]
    fn the_subscription_is_a_frame_rather_than_a_url() {
        let spec = &OkxSwap.streams("BTCUSDT")[0];
        let t = OkxSwap.transport(spec);
        assert_eq!(t.url, "wss://ws.okx.com:8443/ws/v5/public");
        assert_eq!(t.subscribe.len(), 1, "one frame carries the subscription");
        let frame = String::from_utf8(t.subscribe[0].clone()).expect("utf8");
        assert!(frame.contains(r#""op":"subscribe""#));
        assert!(frame.contains(r#""instId":"BTC-USDT-SWAP""#));
        assert!(matches!(t.ack, AckPolicy::Explicit { .. }));
    }

    #[test]
    fn symbols_are_translated_to_this_venue_and_left_alone_if_already_translated() {
        assert_eq!(instrument_id("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(instrument_id("ethusdt"), "ETH-USDT-SWAP");
        assert_eq!(instrument_id("BTC-USDT-SWAP"), "BTC-USDT-SWAP");
    }

    #[test]
    fn an_unknown_instrument_is_none_rather_than_a_guess() {
        assert!(OkxSwap.instrument("BTCUSDT").is_none());
    }

    #[test]
    fn the_id_matches_the_registry_key() {
        assert_eq!(OkxSwap.id(), "okx-swap");
        assert_eq!(super::super::by_id("okx-swap").unwrap().id(), "okx-swap");
    }
}
