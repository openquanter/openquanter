//! Binance USDT-M futures, read side only.
//!
//! Every request here is a GET against an endpoint that reports state.
//! There is no POST and no DELETE, so this cannot open, close, or cancel
//! anything — the boundary is structural rather than a rule someone has
//! to remember.
//!
//! ## Signing
//!
//! Signed endpoints take the query string, HMAC-SHA256 it with the API
//! secret, and carry the result as a `signature` parameter. The signature
//! covers the string *exactly as sent*, so the query is built once and
//! both signed and transmitted from that same string — building it twice
//! is how a signature ends up valid for a request that was not made.
//!
//! ## Timestamps
//!
//! Signed requests carry a millisecond timestamp and are rejected if it
//! is too far from the venue's clock. The offset against the venue is
//! measured once and applied to every request, because a machine whose
//! clock drifts a second produces failures that read like authentication
//! problems.

use oq_hash::hmac::hmac_sha256_hex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::creds::Credentials;

/// Anything that stops a read from producing an answer.
#[derive(Debug)]
pub enum VenueError {
    /// The request never completed.
    Transport(String),
    /// The venue answered, and the answer was a refusal.
    Venue { status: u16, body: String },
    /// The venue answered with something this code cannot read.
    Malformed { what: &'static str, body: String },
}

impl core::fmt::Display for VenueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Venue { status, body } => write!(f, "venue returned {status}: {body}"),
            Self::Malformed { what, body } => {
                write!(f, "could not read {what} from response: {body}")
            }
        }
    }
}

impl core::error::Error for VenueError {}

/// What the account holds, as the venue reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    /// Wallet balance in the margin asset, in the venue's own units.
    pub wallet_balance: f64,
    /// Unrealized profit across all positions.
    pub unrealized: f64,
    /// Margin balance: wallet plus unrealized.
    pub margin_balance: f64,
    /// When this was read, milliseconds since the epoch.
    pub read_at_ms: i64,
}

/// One position leg as the venue reports it.
///
/// Hedge accounts report a long and a short separately, each with its own
/// entry — which is the same distinction the engine's position mode
/// makes, and the reason a reconciler can compare them at all.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionSnapshot {
    pub symbol: String,
    /// `BOTH` under one-way netting, `LONG` or `SHORT` under hedging.
    pub position_side: String,
    /// Signed: positive long, negative short.
    pub amount: f64,
    pub entry_price: f64,
    pub unrealized: f64,
}

/// A resting order.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    pub symbol: String,
    pub order_id: i64,
    pub client_order_id: String,
    pub side: String,
    pub position_side: String,
    pub price: f64,
    pub orig_qty: f64,
    pub executed_qty: f64,
    pub status: String,
}

/// One execution against the account.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub symbol: String,
    pub id: i64,
    pub order_id: i64,
    pub side: String,
    pub position_side: String,
    pub price: f64,
    pub qty: f64,
    pub realized_pnl: f64,
    pub commission: f64,
    pub time_ms: i64,
    pub maker: bool,
}

/// A read-only client.
pub struct Binance {
    base: String,
    creds: Credentials,
    agent: ureq::Agent,
    /// Venue clock minus local clock, in milliseconds.
    clock_offset_ms: i64,
}

impl Binance {
    /// Mainnet USDT-M futures.
    pub const MAINNET: &'static str = "https://fapi.binance.com";
    /// The testnet, which is where anything new should be pointed first.
    pub const TESTNET: &'static str = "https://testnet.binancefuture.com";

    /// Build a client against `base`.
    #[must_use]
    pub fn new(base: impl Into<String>, creds: Credentials) -> Self {
        // Generous, because the alternative is worse. A read that times
        // out is indistinguishable from a read that failed, and a watch
        // treats an unreadable answer as "nothing compared" — so a tight
        // timeout on a slow link produces silence that looks like a
        // quiet account. Measured on the link this runs over, the same
        // request took 0.7 s and 4.4 s a second apart.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(45)))
            // A refusal is read as a response rather than raised as an
            // error, because the error variant carries only the status
            // and the body is the half that says why. `-2015 Invalid
            // API-key, IP, or permissions for action` and `-1021
            // Timestamp for this request is outside of the recvWindow`
            // are both 401s, and the difference between them is an
            // afternoon.
            .http_status_as_error(false)
            .build();
        Self {
            base: base.into(),
            creds,
            agent: config.into(),
            clock_offset_ms: 0,
        }
    }

    /// Measure the venue's clock against this machine's.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn sync_clock(&mut self) -> Result<i64, VenueError> {
        let body = self.get_public("/fapi/v1/time", "")?;
        let venue_ms = field_i64(&body, "serverTime").ok_or_else(|| VenueError::Malformed {
            what: "serverTime",
            body: body.clone(),
        })?;
        self.clock_offset_ms = venue_ms - now_ms();
        Ok(self.clock_offset_ms)
    }

    /// The measured offset, for a caller that wants to report it.
    #[must_use]
    pub const fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
    }

    /// Account balance and unrealized profit.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn account(&self) -> Result<AccountSnapshot, VenueError> {
        let body = self.get_signed("/fapi/v2/account", "")?;
        let read_at_ms = now_ms() + self.clock_offset_ms;
        Ok(AccountSnapshot {
            wallet_balance: need_f64(&body, "totalWalletBalance")?,
            unrealized: need_f64(&body, "totalUnrealizedProfit")?,
            margin_balance: need_f64(&body, "totalMarginBalance")?,
            read_at_ms,
        })
    }

    /// Every position leg the venue reports for `symbol`.
    ///
    /// Legs with no size are dropped: the venue reports them for every
    /// instrument it knows, and a reconciler comparing "what is open"
    /// against a list of mostly-zeroes is comparing the wrong thing.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn positions(&self, symbol: &str) -> Result<Vec<PositionSnapshot>, VenueError> {
        let body = self.get_signed("/fapi/v2/positionRisk", &format!("symbol={symbol}"))?;
        let mut out = Vec::new();
        for o in objects(&body) {
            let amount = need_f64(&o, "positionAmt")?;
            if amount == 0.0 {
                continue;
            }
            out.push(PositionSnapshot {
                symbol: need_str(&o, "symbol")?,
                // One-way accounts omit the side, and `BOTH` is what the
                // venue calls that. A default here is a translation, not
                // a guess at a missing number.
                position_side: field_str(&o, "positionSide").unwrap_or_else(|| "BOTH".into()),
                amount,
                entry_price: need_f64(&o, "entryPrice")?,
                unrealized: need_f64(&o, "unRealizedProfit")?,
            });
        }
        Ok(out)
    }

    /// Orders currently resting for `symbol`.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, VenueError> {
        let body = self.get_signed("/fapi/v1/openOrders", &format!("symbol={symbol}"))?;
        objects(&body)
            .into_iter()
            .map(|o| {
                Ok(OpenOrder {
                    symbol: need_str(&o, "symbol")?,
                    order_id: need_i64(&o, "orderId")?,
                    // The key a reconciler matches on. An empty default
                    // here is an id that matches nothing and reads as an
                    // order the venue never mentioned.
                    client_order_id: need_str(&o, "clientOrderId")?,
                    side: need_str(&o, "side")?,
                    position_side: field_str(&o, "positionSide").unwrap_or_else(|| "BOTH".into()),
                    price: need_f64(&o, "price")?,
                    orig_qty: need_f64(&o, "origQty")?,
                    executed_qty: need_f64(&o, "executedQty")?,
                    status: need_str(&o, "status")?,
                })
            })
            .collect()
    }

    /// Executions against the account, most recent last.
    ///
    /// `since_ms` bounds the window; the venue caps how far back a single
    /// call reaches, so a caller rebuilding history pages forward rather
    /// than asking for everything.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn my_trades(&self, symbol: &str, since_ms: Option<i64>) -> Result<Vec<Trade>, VenueError> {
        let mut query = format!("symbol={symbol}&limit=1000");
        if let Some(t) = since_ms {
            query.push_str(&format!("&startTime={t}"));
        }
        let body = self.get_signed("/fapi/v1/userTrades", &query)?;
        objects(&body)
            .into_iter()
            .map(|o| {
                Ok(Trade {
                    symbol: need_str(&o, "symbol")?,
                    id: need_i64(&o, "id")?,
                    order_id: need_i64(&o, "orderId")?,
                    side: need_str(&o, "side")?,
                    position_side: field_str(&o, "positionSide").unwrap_or_else(|| "BOTH".into()),
                    price: need_f64(&o, "price")?,
                    qty: need_f64(&o, "qty")?,
                    realized_pnl: need_f64(&o, "realizedPnl")?,
                    commission: need_f64(&o, "commission")?,
                    time_ms: need_i64(&o, "time")?,
                    maker: need_bool(&o, "maker")?,
                })
            })
            .collect()
    }

    fn get_public(&self, path: &str, query: &str) -> Result<String, VenueError> {
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        self.send(&url, false)
    }

    /// Sign and send. The signed string and the transmitted string are
    /// the same object, never rebuilt — a signature over a query that
    /// differs from the one sent is valid for a request nobody made.
    fn get_signed(&self, path: &str, query: &str) -> Result<String, VenueError> {
        let url = signed_url(
            &self.base,
            path,
            query,
            now_ms() + self.clock_offset_ms,
            self.creds.secret_bytes(),
        );
        self.send(&url, true)
    }

    fn send(&self, url: &str, signed: bool) -> Result<String, VenueError> {
        let mut req = self.agent.get(url);
        if signed {
            req = req.header("X-MBX-APIKEY", self.creds.key());
        }
        match req.call() {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let body = resp
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| VenueError::Transport(e.to_string()))?;
                if (200..300).contains(&status) {
                    return Ok(body);
                }
                // The venue's own words, which name the cause. The URL is
                // deliberately absent: it carries the signature, and an
                // error is the line most likely to be pasted somewhere
                // else. The response body carries neither.
                Err(VenueError::Venue {
                    status,
                    body: format!("{} — {}", redact(url), body.trim()),
                })
            }
            Err(e) => Err(VenueError::Transport(e.to_string())),
        }
    }
}

/// Build the full signed URL.
///
/// Pure, and separate from the request, so the one invariant that matters
/// here can be asserted without a network: the bytes the signature covers
/// are the bytes that get sent. Rebuilding the query after signing it
/// produces a signature that is valid for a request nobody made, and the
/// venue's rejection says only that the signature was wrong.
fn signed_url(base: &str, path: &str, query: &str, stamp_ms: i64, secret: &[u8]) -> String {
    let stamped = if query.is_empty() {
        format!("timestamp={stamp_ms}&recvWindow=5000")
    } else {
        format!("{query}&timestamp={stamp_ms}&recvWindow=5000")
    };
    let signature = hmac_sha256_hex(secret, stamped.as_bytes());
    format!("{base}{path}?{stamped}&signature={signature}")
}

/// The path of a URL, without the query — which is where the signature
/// and the API key live.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or("<url>")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// A hand-written scan rather than a JSON dependency, for the same reason
// the hashes here are hand-written: the shapes are flat and known, and a
// dependency in the path that reads an account's positions is a
// dependency that has to be trusted with them.

/// Split a JSON array into its top-level objects.
fn objects(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(s) = start.take()
                {
                    out.push(body[s..=i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// The raw text following `"key":`, quoted or not.
///
/// Escapes are honoured for the same reason [`objects`] honours them: the
/// two functions read the same bytes, and a value that ends at a
/// different place depending on which one is looking is a value that gets
/// silently truncated. `clientOrderId` is the field a reconciler matches
/// on, so a truncated one is an order that appears to have vanished.
fn raw_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = body.find(&needle)? + needle.len();
    let rest = body[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(inner) = rest.strip_prefix('"') {
        return unescape_until_quote(inner);
    }
    let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Read a JSON string body up to its closing quote, resolving the escapes
/// a venue actually emits.
///
/// Not a general JSON string reader: `\u` sequences are left as written
/// because nothing in these responses uses them, and inventing a decoder
/// for a case that does not arise is how a parser gets to be wrong in
/// interesting ways.
fn unescape_until_quote(inner: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            _ => out.push(c),
        }
    }
    // No closing quote: the response was cut short, which is a malformed
    // answer rather than a value that happens to run to the end.
    None
}

/// Read a field a comparison depends on, or name the one that was absent.
///
/// The alternative is a default, and a default is what turns a changed
/// response into a changed account: `unwrap_or(0.0)` on a balance yields
/// a number no different from an empty account, and zero is a value a
/// risk gate acts on rather than stops at.
fn need_f64(body: &str, key: &'static str) -> Result<f64, VenueError> {
    field_f64(body, key).ok_or_else(|| malformed(key, body))
}

fn need_i64(body: &str, key: &'static str) -> Result<i64, VenueError> {
    field_i64(body, key).ok_or_else(|| malformed(key, body))
}

fn need_str(body: &str, key: &'static str) -> Result<String, VenueError> {
    field_str(body, key).ok_or_else(|| malformed(key, body))
}

fn need_bool(body: &str, key: &'static str) -> Result<bool, VenueError> {
    field_bool(body, key).ok_or_else(|| malformed(key, body))
}

/// Enough of the response to identify what arrived, and not a megabyte of
/// it: a thousand-fill page in an error message is a message nobody reads.
fn malformed(what: &'static str, body: &str) -> VenueError {
    const LIMIT: usize = 512;
    let mut shown: String = body.chars().take(LIMIT).collect();
    if body.chars().nth(LIMIT).is_some() {
        shown.push('…');
    }
    VenueError::Malformed { what, body: shown }
}

fn field_str(body: &str, key: &str) -> Option<String> {
    raw_field(body, key)
}

fn field_f64(body: &str, key: &str) -> Option<f64> {
    raw_field(body, key)?.parse().ok()
}

fn field_i64(body: &str, key: &str) -> Option<i64> {
    raw_field(body, key)?.parse().ok()
}

fn field_bool(body: &str, key: &str) -> Option<bool> {
    match raw_field(body, key)?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITIONS: &str = r#"[
      {"symbol":"BTCUSDT","positionAmt":"0.256","entryPrice":"71444.87","positionSide":"LONG","unRealizedProfit":"-2197.75"},
      {"symbol":"BTCUSDT","positionAmt":"-0.004","entryPrice":"62820.40","positionSide":"SHORT","unRealizedProfit":"-0.16"},
      {"symbol":"BTCUSDT","positionAmt":"0.000","entryPrice":"0.0","positionSide":"BOTH","unRealizedProfit":"0"}
    ]"#;

    #[test]
    fn both_legs_of_a_hedged_position_are_read() {
        let parsed: Vec<_> = objects(POSITIONS)
            .into_iter()
            .filter_map(|o| {
                let amount = field_f64(&o, "positionAmt")?;
                if amount == 0.0 {
                    return None;
                }
                Some((field_str(&o, "positionSide")?, amount))
            })
            .collect();
        assert_eq!(
            parsed,
            vec![("LONG".to_string(), 0.256), ("SHORT".to_string(), -0.004)]
        );
    }

    /// The venue reports a flat leg for every instrument it knows about.
    /// Keeping them turns "what is open" into a list of mostly zeroes and
    /// makes a reconciler's diff meaningless.
    #[test]
    fn flat_legs_are_dropped() {
        assert_eq!(objects(POSITIONS).len(), 3, "three legs in the payload");
        let open = objects(POSITIONS)
            .into_iter()
            .filter(|o| field_f64(o, "positionAmt").unwrap_or(0.0) != 0.0)
            .count();
        assert_eq!(open, 2);
    }

    #[test]
    fn numbers_arrive_as_strings_and_are_read_as_numbers() {
        let one = &objects(POSITIONS)[0];
        assert_eq!(field_f64(one, "entryPrice"), Some(71_444.87));
        assert_eq!(field_f64(one, "unRealizedProfit"), Some(-2_197.75));
    }

    #[test]
    fn a_brace_inside_a_string_does_not_split_an_object() {
        let body = r#"[{"clientOrderId":"a{b}c","orderId":7},{"clientOrderId":"d","orderId":8}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 2, "got {objs:?}");
        assert_eq!(field_i64(&objs[0], "orderId"), Some(7));
        assert_eq!(field_i64(&objs[1], "orderId"), Some(8));
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let body = r#"[{"clientOrderId":"a\"}b","orderId":9}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 1, "got {objs:?}");
        assert_eq!(field_i64(&objs[0], "orderId"), Some(9));
    }

    #[test]
    fn booleans_are_read() {
        let body = r#"{"maker":true,"buyer":false}"#;
        assert_eq!(field_bool(body, "maker"), Some(true));
        assert_eq!(field_bool(body, "buyer"), Some(false));
    }

    /// The invariant the whole signing path rests on: the signature
    /// covers exactly the query that is transmitted. Signing one string
    /// and sending another yields a request the venue refuses with a
    /// message about signatures, which sends the reader to the key.
    #[test]
    fn the_signature_covers_exactly_the_query_that_is_sent() {
        let url = signed_url(
            "https://fapi.binance.com",
            "/fapi/v1/openOrders",
            "symbol=BTCUSDT",
            1_786_881_328_502,
            b"secret",
        );
        let query = url.split_once('?').expect("a query").1;
        let (signed_part, signature) = query.split_once("&signature=").expect("a signature");
        assert_eq!(
            hmac_sha256_hex(b"secret", signed_part.as_bytes()),
            signature,
            "the transmitted query and the signed query drifted apart"
        );
        assert_eq!(
            signed_part,
            "symbol=BTCUSDT&timestamp=1786881328502&recvWindow=5000"
        );
    }

    /// A request with no parameters of its own still signs the stamp it
    /// sends, rather than falling into a differently-built string.
    #[test]
    fn an_empty_query_signs_what_it_sends_too() {
        let url = signed_url("https://x", "/fapi/v2/account", "", 7, b"k");
        let query = url.split_once('?').expect("a query").1;
        let (signed_part, signature) = query.split_once("&signature=").expect("a signature");
        assert_eq!(hmac_sha256_hex(b"k", signed_part.as_bytes()), signature);
        assert_eq!(signed_part, "timestamp=7&recvWindow=5000");
    }

    /// A field a comparison depends on must fail the read rather than
    /// default. Zero is not a sentinel: an account with a zero balance
    /// and an account whose balance could not be read are the same value
    /// and opposite facts, and a risk gate acts on the first.
    #[test]
    fn a_missing_number_is_an_error_rather_than_zero() {
        let body = r#"{"totalUnrealizedProfit":"1.0"}"#;
        let err = need_f64(body, "totalWalletBalance").expect_err("must not default");
        match err {
            VenueError::Malformed { what, .. } => assert_eq!(what, "totalWalletBalance"),
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(
            need_f64(body, "totalUnrealizedProfit").expect("present"),
            1.0
        );
    }

    /// A renamed or absent field must not shorten the list. An order that
    /// silently drops out of "what is open" is indistinguishable from an
    /// order the venue cancelled, which is the phantom-cancel class the
    /// design document exists to avoid.
    #[test]
    fn an_unreadable_entry_fails_the_read_rather_than_shrinking_the_list() {
        let body = r#"[{"positionAmt":"0.5","symbol":"BTCUSDT","entryPrice":"100.0"}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 1);
        // `unRealizedProfit` is absent; the entry must not simply vanish.
        assert!(need_f64(&objs[0], "unRealizedProfit").is_err());
    }

    /// `objects` already honours escapes. `raw_field` must agree with it,
    /// or a value ends at a different place depending on which function is
    /// looking — and the field this bites is the reconciliation key.
    #[test]
    fn an_escaped_quote_inside_a_value_does_not_truncate_it() {
        let body = r#"{"clientOrderId":"a\"b","orderId":9}"#;
        assert_eq!(field_str(body, "clientOrderId").as_deref(), Some("a\"b"));
        assert_eq!(field_i64(body, "orderId"), Some(9));
    }

    /// A string with no closing quote is a response that was cut short,
    /// not a value that runs to the end of the buffer.
    #[test]
    fn an_unterminated_string_is_not_a_value() {
        assert_eq!(field_str(r#"{"symbol":"BTCUSD"#, "symbol"), None);
    }

    /// The error path must not carry the query string: it holds the
    /// signature and the timestamp, and an error is the line most likely
    /// to be copied somewhere else.
    #[test]
    fn a_failed_request_does_not_report_its_signature() {
        let url = "https://fapi.binance.com/fapi/v2/account?timestamp=1&signature=deadbeef";
        assert_eq!(redact(url), "https://fapi.binance.com/fapi/v2/account");
    }
}
