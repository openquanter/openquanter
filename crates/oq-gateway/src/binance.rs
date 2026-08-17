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
use crate::exec::{
    Endpoint, Execution, NewOrder, OrderAck, OrderUpdate, Placed, PositionSide, Reject, Unresolved,
    UserEvent, UserStream, decimal,
};
use oq_types::{Instrument, Side, TimeInForce};

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

/// Which HTTP verb a request uses.
///
/// Named rather than passed as a string: the difference between a read
/// and a write is the difference this crate is most careful about, and
/// a typo in a string is not a difference the compiler notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Put,
    Delete,
}

/// A client for one deployment of the venue.
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

    /// Build a client against a named deployment.
    ///
    /// Preferred over [`Binance::new`] wherever the choice is between
    /// test and production, because a string that is wrong by one
    /// character is production and an enum cannot be.
    #[must_use]
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Self {
        let base = match endpoint {
            Endpoint::Testnet => Self::TESTNET,
            Endpoint::Live => Self::MAINNET,
        };
        Self::new(base, creds)
    }

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

    /// Now, on the venue's clock.
    ///
    /// The same quantity every snapshot stamps itself with, exposed so
    /// that something which has no snapshot to stamp — a read that
    /// failed — can still be placed on the timeline beside the reads
    /// that succeeded. A log carrying two clocks is harder to reason
    /// about than one carrying a single clock and some gaps.
    #[must_use]
    pub fn venue_time_ms(&self) -> i64 {
        now_ms() + self.clock_offset_ms
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
        self.send_method(Method::Get, url, signed)
    }

    /// Send with an explicit method.
    ///
    /// Split out rather than folded into [`Binance::send`] so that the
    /// crate's read paths keep calling something that can only issue a
    /// GET. A write is a different call, and a reviewer sees it.
    fn send_method(&self, method: Method, url: &str, signed: bool) -> Result<String, VenueError> {
        // Each arm builds and sends in place: the builders are
        // different types per verb, and a POST carries a body where the
        // others do not.
        let key = self.creds.key();
        let sent = match method {
            Method::Get => {
                let mut r = self.agent.get(url);
                if signed {
                    r = r.header("X-MBX-APIKEY", key);
                }
                r.call()
            }
            Method::Delete => {
                let mut r = self.agent.delete(url);
                if signed {
                    r = r.header("X-MBX-APIKEY", key);
                }
                r.call()
            }
            Method::Post => {
                let mut r = self.agent.post(url);
                if signed {
                    r = r.header("X-MBX-APIKEY", key);
                }
                r.send_empty()
            }
            Method::Put => {
                let mut r = self.agent.put(url);
                if signed {
                    r = r.header("X-MBX-APIKEY", key);
                }
                r.send_empty()
            }
        };
        match sent {
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
/// The innermost JSON object containing `needle`.
///
/// Brace-matched outward from the match, honouring strings, so a
/// contract's own definition is returned rather than the array or the
/// document that holds it. [`objects`] cannot do this: exchangeInfo is a
/// single top-level object, so splitting at depth zero yields the whole
/// body, and reading a field from that returns whichever contract
/// happens to be listed first — which is right for exactly one symbol
/// and silently wrong for every other.
fn object_containing(body: &str, needle: &str) -> Option<String> {
    let at = body.find(needle)?;
    let mut depth = 0i32;
    let mut start = None;
    for (i, c) in body[..at].char_indices().rev() {
        match c {
            '}' => depth += 1,
            '{' if depth == 0 => {
                start = Some(i);
                break;
            }
            '{' => depth -= 1,
            _ => {}
        }
    }
    let start = start?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in body[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(body[start..=start + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

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

    /// The stamp a failed read carries has to come off the same clock as
    /// the stamp a successful one carries, or the log records an outage
    /// on a timeline nothing else in it shares.
    #[test]
    fn venue_time_moves_with_the_measured_offset() {
        let mut c = Binance::at(Endpoint::Testnet, Credentials::new("k", "s"));
        let unadjusted = c.venue_time_ms();
        assert!(
            (unadjusted - now_ms()).abs() < 1_000,
            "with no offset it should be local time"
        );

        c.clock_offset_ms = -30_000;
        let adjusted = c.venue_time_ms();
        assert!(
            (adjusted - (unadjusted - 30_000)).abs() < 1_000,
            "expected the offset to apply: {adjusted} vs {unadjusted}"
        );
    }
}

// ---------------------------------------------------------------------
// Order entry.
//
// Kept at the end of the file and behind its own trait implementation
// rather than mixed into the read methods above, because the difference
// between reading an account and moving money is the difference this
// crate is most careful about, and it should be visible in the diff
// that introduces it.
// ---------------------------------------------------------------------

/// What a venue's answer means, decided without a network.
///
/// Separated from the request so the classification can be tested
/// exhaustively against recorded bodies. Getting it wrong is not a
/// visible failure: a refusal read as unknown causes a pointless query,
/// and an unknown read as a refusal causes a duplicate order.
fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    // 5xx is the venue failing to answer, not answering "no". The
    // request may well have been processed before it fell over.
    if (500..600).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("venue returned {status}"),
        });
    }
    // A 4xx carrying the venue's own error code is a decision: the
    // order does not exist, and an identical retry gets an identical
    // refusal.
    if let Some(code) = field_i64(body, "code") {
        return Placed::Rejected(Reject {
            code: Some(code),
            message: field_str(body, "msg").unwrap_or_else(|| body.trim().to_string()),
        });
    }
    // A refusal that does not say why is not a refusal anyone can act
    // on. Treated as unknown, which costs a query and cannot cost a
    // duplicate position.
    Placed::Unknown(Unresolved {
        client_id: client_id.to_string(),
        reason: format!("venue returned {status} without an error code"),
    })
}

/// Read an acknowledgement out of a success body.
///
/// A 2xx whose fields cannot be read is *not* a success: the order
/// exists and this build cannot name it. That is precisely the unknown
/// case, and the client id is how it gets resolved.
fn ack_from(body: &str, client_id: &str) -> Placed {
    match (
        field_i64(body, "orderId"),
        field_str(body, "clientOrderId"),
        field_str(body, "status"),
    ) {
        (Some(venue_id), Some(echoed), Some(status)) => Placed::Accepted(OrderAck {
            venue_id,
            client_id: echoed,
            status,
            executed_qty: field_str(body, "executedQty").unwrap_or_else(|| "0".to_string()),
        }),
        _ => Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: "venue accepted the order but its answer could not be read".to_string(),
        }),
    }
}

/// Binance accepts `^[\.A-Z:/a-z0-9_-]{1,36}$` here, and rejects the
/// rest with a message about the signature rather than about the id.
///
/// Checked before sending rather than after being refused, because an
/// id containing `&` or `=` would not merely be invalid — it would
/// change the meaning of the signed query.
fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 36
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'/' | b'_' | b'-'))
}

/// The query for a new order, without timestamp or signature.
fn order_query(order: &NewOrder, instrument: &Instrument) -> String {
    let side = match order.side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    };
    let qty = decimal(order.qty.0.abs(), instrument.qty_scale);
    // `RESULT` rather than the default `ACK`: the venue then answers
    // with the order's final state, so an order that filled on arrival
    // says so in the response instead of only on the stream. Without
    // it, a fast fill is known to the socket before it is known to the
    // caller that placed it.
    let mut q = format!(
        "symbol={}&side={side}&quantity={qty}&newClientOrderId={}&newOrderRespType=RESULT",
        order.symbol, order.client_id
    );
    match order.limit_price {
        Some(price) => {
            let tif = match order.tif {
                TimeInForce::GoodTilCancel => "GTC",
                TimeInForce::ImmediateOrCancel => "IOC",
                TimeInForce::FillOrKill => "FOK",
            };
            q.push_str(&format!(
                "&type=LIMIT&price={}&timeInForce={tif}",
                decimal(price.0, instrument.price_scale)
            ));
        }
        None => q.push_str("&type=MARKET"),
    }
    match order.position_side {
        PositionSide::OneWay => {
            // Only meaningful on an account holding one net position.
            // On a hedged one the venue refuses it, which is why the
            // two are checked against each other before sending.
            if order.reduce_only {
                q.push_str("&reduceOnly=true");
            }
        }
        PositionSide::Long => q.push_str("&positionSide=LONG"),
        PositionSide::Short => q.push_str("&positionSide=SHORT"),
    }
    q
}

impl Execution for Binance {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        if !valid_client_id(&order.client_id) {
            // Refused here rather than by the venue. Sending it would
            // not merely fail: an id containing `&` or `=` rewrites the
            // query that gets signed.
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable: 1-36 characters of [A-Za-z0-9._:/-]",
                    order.client_id
                ),
            });
        }
        if let Some(price) = order.limit_price {
            if !instrument.price_on_grid(price) {
                // The venue answers this with "Price not increased by
                // tick size", a sentence that only makes sense once you
                // know precision and grid are different numbers. Caught
                // here so the message names the actual problem, and so
                // a price is never quietly moved on the caller's behalf.
                return Placed::Rejected(Reject {
                    code: None,
                    message: format!(
                        "price {} is not a multiple of the tick size ({} in units of \
                         1e-{}); snap it deliberately rather than having it moved",
                        decimal(price.0, instrument.price_scale),
                        instrument.price_tick,
                        instrument.price_scale
                    ),
                });
            }
        }
        if !instrument.qty_on_grid(order.qty) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "quantity {} is not a multiple of the step size ({} in units of \
                     1e-{})",
                    decimal(order.qty.0, instrument.qty_scale),
                    instrument.qty_step,
                    instrument.qty_scale
                ),
            });
        }
        if order.reduce_only && order.position_side.is_hedged() {
            // The venue refuses this combination, and its message names
            // the position side rather than the conflict. Caught here
            // so the answer arrives from the layer that can explain it.
            return Placed::Rejected(Reject {
                code: None,
                message: "reduceOnly and a hedged position side are mutually exclusive: \
                          a hedged account expresses a close by naming the leg"
                    .to_string(),
            });
        }
        let url = signed_url(
            &self.base,
            "/fapi/v1/order",
            &order_query(order, instrument),
            now_ms() + self.clock_offset_ms,
            self.creds.secret_bytes(),
        );
        match self.send_method(Method::Post, &url, true) {
            Ok(body) => ack_from(&body, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        let url = signed_url(
            &self.base,
            "/fapi/v1/order",
            &format!("symbol={symbol}&origClientOrderId={client_id}"),
            now_ms() + self.clock_offset_ms,
            self.creds.secret_bytes(),
        );
        match self.send_method(Method::Delete, &url, true) {
            Ok(body) => ack_from(&body, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(&self, symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let body = match self.get_signed(
            "/fapi/v1/order",
            &format!("symbol={symbol}&origClientOrderId={client_id}"),
        ) {
            Ok(b) => b,
            // -2013 is the venue saying it has never heard of this id,
            // which after an unknown placement is the answer that the
            // order never landed. Any other refusal is a real error and
            // must not be read as "no such order".
            Err(VenueError::Venue { status, body }) if field_i64(&body, "code") == Some(-2013) => {
                let _ = status;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        match ack_from(&body, client_id) {
            Placed::Accepted(ack) => Ok(Some(ack)),
            _ => Err(malformed("order status", &body)),
        }
    }
}

#[cfg(test)]
mod order_entry {
    use super::*;
    use oq_types::{PriceTicks, QtyLots};

    fn btc() -> Instrument {
        // 0.01 USDT price steps, 0.001 BTC quantity steps.
        Instrument::linear(2, 3)
    }

    fn limit() -> NewOrder {
        NewOrder {
            symbol: "BTCUSDT".into(),
            side: Side::Buy,
            limit_price: Some(PriceTicks(12_000_000)),
            qty: QtyLots(2),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq-1".into(),
            reduce_only: false,
            position_side: PositionSide::OneWay,
        }
    }

    #[test]
    fn a_server_error_is_unknown_because_it_may_have_been_processed() {
        // The failure that produces duplicate positions if read as a
        // refusal: the venue fell over, possibly after accepting.
        let p = classify(502, "Bad Gateway", "oq-1");
        assert!(matches!(p, Placed::Unknown(_)), "got {p:?}");
    }

    #[test]
    fn a_refusal_that_names_its_code_is_final() {
        let body = r#"{"code":-2019,"msg":"Margin is insufficient."}"#;
        match classify(400, body, "oq-1") {
            Placed::Rejected(r) => {
                assert_eq!(r.code, Some(-2019));
                assert_eq!(r.message, "Margin is insufficient.");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_with_no_code_is_unknown_rather_than_assumed_final() {
        // Costs a query. Assuming it final costs a live order nobody
        // is tracking.
        let p = classify(400, "<html>gateway timeout</html>", "oq-1");
        assert!(matches!(p, Placed::Unknown(_)), "got {p:?}");
    }

    #[test]
    fn an_acceptance_is_read_into_an_ack() {
        let body =
            r#"{"orderId":283194212,"clientOrderId":"oq-1","status":"NEW","executedQty":"0.000"}"#;
        match ack_from(body, "oq-1") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, 283_194_212);
                assert_eq!(a.client_id, "oq-1");
                assert_eq!(a.status, "NEW");
                assert_eq!(a.executed_qty, "0.000");
            }
            other => panic!("expected an acceptance, got {other:?}"),
        }
    }

    #[test]
    fn a_success_that_cannot_be_read_is_unknown_not_success() {
        // The order exists and this build cannot name it. Reporting
        // success would lose it; reporting failure would duplicate it.
        let p = ack_from(r#"{"orderId":283194212}"#, "oq-1");
        assert!(matches!(p, Placed::Unknown(_)), "got {p:?}");
    }

    #[test]
    fn a_limit_order_carries_its_price_and_time_in_force() {
        let q = order_query(&limit(), &btc());
        assert!(q.contains("type=LIMIT"), "{q}");
        assert!(q.contains("price=120000.00"), "{q}");
        assert!(q.contains("timeInForce=GTC"), "{q}");
        assert!(q.contains("quantity=0.002"), "{q}");
        assert!(q.contains("newClientOrderId=oq-1"), "{q}");
        assert!(!q.contains("reduceOnly"), "{q}");
    }

    #[test]
    fn a_market_order_names_no_price_at_all() {
        // Not a price of zero. A zero that reaches a venue as a price
        // is an order to buy at nothing.
        let mut o = limit();
        o.limit_price = None;
        let q = order_query(&o, &btc());
        assert!(q.contains("type=MARKET"), "{q}");
        assert!(!q.contains("price="), "{q}");
        assert!(!q.contains("timeInForce"), "{q}");
    }

    #[test]
    fn a_short_sends_a_positive_quantity_and_a_sell_side() {
        // The sign lives in the side. A negative quantity in the query
        // is refused by the venue with a message about the quantity,
        // which is a long way from the code that produced the sign.
        let mut o = limit();
        o.side = Side::Sell;
        o.qty = QtyLots(-2);
        let q = order_query(&o, &btc());
        assert!(q.contains("side=SELL"), "{q}");
        assert!(q.contains("quantity=0.002"), "{q}");
    }

    #[test]
    fn a_client_id_that_could_rewrite_the_query_is_refused_before_sending() {
        // `&` and `=` are the characters that matter: an id carrying
        // them does not produce an invalid request, it produces a
        // different one — and the signature covers the different one.
        for bad in ["oq&quantity=99", "oq=1", "", &"x".repeat(37)] {
            assert!(!valid_client_id(bad), "{bad:?} must not be accepted");
        }
        for good in ["oq-1", "oq_1", "a.b:c/d", &"y".repeat(36)] {
            assert!(valid_client_id(good), "{good:?} must be accepted");
        }
    }

    #[test]
    fn the_signature_covers_the_order_parameters() {
        // The invariant the read paths already assert, restated for the
        // path that moves money: the bytes signed are the bytes sent,
        // so a quantity cannot be altered after signing.
        let url = signed_url(
            "https://example.test",
            "/fapi/v1/order",
            &order_query(&limit(), &btc()),
            1_700_000_000_000,
            b"secret",
        );
        let (before_sig, sig) = url.split_once("&signature=").expect("signature present");
        let query = before_sig.split_once('?').expect("query present").1;
        assert_eq!(sig, hmac_sha256_hex(b"secret", query.as_bytes()));
        assert!(query.contains("quantity=0.002"), "{query}");
    }
}

// ---------------------------------------------------------------------
// User data stream.
//
// The key is fetched over HTTPS and the events arrive over a websocket,
// which is the shape the venue forces: nothing can be sent on the
// socket, and nothing can be heard without it.
// ---------------------------------------------------------------------

impl Binance {
    /// Mainnet user data stream host.
    pub const MAINNET_STREAM: &'static str = "wss://fstream.binance.com";
    /// Testnet user data stream host.
    pub const TESTNET_STREAM: &'static str = "wss://stream.binancefuture.com";

    /// The stream host matching this client's REST base.
    ///
    /// Derived rather than configured, so a client pointed at the
    /// testnet cannot end up listening to production — a mismatch that
    /// would show as an account that never trades while orders fill.
    #[must_use]
    pub fn stream_host(&self) -> &'static str {
        if self.base == Self::TESTNET {
            Self::TESTNET_STREAM
        } else {
            Self::MAINNET_STREAM
        }
    }

    /// Open a user data stream and return where to connect.
    ///
    /// # Errors
    /// Anything the request reports, or a body without a key.
    pub fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        let url = format!("{}/fapi/v1/listenKey", self.base);
        let body = self.send_method(Method::Post, &url, true)?;
        let key = field_str(&body, "listenKey").ok_or_else(|| malformed("listen key", &body))?;
        Ok(UserStream::new(
            format!("{}/ws/{key}", self.stream_host()),
            key,
        ))
    }

    /// Renew a stream's key.
    ///
    /// The key lasts an hour. Renewal is not housekeeping: a stream
    /// whose key lapsed stops delivering, and a consumer that treats
    /// quiet as calm will trade against a position that has moved.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        let url = format!("{}/fapi/v1/listenKey", self.base);
        self.send_method(Method::Put, &url, true).map(|_| ())
    }

    /// Close a user data stream.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn close_user_stream(&self) -> Result<(), VenueError> {
        let url = format!("{}/fapi/v1/listenKey", self.base);
        self.send_method(Method::Delete, &url, true).map(|_| ())
    }
}

/// Read one message from the user data stream.
///
/// Pure, so every event this build claims to understand is checked
/// against a recorded message without a socket. Returns `None` for
/// messages that are not account events at all — the venue also sends
/// responses to subscription frames down the same connection.
#[must_use]
pub fn parse_user_event(payload: &str) -> Option<UserEvent> {
    let kind = field_str(payload, "e")?;
    match kind.as_str() {
        "ORDER_TRADE_UPDATE" => {
            // The order sits under "o"; every field below is inside it,
            // and the outer object carries only the type and the times.
            let inner = payload.split_once(r#""o":{"#).map(|(_, rest)| rest)?;
            Some(UserEvent::Order(OrderUpdate {
                symbol: field_str(inner, "s")?,
                client_id: field_str(inner, "c")?,
                venue_id: field_i64(inner, "i")?,
                status: field_str(inner, "X")?,
                last_qty: field_str(inner, "l").unwrap_or_else(|| "0".into()),
                cumulative_qty: field_str(inner, "z").unwrap_or_else(|| "0".into()),
                last_price: field_str(inner, "L").unwrap_or_else(|| "0".into()),
                // Absent, or -1 when the event is not a fill. Both mean
                // the same thing and both must map to None, or a
                // deduplication table acquires an entry for "-1" that
                // swallows every subsequent non-fill.
                trade_id: field_i64(inner, "t").filter(|id| *id > 0),
                event_ms: field_i64(payload, "E").unwrap_or_default(),
            }))
        }
        "listenKeyExpired" => Some(UserEvent::Expired),
        other => Some(UserEvent::Other {
            kind: other.to_string(),
            payload: payload.to_string(),
        }),
    }
}

#[cfg(test)]
mod user_stream {
    use super::*;

    const FILL: &str = r#"{"e":"ORDER_TRADE_UPDATE","E":1786891783639,"T":1786891783630,"o":{"s":"BTCUSDT","c":"oq-1","S":"BUY","o":"LIMIT","f":"GTC","q":"0.002","p":"120000.00","X":"FILLED","i":283194212,"l":"0.002","z":"0.002","L":"119999.90","t":481923,"n":"0.00479999","N":"USDT"}}"#;

    #[test]
    fn a_fill_carries_the_ids_that_join_it_to_an_order_and_deduplicate_it() {
        match parse_user_event(FILL) {
            Some(UserEvent::Order(u)) => {
                assert_eq!(u.client_id, "oq-1", "the id the caller chose");
                assert_eq!(u.venue_id, 283_194_212);
                assert_eq!(u.status, "FILLED");
                assert_eq!(u.cumulative_qty, "0.002");
                assert_eq!(u.last_price, "119999.90");
                assert_eq!(u.trade_id, Some(481_923), "the deduplication key");
                assert_eq!(u.event_ms, 1_786_891_783_639);
            }
            other => panic!("expected an order update, got {other:?}"),
        }
    }

    #[test]
    fn a_non_fill_has_no_trade_id_rather_than_a_sentinel_one() {
        // The venue sends -1 here for events that are not fills.
        // Keeping it would put a row keyed "-1" in the deduplication
        // table, and the second non-fill would be discarded as a
        // duplicate of the first.
        let placed = FILL.replace(r#""t":481923"#, r#""t":-1"#);
        match parse_user_event(&placed) {
            Some(UserEvent::Order(u)) => assert_eq!(u.trade_id, None),
            other => panic!("expected an order update, got {other:?}"),
        }
    }

    #[test]
    fn an_expired_key_is_an_event_and_not_silence() {
        assert_eq!(
            parse_user_event(r#"{"e":"listenKeyExpired","E":1786891783639}"#),
            Some(UserEvent::Expired)
        );
    }

    #[test]
    fn an_unmapped_event_is_kept_whole_rather_than_dropped() {
        let m = r#"{"e":"ACCOUNT_UPDATE","E":1,"a":{"B":[]}}"#;
        match parse_user_event(m) {
            Some(UserEvent::Other { kind, payload }) => {
                assert_eq!(kind, "ACCOUNT_UPDATE");
                assert_eq!(payload, m, "the whole message survives for a reader");
            }
            other => panic!("expected an unmapped event, got {other:?}"),
        }
    }

    #[test]
    fn something_that_is_not_an_account_event_is_none() {
        assert_eq!(parse_user_event(r#"{"result":null,"id":1}"#), None);
    }

    #[test]
    fn the_stream_host_follows_the_rest_base() {
        // A client pointed at the testnet must not listen to production.
        let creds = Credentials::new("k", "s");
        let test = Binance::at(Endpoint::Testnet, creds);
        assert_eq!(test.stream_host(), Binance::TESTNET_STREAM);
        let live = Binance::at(Endpoint::Live, Credentials::new("k", "s"));
        assert_eq!(live.stream_host(), Binance::MAINNET_STREAM);
    }

    #[test]
    fn a_stream_does_not_print_its_own_credential() {
        let s = UserStream::new("wss://x/ws/SECRETKEY".into(), "SECRETKEY".into());
        let shown = format!("{s:?}");
        assert!(!shown.contains("SECRETKEY"), "{shown}");
    }
}

impl Binance {
    /// The venue's listing for one symbol, as raw JSON.
    ///
    /// Read from the deployment being traded on rather than from a
    /// table compiled into the binary. Precision tables belong in
    /// source for replay, where the answer must not change; here the
    /// question is what *this* venue accepts right now, and a testnet
    /// does not always list a contract the way production does.
    ///
    /// # Errors
    /// Anything the request reports, or a symbol the venue does not list.
    pub fn exchange_info(&self, symbol: &str) -> Result<String, VenueError> {
        let body = self.get_public("/fapi/v1/exchangeInfo", &format!("symbol={symbol}"))?;
        // The `symbol=` filter is advisory: at least one deployment
        // ignores it and answers with every contract it lists. So the
        // one wanted has to be found inside the response rather than
        // assumed to be the response, and `objects` cannot do it —
        // exchangeInfo is a single top-level object, so splitting on
        // depth zero yields the whole body and reading a field from that
        // returns whichever contract happens to be listed first.
        object_containing(&body, &format!("\"symbol\":\"{symbol}\"")).ok_or_else(|| {
            VenueError::Malformed {
                what: "symbol not listed",
                body: symbol.to_string(),
            }
        })
    }

    /// Last traded price for one symbol, as raw JSON.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn ticker_price(&self, symbol: &str) -> Result<String, VenueError> {
        self.get_public("/fapi/v1/ticker/price", &format!("symbol={symbol}"))
    }
}

impl Binance {
    /// Whether this account carries a long and a short leg at once.
    ///
    /// Asked rather than assumed. The two modes take different order
    /// parameters, and an order built for the wrong one is refused with
    /// a message about a position side the caller never set — which is
    /// a long way from the setting that actually caused it.
    ///
    /// # Errors
    /// Anything the request reports, or a body without the field.
    pub fn is_hedged_account(&self) -> Result<bool, VenueError> {
        let body = self.get_signed("/fapi/v1/positionSide/dual", "")?;
        field_bool(&body, "dualSidePosition").ok_or_else(|| malformed("position mode", &body))
    }
}

#[cfg(test)]
mod hedged_accounts {
    use super::*;
    use oq_types::{PriceTicks, QtyLots};

    fn order(side: PositionSide, reduce_only: bool) -> NewOrder {
        NewOrder {
            symbol: "BTCUSDT".into(),
            side: Side::Buy,
            limit_price: Some(PriceTicks(12_000_000)),
            qty: QtyLots(2),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq-1".into(),
            reduce_only,
            position_side: side,
        }
    }

    #[test]
    fn a_one_way_account_names_no_leg() {
        let q = order_query(
            &order(PositionSide::OneWay, false),
            &Instrument::linear(2, 3),
        );
        assert!(!q.contains("positionSide"), "{q}");
    }

    #[test]
    fn a_hedged_account_names_the_leg() {
        let q = order_query(&order(PositionSide::Long, false), &Instrument::linear(2, 3));
        assert!(q.contains("positionSide=LONG"), "{q}");
        let q = order_query(
            &order(PositionSide::Short, false),
            &Instrument::linear(2, 3),
        );
        assert!(q.contains("positionSide=SHORT"), "{q}");
    }

    #[test]
    fn reduce_only_is_sent_only_where_the_venue_accepts_it() {
        // One-way: the flag is how a close is expressed.
        let q = order_query(
            &order(PositionSide::OneWay, true),
            &Instrument::linear(2, 3),
        );
        assert!(q.contains("reduceOnly=true"), "{q}");
        // Hedged: naming the leg is how a close is expressed, and the
        // flag is refused. It must not reach the wire even if set.
        let q = order_query(&order(PositionSide::Long, true), &Instrument::linear(2, 3));
        assert!(!q.contains("reduceOnly"), "{q}");
    }

    #[test]
    fn the_final_state_is_requested_rather_than_a_bare_acknowledgement() {
        // Without this the venue answers before it knows whether the
        // order filled, and a fill that happened on arrival reaches the
        // socket before it reaches the caller that placed it.
        let q = order_query(
            &order(PositionSide::OneWay, false),
            &Instrument::linear(2, 3),
        );
        assert!(q.contains("newOrderRespType=RESULT"), "{q}");
    }
}

#[cfg(test)]
mod listings {
    use super::*;

    /// The shape the venue actually answers with: one top-level object,
    /// an array of contracts, each with a nested array of filters. The
    /// wanted contract is deliberately not the first.
    const BODY: &str = r#"{"timezone":"UTC","symbols":[
      {"symbol":"BTCUSDT","pricePrecision":2,"quantityPrecision":3,
       "filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},
                  {"filterType":"LOT_SIZE","stepSize":"0.001"}]},
      {"symbol":"ETHUSDT","pricePrecision":2,"quantityPrecision":4,
       "filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},
                  {"filterType":"LOT_SIZE","stepSize":"0.0001"}]}
    ]}"#;

    #[test]
    fn a_contract_that_is_not_the_first_is_still_found() {
        // The defect this replaces answered with the first contract for
        // every question, so it was right for one symbol and silently
        // wrong for the other six hundred.
        let eth = object_containing(BODY, r#""symbol":"ETHUSDT""#).expect("listed");
        assert_eq!(field_str(&eth, "symbol").as_deref(), Some("ETHUSDT"));
        assert_eq!(field_i64(&eth, "quantityPrecision"), Some(4));
        assert!(
            eth.contains("0.0001"),
            "its own filters came with it: {eth}"
        );
        assert!(!eth.contains("BTCUSDT"), "and only its own: {eth}");
    }

    #[test]
    fn the_first_contract_is_still_found_correctly() {
        let btc = object_containing(BODY, r#""symbol":"BTCUSDT""#).expect("listed");
        assert_eq!(field_i64(&btc, "quantityPrecision"), Some(3));
        assert!(!btc.contains("ETHUSDT"), "{btc}");
    }

    #[test]
    fn a_contract_the_venue_does_not_list_is_none() {
        assert!(object_containing(BODY, r#""symbol":"NOTLISTED""#).is_none());
    }

    #[test]
    fn a_brace_inside_a_string_does_not_close_the_object_early() {
        let body = r#"{"a":[{"symbol":"X","note":"has a } inside","v":1}]}"#;
        let x = object_containing(body, r#""symbol":"X""#).expect("found");
        assert_eq!(
            field_i64(&x, "v"),
            Some(1),
            "the object ran to its real end: {x}"
        );
    }
}
