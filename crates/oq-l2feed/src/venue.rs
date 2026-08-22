//! Venue-specific knowledge: stream names and where the exchange
//! timestamp lives inside a payload.
//!
//! Deliberately thin. The capture path stores payloads verbatim, so the
//! only thing it needs from a venue is enough parsing to answer "which
//! UTC day does this belong to". Everything else is the consumer's
//! problem, and every field parsed here is a field that could be parsed
//! wrong at capture time and lost forever.

/// A market data stream to subscribe to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSpec {
    /// Name used in the archive path, e.g. `depth`.
    pub name: String,
    /// Venue subscription topic, e.g. `btcusdt@depth@0ms`.
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

/// The streams the capture plan calls for, for one symbol.
///
/// Order matters only for readability. The set is the one in
/// `docs/CAPTURE-FORMAT.md`: incremental depth and best bid/offer for
/// the book and the queue model, aggregated trades for what consumes
/// the queue ahead of you, mark price for the margin engine, and forced
/// liquidations for tail behaviour.
#[must_use]
pub fn binance_perp_streams(symbol: &str) -> Vec<StreamSpec> {
    let lower = symbol.to_lowercase();
    vec![
        StreamSpec::new("depth", format!("{lower}@depth@0ms")),
        StreamSpec::new("bookTicker", format!("{lower}@bookTicker")),
        StreamSpec::new("aggTrade", format!("{lower}@aggTrade")),
        StreamSpec::new("markPrice", format!("{lower}@markPrice@1s")),
        StreamSpec::new("forceOrder", format!("{lower}@forceOrder")),
    ]
}

/// WebSocket URL for a single stream on Binance USD-M futures.
#[must_use]
pub fn binance_perp_url(topic: &str) -> String {
    format!("wss://fstream.binance.com/ws/{topic}")
}

/// REST endpoint for the order book snapshot that re-establishes state
/// after a reconnect.
#[must_use]
pub fn binance_perp_snapshot_url(symbol: &str, limit: u32) -> String {
    format!(
        "https://fapi.binance.com/fapi/v1/depth?symbol={}&limit={limit}",
        symbol.to_uppercase()
    )
}

/// Extract the exchange event time from a payload, in nanoseconds.
///
/// Returns `None` when the payload carries no event time, in which case
/// the caller falls back to local time for day attribution.
///
/// The scan is deliberately crude — find the `"E":` key and read the
/// integer after it — because the payload is stored verbatim regardless
/// and this value only decides which file the record lands in. A parser
/// sophisticated enough to be wrong in interesting ways would be a
/// worse trade: getting the day wrong is recoverable by re-sorting the
/// archive, getting the bytes wrong is not.
#[must_use]
pub fn binance_event_time_ns(payload: &[u8]) -> Option<i64> {
    let millis = find_int_field(payload, b"\"E\":")?;
    millis.checked_mul(1_000_000)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_set_matches_the_capture_plan() {
        let streams = binance_perp_streams("BTCUSDT");
        let names: Vec<_> = streams.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["depth", "bookTicker", "aggTrade", "markPrice", "forceOrder"]
        );
        assert_eq!(streams[0].topic, "btcusdt@depth@0ms");
        assert!(binance_perp_url(&streams[0].topic).starts_with("wss://"));
        assert!(binance_perp_snapshot_url("btcusdt", 1000).contains("symbol=BTCUSDT"));
    }

    #[test]
    fn extracts_the_event_time_from_a_depth_update() {
        let payload = br#"{"e":"depthUpdate","E":1786780800123,"T":1786780800120,"s":"BTCUSDT","U":1,"u":2,"b":[["1.0","2.0"]],"a":[]}"#;
        assert_eq!(
            binance_event_time_ns(payload),
            Some(1_786_780_800_123_000_000)
        );
    }

    #[test]
    fn extracts_from_a_combined_stream_wrapper() {
        let payload = br#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1786780800999,"s":"BTCUSDT","p":"1.0"}}"#;
        assert_eq!(
            binance_event_time_ns(payload),
            Some(1_786_780_800_999_000_000)
        );
    }

    #[test]
    fn missing_or_malformed_event_time_is_none_not_a_guess() {
        assert_eq!(binance_event_time_ns(br#"{"result":null,"id":1}"#), None);
        assert_eq!(binance_event_time_ns(br#"{"E":}"#), None);
        assert_eq!(binance_event_time_ns(b""), None);
        // A key that appears only inside a string value must not be
        // mistaken for the field; the scan skips it and finds nothing.
        assert_eq!(
            binance_event_time_ns(br#"{"msg":"\"E\": not a field"}"#),
            None
        );
    }

    #[test]
    fn keeps_scanning_past_a_non_numeric_match() {
        let payload = br#"{"a":{"E":"x"},"E":1786780800001}"#;
        assert_eq!(
            binance_event_time_ns(payload),
            Some(1_786_780_800_001_000_000)
        );
    }
}
