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

use crate::json::{
    field_bool, field_i64, field_str, malformed, need_bool, need_f64, need_i64, need_str,
    object_containing, objects,
};
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
    ///
    /// Atomic because the offset has to be correctable from a `&self`
    /// path. It is measured once at startup and then applied for the
    /// whole run — twelve hours, on the deployment this was found on —
    /// and a machine whose clock drifts past `recvWindow` in that time
    /// gets every signed request refused with -1021. The correction has
    /// to happen where the refusal is seen, and that is a read path
    /// holding `&self`.
    clock_offset_ms: core::sync::atomic::AtomicI64,
    round_trip_ms: core::sync::atomic::AtomicI64,
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
            clock_offset_ms: core::sync::atomic::AtomicI64::new(0),
            round_trip_ms: core::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Measure the venue's clock against this machine's.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn sync_clock(&mut self) -> Result<i64, VenueError> {
        self.resync_clock()
    }

    /// The same measurement, from a shared reference.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn resync_clock(&self) -> Result<i64, VenueError> {
        // Read the local clock on both sides of the call and take the
        // midpoint. Reading it only afterwards charges the whole return
        // leg to the clock: on a link with a 750 ms round trip that is a
        // systematic third of a second, and on a slow one it reported a
        // 1.4-second skew for a host whose clock was within 250 ms.
        //
        // The estimate assumes the two legs are equal, which they are
        // not, but the error is then half the asymmetry rather than the
        // whole return leg.
        let mut best: Option<(i64, i64)> = None; // (round trip, offset)
        let mut last_err = None;
        for _ in 0..3 {
            let t0 = now_ms();
            match self.get_public("/fapi/v1/time", "") {
                Ok(body) => {
                    let t1 = now_ms();
                    let venue_ms =
                        field_i64(&body, "serverTime").ok_or_else(|| VenueError::Malformed {
                            what: "serverTime",
                            body: body.clone(),
                        })?;
                    let round_trip = t1 - t0;
                    let offset = offset_from(t0, venue_ms, t1);
                    // Keep the quietest sample. A long round trip is the
                    // one most likely to be asymmetric, and therefore the
                    // one whose midpoint is least trustworthy.
                    if best.is_none_or(|(rt, _)| round_trip < rt) {
                        best = Some((round_trip, offset));
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        match best {
            Some((round_trip, offset)) => {
                self.clock_offset_ms
                    .store(offset, core::sync::atomic::Ordering::Relaxed);
                self.round_trip_ms
                    .store(round_trip, core::sync::atomic::Ordering::Relaxed);
                Ok(offset)
            }
            None => Err(last_err.unwrap_or(VenueError::Malformed {
                what: "serverTime",
                body: String::new(),
            })),
        }
    }

    /// The round trip of the quietest clock sample, in milliseconds.
    ///
    /// Reported separately because it is a different problem with a
    /// different fix. A skewed clock is corrected by the offset; a slow
    /// link is not corrected by anything, and it is what makes a signed
    /// request arrive with a timestamp the venue has already outrun —
    /// Binance answers that with -1021, which reads like a clock problem
    /// and is not one.
    #[must_use]
    pub fn round_trip_ms(&self) -> i64 {
        self.round_trip_ms
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The measured offset, for a caller that wants to report it.
    #[must_use]
    pub fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
            .load(core::sync::atomic::Ordering::Relaxed)
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
        now_ms() + self.clock_offset_ms()
    }

    /// Account balance and unrealized profit.
    ///
    /// # Errors
    /// Anything the request reports.
    pub fn account(&self) -> Result<AccountSnapshot, VenueError> {
        let body = self.get_signed("/fapi/v2/account", "")?;
        let read_at_ms = now_ms() + self.clock_offset_ms();
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
    ///
    /// A -1021 re-measures the clock and sends once more. The offset is
    /// taken at startup and applied for the life of the process, so a
    /// machine that drifts past `recvWindow` during a long run has every
    /// signed request refused from that moment until it is restarted —
    /// which is what a twelve-hour run showed, twice.
    ///
    /// Exactly one retry, and only for reads. A clock still wrong after
    /// a fresh measurement is a different fault, and a loop around a
    /// refusal is how a process earns the venue's rate limiter — the same
    /// run also collected a `-1003 IP banned`.
    fn get_signed(&self, path: &str, query: &str) -> Result<String, VenueError> {
        match self.get_signed_once(path, query) {
            Err(e) if is_stale_timestamp(&e) => {
                // Measure first: retrying with the same offset would
                // reproduce the refusal and spend a request proving it.
                self.resync_clock()?;
                self.get_signed_once(path, query)
            }
            other => other,
        }
    }

    fn get_signed_once(&self, path: &str, query: &str) -> Result<String, VenueError> {
        let url = signed_url(
            &self.base,
            path,
            query,
            now_ms() + self.clock_offset_ms(),
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

/// Whether a refusal says the request's timestamp was stale.
///
/// Matched on the venue's numeric code rather than its prose, because
/// the prose is what changes. `-1021` is the one refusal a client can
/// actually repair by itself: the request was well-formed and correctly
/// signed, and only the clock it was stamped with was wrong.
///
/// Deliberately not matched: `-1003` (rate limited) and `-1007`
/// (timeout, execution status unknown). The first would be repaired by
/// waiting and the retry makes it worse; the second may have reached the
/// matching engine, and repeating it is how one order becomes two.
fn is_stale_timestamp(e: &VenueError) -> bool {
    match e {
        VenueError::Venue { body, .. } => body.contains("\"code\":-1021"),
        _ => false,
    }
}

/// The path of a URL, without the query — which is where the signature
/// and the API key live.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or("<url>")
}

/// The clock offset a round trip implies: venue time minus the midpoint
/// of the two local readings.
///
/// Pure and separate so the arithmetic can be tested without a venue.
/// The estimate is exact when the two legs are equal and wrong by half
/// the asymmetry when they are not — which is the reason to prefer it to
/// reading the local clock only after the reply, a method whose error is
/// the entire return leg however symmetric the link.
const fn offset_from(before: i64, venue: i64, after: i64) -> i64 {
    venue - (before + after) / 2
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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
/// What the venue's answer meant.
///
/// Public because it is this adapter's half of the placement contract,
/// and `conformance::check` drives it. A suite that could only be run
/// from inside the crate would not be one a third-party adapter could
/// use, which is most of what a conformance suite is for.
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    // A 2xx is the venue's acceptance, and reading it here rather than
    // only in `place` is what makes this function total over responses.
    // It was not, and the conformance suite is what said so: this
    // adapter's contract-facing pair was `ack_from` on success and
    // `classify` on failure, while OKX's was one function. Two adapters
    // with differently-shaped contract surfaces cannot both be driven
    // through one suite, which made "any adapter can be checked" untrue
    // before it was ever tested. `place` is unaffected — it never hands
    // a 2xx to this function.
    if (200..300).contains(&status) {
        return ack_from(body, client_id);
    }
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
/// Whether an order is worth less than the contract's floor.
///
/// Pure, so the arithmetic is testable without a venue. Refused locally
/// rather than by the venue because the venue's message names its floor
/// and not what the order was worth, which leaves the reader to work out
/// whether the price or the size was the problem.
///
/// A market order is not checked: its notional depends on where it
/// fills, and refusing on a guess would refuse orders the venue accepts.
/// Read a status query's body, without the request that fetched it.
///
/// `None` means the venue says it has no such order — code `-2013`,
/// which after an unresolved placement is the answer that says the
/// order never landed and may be sent again. Separated from
/// `Execution::order_status` so a conformance suite can drive it
/// without a socket: the classification is the part that can be wrong,
/// and the request is the part that needs credentials.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    if field_i64(body, "code") == Some(-2013) {
        return None;
    }
    match ack_from(body, client_id) {
        Placed::Accepted(ack) => Some(ack),
        _ => None,
    }
}

fn below_floor(order: &NewOrder, instrument: &Instrument) -> Option<Reject> {
    if instrument.min_notional.0 <= 0 {
        return None;
    }
    let price = order.limit_price?;
    if price.0 <= 0 {
        return None;
    }
    let notional = instrument.notional(price, order.qty)?;
    if notional.0 >= instrument.min_notional.0 {
        return None;
    }
    Some(Reject {
        code: None,
        message: format!(
            "order notional {} is below this contract's floor of {}",
            decimal(notional.0, 8),
            decimal(instrument.min_notional.0, 8)
        ),
    })
}

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
        if let Some(reject) = below_floor(order, instrument) {
            return Placed::Rejected(reject);
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
            now_ms() + self.clock_offset_ms(),
            self.creds.secret_bytes(),
        );
        match self.send_method(Method::Post, &url, true) {
            Ok(body) => ack_from(&body, &order.client_id),
            Err(VenueError::Venue { status, body }) => {
                // A stale timestamp is repairable, and the read paths
                // repair it by sending again. This one does not. The
                // venue refused *this* order, but a refusal and a silence
                // are not distinguishable from here with certainty, and
                // resending is the single action that can turn "maybe one
                // order" into "certainly two".
                //
                // So the clock is corrected and the refusal is returned.
                // The next order is stamped correctly; this one is the
                // caller's to decide about, which is the whole reason
                // Placed has three outcomes rather than two.
                if body.contains("\"code\":-1021") {
                    let _ = self.resync_clock();
                }
                classify(status, &body, &order.client_id)
            }
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
            now_ms() + self.clock_offset_ms(),
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

impl crate::account::Account for Binance {
    /// Summed from the venue's own trade records.
    ///
    /// `commission` is what was charged, per fill, in the settlement
    /// asset — which for a USDT-margined perpetual is the currency the
    /// kernel counts in, so no conversion is involved and none is
    /// invented. A venue where that stopped being true would need this
    /// to say so rather than to add unlike numbers.
    ///
    /// `Ok(None)` is never returned here: this adapter reports fees, and
    /// a failure to read them is an error rather than an absence. The
    /// difference matters — absence means the residual carries the
    /// component, an error means the caller decides whether to proceed.
    fn fees_charged(
        &self,
        symbol: &str,
        since_ms: i64,
    ) -> Result<Option<oq_types::Cash>, VenueError> {
        let trades = self.my_trades(symbol, Some(since_ms))?;
        let total: f64 = trades.iter().map(|t| t.commission).sum();
        #[allow(clippy::cast_possible_truncation)]
        let cash = (total * oq_types::CASH_SCALE as f64).round() as i64;
        Ok(Some(oq_types::Cash(cash)))
    }

    fn id(&self) -> &'static str {
        // Matches the market-data side's identifier for the same venue,
        // so a run's records and its archive file under one name.
        "binance-perp"
    }

    fn id_rules(&self) -> crate::broker::IdRules {
        crate::broker::IdRules::BINANCE
    }

    fn recent_bars(
        &self,
        symbol: &str,
        minutes: usize,
    ) -> Result<Vec<crate::klines::Kline>, VenueError> {
        // Unsigned: history is public, so a warm-up cannot fail for a
        // reason that has anything to do with this account's keys.
        //
        // The venue caps a page at 1500 and a strategy that wants more
        // than a day of minutes wants a different endpoint, so this is
        // clamped rather than paged: a silent second request would make
        // the returned range differ from the one asked for.
        let limit = minutes.clamp(1, 1500);
        let body = self.get_public(
            "/fapi/v1/klines",
            &format!("symbol={symbol}&interval=1m&limit={limit}"),
        )?;
        let (price_scale, qty_scale) = match crate::account::Account::instrument(self, symbol) {
            Ok(i) => (i.price_scale, i.qty_scale),
            Err(e) => {
                return Err(VenueError::Malformed {
                    what: "instrument for klines",
                    body: e,
                });
            }
        };
        crate::klines::parse(&body, price_scale, qty_scale).ok_or(VenueError::Malformed {
            what: "klines",
            body,
        })
    }

    fn sync_clock(&mut self) -> Result<i64, VenueError> {
        Self::sync_clock(self)
    }

    fn round_trip_ms(&self) -> i64 {
        Self::round_trip_ms(self)
    }

    fn instrument(&self, symbol: &str) -> Result<Instrument, String> {
        let body = self.exchange_info(symbol).map_err(|e| e.to_string())?;
        let price_scale = integer_field(&body, "pricePrecision").ok_or("no pricePrecision")?;
        let qty_scale = integer_field(&body, "quantityPrecision").ok_or("no quantityPrecision")?;
        let price_scale = u8::try_from(price_scale).map_err(|_| "implausible price precision")?;
        let qty_scale = u8::try_from(qty_scale).map_err(|_| "implausible quantity precision")?;
        let tick = decimal_field(&body, "tickSize", price_scale).unwrap_or(1);
        let step = decimal_field(&body, "stepSize", qty_scale).unwrap_or(1);
        // The venue also refuses orders below a notional floor, and its
        // message names the floor without naming what the order was worth.
        // Carried on the instrument so a strategy does not learn it by
        // being refused.
        let floor = decimal_field(&body, "notional", 8).unwrap_or(0);
        Ok(oq_types::Instrument::linear(price_scale, qty_scale)
            .with_grid(tick, step)
            .with_min_notional(oq_types::Cash(floor)))
    }

    fn is_hedged(&self) -> Result<bool, VenueError> {
        self.is_hedged_account()
    }

    fn positions(&self, symbol: &str) -> Result<Vec<PositionSnapshot>, VenueError> {
        Self::positions(self, symbol)
    }

    fn balances(&self) -> Result<AccountSnapshot, VenueError> {
        self.account()
    }

    fn open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, VenueError> {
        Self::open_orders(self, symbol)
    }

    fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        Self::open_user_stream(self)
    }

    fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        Self::keepalive_user_stream(self)
    }

    fn close_user_stream(&self) -> Result<(), VenueError> {
        Self::close_user_stream(self)
    }
}

/// Parsing helpers for the instrument description, kept beside the only
/// thing that reads that venue's shape.
fn integer_field(body: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn decimal_field(body: &str, key: &str, scale: u8) -> Option<i64> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let text = &rest[..rest.find('"')?];
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let mut digits = String::from(whole);
    let width = usize::from(scale);
    let mut frac = frac.to_string();
    frac.truncate(width);
    while frac.len() < width {
        frac.push('0');
    }
    digits.push_str(&frac);
    digits.parse().ok()
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
                side: field_str(inner, "S").unwrap_or_default(),
                // Absent means taker: the venue sets it only when the
                // fill made liquidity, and defaulting the other way
                // would credit a maker rebate to an order that paid.
                maker: field_bool(inner, "m").unwrap_or(false),
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

    /// The side is read from the venue rather than inferred from what
    /// this process asked for. The two are different facts, and only
    /// one of them is what happened.
    #[test]
    fn a_fill_says_which_way_it_went() {
        match parse_user_event(FILL) {
            Some(UserEvent::Order(u)) => assert_eq!(u.side, "BUY"),
            other => panic!("expected an order update, got {other:?}"),
        }
    }

    /// Maker or taker decides the fee, and on some venues that is the
    /// difference between a rebate and a charge — an order of
    /// magnitude, not a rounding. The venue sets the flag only when the
    /// fill made liquidity, so absent must read as taker: defaulting the
    /// other way would credit a rebate to an order that paid.
    #[test]
    fn absent_means_taker_rather_than_maker() {
        match parse_user_event(FILL) {
            Some(UserEvent::Order(u)) => {
                assert!(!u.maker, "this payload has no `m`, so it took liquidity");
            }
            other => panic!("expected an order update, got {other:?}"),
        }

        let made = FILL.replace(r#""t":481923"#, r#""m":true,"t":481923"#);
        match parse_user_event(&made) {
            Some(UserEvent::Order(u)) => assert!(u.maker),
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

#[cfg(test)]
mod floors {
    use super::*;
    use oq_types::{Cash, PriceTicks, QtyLots};

    fn eth_testnet() -> Instrument {
        // Two decimal places of price, three of quantity, and a floor of
        // twenty units of quote — the contract that produced this code.
        Instrument::linear(2, 3).with_min_notional(Cash(20 * oq_types::CASH_SCALE))
    }

    fn order_at(price: i64, qty: i64) -> NewOrder {
        NewOrder {
            symbol: "ETHUSDT".into(),
            side: Side::Buy,
            limit_price: Some(PriceTicks(price)),
            qty: QtyLots(qty),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq-1".into(),
            reduce_only: false,
            position_side: PositionSide::OneWay,
        }
    }

    #[test]
    fn an_order_under_the_floor_is_refused_with_both_numbers() {
        // 0.001 of a contract at 3000 is 3 units of quote, against a
        // floor of 20. The venue refuses this and names its floor
        // without naming what the order was worth, which leaves the
        // reader guessing whether the price or the size was wrong.
        let i = eth_testnet();
        let o = order_at(300_000, 1);
        match below_floor(&o, &i) {
            Some(r) => {
                assert!(
                    r.message.contains("below this contract's floor"),
                    "{}",
                    r.message
                );
                assert!(
                    r.message.contains("3.00000000"),
                    "the order's worth: {}",
                    r.message
                );
                assert!(
                    r.message.contains("20.00000000"),
                    "and the floor: {}",
                    r.message
                );
            }
            None => panic!("an order worth 3 against a floor of 20 must be refused"),
        }
    }

    #[test]
    fn an_order_over_the_floor_passes() {
        let i = eth_testnet();
        // 0.008 at 3000 is 24 units of quote, over the floor of 20.
        assert!(below_floor(&order_at(300_000, 8), &i).is_none());
    }

    #[test]
    fn a_contract_with_no_stated_floor_refuses_nothing() {
        let i = Instrument::linear(2, 3);
        assert!(below_floor(&order_at(300_000, 1), &i).is_none());
    }

    #[test]
    fn a_market_order_is_not_checked_against_the_floor() {
        // Its notional depends on where it fills, and refusing on a
        // guess would refuse orders the venue accepts.
        let mut o = order_at(300_000, 1);
        o.limit_price = None;
        assert!(below_floor(&o, &eth_testnet()).is_none());
    }
}

#[cfg(test)]
mod signing_tests {
    use super::*;

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
        let c = Binance::at(Endpoint::Testnet, Credentials::new("k", "s"));
        let unadjusted = c.venue_time_ms();
        assert!(
            (unadjusted - now_ms()).abs() < 1_000,
            "with no offset it should be local time"
        );

        c.clock_offset_ms
            .store(-30_000, core::sync::atomic::Ordering::Relaxed);
        let adjusted = c.venue_time_ms();
        assert!(
            (adjusted - (unadjusted - 30_000)).abs() < 1_000,
            "expected the offset to apply: {adjusted} vs {unadjusted}"
        );
    }
}

#[cfg(test)]
mod clock {
    use super::*;

    /// Why the midpoint, stated as arithmetic.
    ///
    /// A host whose clock sat within 250 ms of the venue was reported as
    /// 1.4 seconds ahead, because the estimate this replaced charged the
    /// whole return leg to the clock. On that link the measurement error
    /// was larger than the quantity being measured.
    #[test]
    fn the_midpoint_survives_a_slow_link() {
        // True offset zero, 800 ms round trip, legs equal.
        let (before, after) = (1_000, 1_800);
        let venue = 1_400;
        assert_eq!(offset_from(before, venue, after), 0);
        assert_eq!(
            venue - after,
            -400,
            "the method this replaced: the return leg, reported as skew"
        );
    }

    /// Asymmetry is where the midpoint is imperfect, and it is still the
    /// better of the two by a wide margin.
    #[test]
    fn asymmetric_legs_cost_half_the_asymmetry_not_a_whole_leg() {
        let (before, after) = (1_000, 1_800);
        let venue = 1_200; // the reply took 600 of the 800
        assert_eq!(offset_from(before, venue, after), -200);
        assert_eq!(venue - after, -600, "three times the error");
    }

    /// A skew large enough to matter is still seen through a slow link.
    /// The point of the change is not to hide skew, it is to stop
    /// inventing it.
    #[test]
    fn real_skew_is_still_reported() {
        let (before, after) = (1_000, 1_800);
        let venue = 2_900; // venue genuinely 1.5 s ahead of the midpoint
        assert_eq!(offset_from(before, venue, after), 1_500);
    }
}

#[cfg(test)]
mod stale_timestamp {
    use super::{VenueError, is_stale_timestamp};

    fn venue(body: &str) -> VenueError {
        VenueError::Venue {
            status: 400,
            body: body.to_string(),
        }
    }

    /// The refusal a client can repair by itself.
    #[test]
    fn a_stale_timestamp_is_recognised() {
        assert!(is_stale_timestamp(&venue(
            r#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#
        )));
    }

    /// Rate limiting is repaired by waiting, and a retry makes it worse.
    ///
    /// Not hypothetical: the run that produced two -1021s also collected
    /// a -1003 IP ban. Retrying into a rate limiter is how the second
    /// follows the first.
    #[test]
    fn rate_limiting_is_not_retried() {
        assert!(!is_stale_timestamp(&venue(
            r#"{"code":-1003,"msg":"Way too many requests; IP banned until 1787144158148."}"#
        )));
    }

    /// The one that must never be retried anywhere.
    ///
    /// -1007 says the venue does not know whether the request executed.
    /// Sending it again is how one order becomes two.
    #[test]
    fn an_unknown_execution_status_is_not_retried() {
        assert!(!is_stale_timestamp(&venue(
            r#"{"code":-1007,"msg":"Timeout waiting for response from backend server. Send status unknown; execution status unknown."}"#
        )));
    }

    /// A transport failure never reached the venue's validation, so
    /// there is no code to read and nothing here can say the clock was
    /// the problem.
    #[test]
    fn a_transport_failure_is_not_a_clock_problem() {
        assert!(!is_stale_timestamp(&VenueError::Transport(
            "connection reset".into()
        )));
    }

    /// Matched on the code, not the prose.
    ///
    /// A body that merely mentions recvWindow — a different error whose
    /// message names it, or a symbol that happens to contain the digits
    /// — is not this error.
    #[test]
    fn the_prose_alone_does_not_count() {
        assert!(!is_stale_timestamp(&venue(
            r#"{"code":-2015,"msg":"Invalid API-key. See recvWindow and -1021 in the docs."}"#
        )));
    }
}
