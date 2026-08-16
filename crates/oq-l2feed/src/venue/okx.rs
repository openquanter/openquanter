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

use super::{AckPolicy, Instrument, PollSpec, StreamSpec, Trade, Transport, Venue};
use crate::depth::{DepthUpdate, Level, ParseError, Scales, parse_fixed};

/// OKX perpetual swaps.
#[derive(Debug, Clone, Copy, Default)]
pub struct OkxSwap;

/// How long to wait for the subscription acknowledgement.
///
/// Short, because this venue answers directly rather than leaving it to
/// be inferred from traffic: if the acknowledgement has not arrived in
/// this long, it is not coming.
const ACK_DEADLINE: Duration = Duration::from_secs(20);

/// How often to prove the connection is still there.
///
/// The venue closes a connection it has received nothing on for thirty
/// seconds, with `4004 No data received in 30s`. Twenty leaves room for
/// one round trip to be slow without spending the margin twice over.
const KEEPALIVE: Duration = Duration::from_secs(20);

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
            // `trades-all`, not `trades`. The plain channel pushes only
            // the last fill of a taker order, so a capture from it is
            // missing trades by design — measured over a minute, 32 ids
            // spanning 34, against 40 spanning 40 here. The archive's
            // completeness check follows trade ids and would report the
            // difference forever as a fault in the capture.
            StreamSpec::new("trade", format!("trades-all:{inst}")),
            // The margin engine's inputs. Liquidation is computed
            // against mark price rather than the last trade, and funding
            // is a cash flow the backtest has to pay. Binance has no
            // working stream for either and polls them; here they are
            // channels, so they are subscribed. Leaving them out would
            // make an OKX capture and a Binance capture of the same day
            // not comparable, which is the property the second venue
            // exists to demonstrate.
            StreamSpec::new("markPrice", format!("mark-price:{inst}")),
            StreamSpec::new("fundingRate", format!("funding-rate:{inst}")),
        ]
    }

    /// Nothing is polled here. Funding and mark price have working
    /// channels on this venue, unlike the venue whose absence of one
    /// forced the polling path to exist, so they are subscribed in
    /// [`Self::streams`] rather than fetched on a timer.
    fn polls(&self, _symbol: &str) -> Vec<PollSpec> {
        Vec::new()
    }

    fn transport(&self, spec: &StreamSpec) -> Transport {
        let (channel, inst) = spec.topic.split_once(':').unwrap_or(("books", &spec.topic));
        // The correlation id is alphanumeric only — a hyphen in it is
        // rejected with `60033 Parameter id error`, before the venue
        // ever looks at the channel. `books` and `trades` happen to be
        // legal ids; `mark-price` and `funding-rate` are not, so a
        // channel name cannot be reused as one unmodified. The failure
        // does not name the id, it names a parameter, and the obvious
        // reading is that the channel is wrong.
        let id: String = channel
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let subscribe = format!(
            r#"{{"id":"{id}","op":"subscribe","args":[{{"channel":"{channel}","instId":"{inst}"}}]}}"#
        );
        Transport {
            url: endpoint(channel).to_string(),
            subscribe: vec![subscribe.into_bytes()],
            // An explicit acknowledgement, so a rejected subscription is
            // an error rather than a silence to be interpreted — which
            // requires listening for the refusal as well as the
            // acceptance. Watching only for the acceptance leaves the
            // refusal to fall through as data and the wait to end at the
            // deadline, which is the same outcome as a venue that says
            // nothing at all.
            ack: AckPolicy::Explicit {
                marker: br#""event":"subscribe""#.to_vec(),
                reject_marker: br#""event":"error""#.to_vec(),
                deadline: ACK_DEADLINE,
            },
            keepalive: Some(KEEPALIVE),
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

    /// A quoted integer after `"tradeId":`, and there may be several in
    /// one frame — this venue batches trades into a `data` array.
    fn trade_ids(&self, payload: &[u8]) -> Vec<u64> {
        super::ids_after(payload, br#""tradeId":"#, true)
    }

    /// Looked up after the symbol is translated, because the table is
    /// keyed by this venue's own instrument ids.
    ///
    /// The venue publishes a tick size where the other publishes a count
    /// of decimal places; the conversion happens in the generator, not
    /// here, so this stays a lookup. See [`super::okx_instruments`].
    fn instrument(&self, symbol: &str) -> Option<Instrument> {
        let (price_scale, qty_scale) = super::okx_instruments::precision(&instrument_id(symbol))?;
        Some(Instrument {
            price_scale,
            qty_scale,
        })
    }

    /// Price and size are `"px"` and `"sz"`, nested under `"data"`.
    fn parse_trade(&self, payload: &[u8], scales: Scales) -> Option<Trade> {
        let text = core::str::from_utf8(payload).ok()?;
        if !text.contains(r#""channel":"trades"#) {
            return None;
        }
        Some(Trade {
            price: parse_fixed(&quoted(text, r#""px":"#)?, scales.price).ok()?,
            qty: parse_fixed(&quoted(text, r#""sz":"#)?, scales.qty).ok()?,
        })
    }

    /// The book arrives as `"bids"` and `"asks"` inside `"data"`, and
    /// the sequence is `seqId` and `prevSeqId` where the other venue
    /// uses `u` and `pu`. The shapes differ; what they mean does not,
    /// which is why both can produce the same `DepthUpdate`.
    fn parse_depth(&self, payload: &[u8], scales: Scales) -> Result<DepthUpdate, ParseError> {
        let text = core::str::from_utf8(payload).map_err(|_| ParseError::NotDepth)?;
        if !text.contains(r#""channel":"books""#) {
            return Err(ParseError::NotDepth);
        }

        let ts = quoted(text, r#""ts":"#)
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(ParseError::MissingField("ts"))?;
        let seq = bare_int(text, r#""seqId":"#).ok_or(ParseError::MissingField("seqId"))?;
        // A snapshot has no predecessor and the venue says so with -1.
        // `prev_final_id` being an Option is exactly the right shape for
        // that: reporting -1 as a real predecessor would make the first
        // message after a resubscribe look like a break in the chain.
        let prev = bare_int(text, r#""prevSeqId":"#).filter(|p| *p >= 0);
        let first = prev.map_or(seq, |p| p + 1);

        Ok(DepthUpdate {
            event_ms: ts,
            first_id: u64::try_from(first).unwrap_or(0),
            final_id: u64::try_from(seq).unwrap_or(0),
            prev_final_id: prev.and_then(|p| u64::try_from(p).ok()),
            bids: levels(text, r#""bids":[["#, scales)?,
            asks: levels(text, r#""asks":[["#, scales)?,
        })
    }
}

/// A quoted JSON string value following `key`.
fn quoted(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(key)? + key.len()..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string())
}

/// A bare JSON integer following `key`, which may be negative.
fn bare_int(text: &str, key: &str) -> Option<i64> {
    let rest = &text[text.find(key)? + key.len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

/// Levels arrive as `[["price","size","liquidations","orders"], ...]`.
/// Only the first two matter here; the rest is still in the archive.
fn levels(text: &str, key: &str, scales: Scales) -> Result<Vec<Level>, ParseError> {
    let Some(start) = text.find(key) else {
        return Ok(Vec::new());
    };
    // Start at the outer bracket so the per-level split below sees the
    // first level too.
    let rest = &text[start + key.len() - 2..];
    let end = rest.find("]]").map_or(rest.len(), |i| i + 2);
    let mut out = Vec::new();
    for entry in rest[..end].split('[').skip(1) {
        let mut parts = entry.trim_start_matches('"').split(',');
        let Some(price) = parts.next() else { continue };
        let Some(qty) = parts.next() else { continue };
        let price = price.trim_matches(|c| c == '"' || c == ']');
        let qty = qty.trim_matches(|c| c == '"' || c == ']');
        if price.is_empty() || qty.is_empty() {
            continue;
        }
        out.push(Level {
            price: parse_fixed(price, scales.price)?,
            qty: parse_fixed(qty, scales.qty)?,
        });
    }
    Ok(out)
}

/// Which endpoint serves a channel.
///
/// Not one endpoint after all. Most public channels live on `/public`,
/// but the complete trade feed is on `/business` — subscribing to it on
/// the wrong one is refused with `doesn't exist`, which reads as a
/// channel that was never there rather than one served elsewhere.
fn endpoint(channel: &str) -> &'static str {
    match channel {
        "trades-all" => "wss://ws.okx.com:8443/ws/v5/business",
        _ => "wss://ws.okx.com:8443/ws/v5/public",
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
    // Linear quotes only. `BTCUSD` on this venue is `BTC-USD-SWAP`,
    // which is the *inverse* contract: margined in the base asset and
    // sized in USD contracts rather than in coin. Translating it here
    // would put a different kind of instrument under an archive path
    // that looks like every other one, and nothing downstream would
    // notice until the sizes were used. An untranslated symbol is
    // rejected by the venue with a message naming it; a wrong
    // translation is accepted and captured.
    for quote in ["USDT", "USDC"] {
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

    /// The complete trade feed is served from a different endpoint than
    /// the book. Asking for it on `/public` is refused with `doesn't
    /// exist`, which reads as a channel that was never there.
    #[test]
    fn the_complete_trade_feed_comes_from_the_business_endpoint() {
        let specs = OkxSwap.streams("BTCUSDT");
        let trade = specs.iter().find(|s| s.name == "trade").expect("trade");
        let t = OkxSwap.transport(trade);
        assert_eq!(t.url, "wss://ws.okx.com:8443/ws/v5/business");
        let frame = String::from_utf8(t.subscribe[0].clone()).expect("utf8");
        assert!(
            frame.contains(r#""channel":"trades-all""#),
            "the plain trades channel drops fills by design: {frame}"
        );

        let depth = specs.iter().find(|s| s.name == "depth").expect("depth");
        assert_eq!(
            OkxSwap.transport(depth).url,
            "wss://ws.okx.com:8443/ws/v5/public",
            "the book stays on the public endpoint"
        );
    }

    /// Both trade channels must still be recognised as trades — the
    /// gate is a prefix, and a gate written for `"trades"` exactly would
    /// reject every `trades-all` payload as though it were not a trade.
    #[test]
    fn a_trades_all_payload_is_still_a_trade() {
        let scales = Scales { price: 1, qty: 2 };
        let payload = br#"{"arg":{"channel":"trades-all","instId":"BTC-USDT-SWAP"},"data":[{"tradeId":"1","px":"62981.8","sz":"0.05","side":"buy","ts":"1786881328502"}]}"#;
        assert!(
            OkxSwap.parse_trade(payload, scales).is_some(),
            "trades-all must parse as a trade"
        );
        assert_eq!(OkxSwap.trade_ids(payload), vec![1]);
    }

    /// Several trades in one frame all count. Taking only the first
    /// would report every other trade in that frame as missing.
    #[test]
    fn every_trade_in_a_batched_frame_is_counted() {
        let payload = br#"{"arg":{"channel":"trades-all"},"data":[{"tradeId":"10","px":"1"},{"tradeId":"11","px":"1"},{"tradeId":"12","px":"1"}]}"#;
        assert_eq!(OkxSwap.trade_ids(payload), vec![10, 11, 12]);
    }

    /// The other venue's shape yields nothing here, and this venue's
    /// yields nothing there. That is the whole reason the reader belongs
    /// to the venue: neither errors, both quietly find no ids, and no
    /// ids means no gaps among them.
    #[test]
    fn neither_venue_reads_the_other_venue_ids() {
        let okx = br#"{"data":[{"tradeId":"2836635170"}]}"#;
        let binance = br#"{"e":"trade","t":12345,"p":"1"}"#;
        assert_eq!(OkxSwap.trade_ids(okx), vec![2_836_635_170]);
        assert!(OkxSwap.trade_ids(binance).is_empty());
        assert_eq!(
            super::super::binance::BinancePerp.trade_ids(binance),
            vec![12345]
        );
        assert!(
            super::super::binance::BinancePerp.trade_ids(okx).is_empty(),
            "a quoted id must not be read as a bare one"
        );
    }

    /// The correlation id is alphanumeric only. Reusing a hyphenated
    /// channel name as one is refused with `60033 Parameter id error`,
    /// which names a parameter rather than the id and reads as though
    /// the channel were wrong.
    #[test]
    fn a_hyphenated_channel_does_not_become_a_hyphenated_id() {
        let specs = OkxSwap.streams("BTCUSDT");
        let mark = specs
            .iter()
            .find(|s| s.name == "markPrice")
            .expect("mark price is captured");
        let frame = String::from_utf8(OkxSwap.transport(mark).subscribe[0].clone()).expect("utf8");
        assert!(
            frame.contains(r#""channel":"mark-price""#),
            "the channel keeps its hyphen: {frame}"
        );
        assert!(
            frame.contains(r#""id":"markprice""#),
            "the id must not: {frame}"
        );
    }

    /// The margin engine's inputs have to be in the archive, or a day
    /// captured here and a day captured at the other venue are not the
    /// same thing. Liquidation is computed against mark price, and
    /// funding is a cash flow a backtest has to pay.
    #[test]
    fn mark_price_and_funding_are_captured_like_the_other_venue_polls_them() {
        let specs = OkxSwap.streams("BTCUSDT");
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"markPrice"), "got {names:?}");
        assert!(names.contains(&"fundingRate"), "got {names:?}");
    }

    #[test]
    fn symbols_are_translated_to_this_venue_and_left_alone_if_already_translated() {
        assert_eq!(instrument_id("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(instrument_id("ethusdt"), "ETH-USDT-SWAP");
        assert_eq!(instrument_id("BTC-USDT-SWAP"), "BTC-USDT-SWAP");
    }

    /// `BTC-USD-SWAP` is the inverse contract: margined in the base
    /// asset and sized in USD contracts. Translating `BTCUSD` into it
    /// would file a different kind of instrument under a path that looks
    /// like every other one, and nothing downstream would notice until
    /// the sizes were used. Left untranslated, the venue refuses it and
    /// says which symbol it refused.
    #[test]
    fn a_usd_suffix_is_not_translated_into_the_inverse_contract() {
        assert_eq!(instrument_id("BTCUSD"), "BTCUSD");
        assert_ne!(instrument_id("BTCUSD"), "BTC-USD-SWAP");
        // The linear quotes still translate, and the longer suffix is
        // matched first so BTCUSDT does not fall through to USD.
        assert_eq!(instrument_id("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(instrument_id("BTCUSDC"), "BTC-USDC-SWAP");
    }

    #[test]
    fn precision_is_found_through_the_translated_symbol() {
        // The archive and every other venue call it BTCUSDT; the table
        // is keyed by BTC-USDT-SWAP. A lookup that skipped the
        // translation would find nothing and the tools would refuse to
        // convert data they can read perfectly well.
        let i = OkxSwap.instrument("BTCUSDT").expect("listed");
        assert_eq!((i.price_scale, i.qty_scale), (1, 2));
        assert_eq!(OkxSwap.instrument("btcusdt"), OkxSwap.instrument("BTCUSDT"));
    }

    #[test]
    fn an_unknown_instrument_is_none_rather_than_a_guess() {
        assert!(OkxSwap.instrument("NOTLISTEDUSDT").is_none());
    }

    #[test]
    fn the_id_matches_the_registry_key() {
        assert_eq!(OkxSwap.id(), "okx-swap");
        assert_eq!(super::super::by_id("okx-swap").unwrap().id(), "okx-swap");
    }
}
