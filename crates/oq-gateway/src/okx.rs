//! OKX perpetual swaps.
//!
//! The second venue, chosen because it differs from the first in ways
//! that are structural rather than cosmetic. A venue abstraction that
//! only ever met one venue is a guess; these four differences are what
//! it was tested against.
//!
//! # 1. A 200 does not mean the order was accepted
//!
//! Binance says no with an HTTP status. OKX says no with HTTP 200 and a
//! body:
//!
//! ```text
//! {"code":"1","msg":"","data":[{"sCode":"51008","sMsg":"Insufficient..."}]}
//! ```
//!
//! There are *two* codes and both matter. The envelope's `code` is the
//! request's outcome; each element's `sCode` is that order's. A request
//! can succeed while the order in it was refused. An adapter that trusts
//! the status line reports every rejection as an acceptance, and the
//! caller books a position that does not exist — which is the single
//! worst failure this crate can have, so [`classify`] reads the body and
//! never the status alone.
//!
//! # 2. Testnet is a header, not a hostname
//!
//! Binance's testnet is a different domain: pointing at the wrong one
//! fails loudly. OKX's demo trading is the *same* host with
//! `x-simulated-trading: 1` set. A dropped header is not a broken
//! request, it is a live one. So the header is derived from
//! [`Endpoint`] at construction and there is no way to set it by hand;
//! `Endpoint::Live` is the only path to real money and it must be named.
//!
//! # 3. Size is in contracts, not in coins
//!
//! `sz` counts contracts, and one contract is `ctVal` of the underlying
//! — 0.01 BTC on BTC-USDT-SWAP. Sending a coin quantity as `sz` is off
//! by a factor of a hundred, in the direction a balance check on a small
//! order does not catch.
//!
//! And `sz` is a *decimal*, not a count. The venue's own listing for
//! BTC-USDT-SWAP gives `lotSz: "0.01"`, so a hundredth of a contract is
//! a legal order size. This adapter first assumed whole contracts, and
//! the venue's real payload is what said otherwise — which is the
//! argument for reading a listing rather than writing a table.
//! [`Listing::size_text`] does the conversion, on the venue's own grid,
//! and refuses rather than rounding: a size quietly rounded is a
//! different order than the one that was risked, and the position check
//! downstream then blames the venue.
//!
//! # 4. Three secrets, and the signature covers the body
//!
//! The pair becomes a triple — key, secret, passphrase — and the
//! signature is base64 of an HMAC over `timestamp + method + path +
//! body`, not hex of an HMAC over the query. A POST signs its body; a
//! GET signs its query string as part of the path.
//!
//! # What this has not done
//!
//! **It has not been run against OKX.** Every pure function here is
//! tested against payloads taken from the venue's documented shapes, and
//! that is not the same as having placed an order. The Binance adapter
//! was written to the same standard and the first real run found five
//! defects no unit test reached — a price on the right precision but off
//! the tick grid, a contract lookup that returned the first symbol, a
//! read timeout read as a disconnect, an order below a floor nobody had
//! asked about, and a risk gate handed a hardcoded zero. Assume this one
//! has its own five. `oq-order-check` against OKX demo trading is what
//! turns this from written to working, and until someone runs it this
//! module is not to be pointed at `Endpoint::Live`.

use core::time::Duration;

use oq_hash::hmac::hmac_sha256;
use oq_types::{Instrument, QtyLots, Side, TimeInForce};

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{
    Endpoint, Execution, Handshake, NewOrder, Opening, OrderAck, OrderUpdate, Placed, PositionSide,
    Reject, Unresolved, UserEvent, UserStream, decimal,
};
use crate::json::{array_field, field_str, malformed, objects};

/// A client for one OKX deployment.
pub struct Okx {
    base: String,
    creds: Credentials,
    agent: ureq::Agent,
    /// Whether every request carries `x-simulated-trading: 1`.
    ///
    /// Not a setting. Derived from [`Endpoint`] once, at construction,
    /// because the difference between demo and live here is one header
    /// and a header is exactly the kind of thing that gets dropped.
    simulated: bool,
    /// Venue clock minus local clock, in milliseconds.
    clock_offset_ms: i64,
    /// The round trip measured while the offset was taken.
    ///
    /// Kept because a clock offset alone cannot say whether it is worth
    /// believing: the estimate is the midpoint of a round trip, so a
    /// long trip is a wide interval, and a caller refused for a stale
    /// timestamp needs to know which of the two it is looking at.
    round_trip_ms: i64,
}

impl Okx {
    /// The one host. Demo trading is the same one.
    pub const HOST: &'static str = "https://www.okx.com";

    /// Build a client against a named deployment.
    #[must_use]
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Self {
        Self::new(Self::HOST, creds, endpoint)
    }

    /// Build a client against `base`, for a named deployment.
    ///
    /// `base` exists for tests and for a proxy. It does not select
    /// between demo and live — [`Endpoint`] does, and nothing else can.
    #[must_use]
    pub fn new(base: impl Into<String>, creds: Credentials, endpoint: Endpoint) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(45)))
            .http_status_as_error(false)
            .build();
        Self {
            base: base.into(),
            creds,
            agent: config.into(),
            simulated: matches!(endpoint, Endpoint::Testnet),
            clock_offset_ms: 0,
            round_trip_ms: 0,
        }
    }

    /// Whether this client is pointed at demo trading.
    #[must_use]
    pub const fn is_simulated(&self) -> bool {
        self.simulated
    }
}

// ---------------------------------------------------------------------
// Pure: everything below decides what to send and what an answer meant,
// and none of it touches a socket.
// ---------------------------------------------------------------------

/// `2026-08-18T02:03:04.567Z`, which is the only format OKX accepts.
///
/// Computed from the epoch by hand rather than by a date library: the
/// civil-from-days algorithm is twenty lines and a dependency in the
/// signing path is not worth twenty lines.
pub(crate) fn iso_timestamp(ms: i64) -> String {
    let (days, rem_ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (h, m, s, milli) = (
        rem_ms / 3_600_000,
        rem_ms / 60_000 % 60,
        rem_ms / 1_000 % 60,
        rem_ms % 1_000,
    );

    // Howard Hinnant's civil_from_days, shifted to a March-based year so
    // the leap day lands at the end and the month lengths repeat.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

/// The signature OKX expects: base64 of HMAC-SHA256 over
/// `timestamp + method + requestPath + body`.
///
/// `request_path` includes the query string when there is one, because
/// the venue signs what it receives and a GET's parameters are part of
/// that.
pub(crate) fn sign(
    secret: &[u8],
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
) -> String {
    let message = format!("{timestamp}{method}{request_path}{body}");
    crate::b64::encode(&hmac_sha256(secret, message.as_bytes()))
}

/// Why an order could not be expressed in contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeError {
    /// What went wrong, in terms the caller can act on.
    pub message: String,
}

/// One contract, as the venue itself describes it.
///
/// Read from `/api/v5/public/instruments` rather than written down. A
/// baked table is wrong the day the venue relists something, and the
/// failure is an order rejected for a precision nobody changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The venue's own spelling, e.g. `BTC-USDT-SWAP`.
    pub inst_id: String,
    /// How much of the underlying one contract is, at [`CONTRACT_SCALE`].
    ///
    /// [`CONTRACT_SCALE`]: oq_types::CONTRACT_SCALE
    pub contract_value: i64,
    /// Decimal places in a price.
    pub price_scale: u8,
    /// Price grid, in units of `1e-price_scale`.
    pub price_tick: i64,
    /// Decimal places in a size, which is counted in contracts.
    pub size_scale: u8,
    /// Size grid, in units of `1e-size_scale` contracts.
    pub lot_size: i64,
    /// Smallest order, in units of `1e-size_scale` contracts.
    pub min_size: i64,
}

/// A decimal string as a fixed-point integer at `scale`, and the number
/// of decimal places it actually carried.
///
/// Returns `None` rather than a guess: a listing field this build cannot
/// read is a listing it must not trade against.
fn parse_decimal(text: &str) -> Option<(i64, u8)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match text.split_once('.') {
        None => (text, ""),
        Some((a, b)) => (a, b),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
        || u8::try_from(frac_part.len()).is_err()
    {
        return None;
    }
    let digits: String = format!("{int_part}{frac_part}");
    digits
        .parse::<i64>()
        .ok()
        .map(|v| (v, u8::try_from(frac_part.len()).unwrap_or(0)))
}

/// Read one instrument out of a `/api/v5/public/instruments` response.
///
/// # Errors
/// When the response has no such instrument, or a field this build needs
/// is missing or unreadable. Named per field, because the usual cause is
/// a venue that renamed something and the fix depends on which.
pub fn parse_listing(body: &str, inst_id: &str) -> Result<Listing, VenueError> {
    let needle = format!("\"instId\":\"{inst_id}\"");
    let datum = objects(body)
        .into_iter()
        .find(|o| o.contains(&needle))
        .ok_or_else(|| VenueError::Transport(format!("no listing for {inst_id}")))?;

    let field = |key: &'static str| -> Result<(i64, u8), VenueError> {
        let raw = field_str(&datum, key)
            .ok_or_else(|| VenueError::Transport(format!("listing has no {key}")))?;
        parse_decimal(&raw)
            .ok_or_else(|| VenueError::Transport(format!("listing {key} is {raw:?}")))
    };

    let (tick, price_scale) = field("tickSz")?;
    let (lot, size_scale) = field("lotSz")?;
    let (min_raw, min_scale) = field("minSz")?;
    let (ct_val, ct_scale) = field("ctVal")?;

    // minSz and lotSz are both sizes and both decimals, but the venue
    // does not promise they carry the same number of places. Rescale
    // rather than assume, or a `minSz` of "1" against a `lotSz` of
    // "0.01" would be read as one hundredth of a contract.
    let min_size = rescale(min_raw, min_scale, size_scale)
        .ok_or_else(|| VenueError::Transport("minSz does not fit the size grid".to_string()))?;
    let contract_value = rescale(ct_val, ct_scale, 8).ok_or_else(|| {
        VenueError::Transport("ctVal does not fit the contract scale".to_string())
    })?;

    Ok(Listing {
        inst_id: inst_id.to_string(),
        contract_value,
        price_scale,
        price_tick: tick,
        size_scale,
        lot_size: lot,
        min_size,
    })
}

/// Move a fixed-point value from one scale to another, exactly or not
/// at all.
fn rescale(value: i64, from: u8, to: u8) -> Option<i64> {
    if from == to {
        return Some(value);
    }
    if from < to {
        let factor = 10_i128.checked_pow(u32::from(to - from))?;
        i64::try_from(i128::from(value) * factor).ok()
    } else {
        let factor = 10_i128.pow(u32::from(from - to));
        let v = i128::from(value);
        (v % factor == 0).then(|| i64::try_from(v / factor).ok())?
    }
}

impl Listing {
    /// The shared instrument model, for the checks that are not
    /// venue-specific.
    ///
    /// `qty_scale` and `qty_step` describe the size *in contracts*,
    /// because that is the grid this venue enforces. A caller that
    /// thinks in coins converts with [`Listing::size_text`].
    #[must_use]
    pub const fn instrument(&self) -> Instrument {
        Instrument {
            price_scale: self.price_scale,
            qty_scale: self.size_scale,
            contract_size: self.contract_value,
            price_tick: self.price_tick,
            qty_step: self.lot_size,
            min_notional: oq_types::Cash(0),
        }
    }

    /// A quantity of the underlying as the `sz` this venue expects.
    ///
    /// Refuses rather than rounds, and refuses below the venue's own
    /// minimum, because both failures are silent in the other direction:
    /// a rounded size sends an order nobody sized, and one below the
    /// minimum is refused by the venue with a message about the size
    /// rather than about the floor.
    ///
    /// # Errors
    /// When the quantity is not on the venue's size grid, or is below
    /// its minimum order.
    pub fn size_text(&self, qty: QtyLots, qty_scale: u8) -> Result<String, SizeError> {
        if self.contract_value <= 0 {
            return Err(SizeError {
                message: "the listing does not say how much one contract is worth".to_string(),
            });
        }
        // contracts = qty / contract_value, both fixed-point at their
        // own scales, expressed at `size_scale`. i128 throughout so a
        // large order does not overflow on the way to a small answer.
        let numerator = i128::from(qty.0)
            * i128::from(oq_types::CONTRACT_SCALE)
            * 10_i128.pow(u32::from(self.size_scale));
        let denominator = i128::from(self.contract_value) * 10_i128.pow(u32::from(qty_scale));
        if denominator == 0 || numerator % denominator != 0 {
            return Err(SizeError {
                message: format!(
                    "{} is not expressible on this venue's size grid \
                     ({} contracts, at {} of the underlying each)",
                    decimal(qty.0, qty_scale),
                    decimal(self.lot_size, self.size_scale),
                    decimal(self.contract_value, 8),
                ),
            });
        }
        let contracts = i64::try_from(numerator / denominator).map_err(|_| SizeError {
            message: "the order is larger than the venue can express".to_string(),
        })?;
        if contracts % self.lot_size != 0 {
            return Err(SizeError {
                message: format!(
                    "{} contracts is not a multiple of the lot size {}; \
                     size the order on the grid rather than having it rounded",
                    decimal(contracts, self.size_scale),
                    decimal(self.lot_size, self.size_scale),
                ),
            });
        }
        if contracts < self.min_size {
            return Err(SizeError {
                message: format!(
                    "{} contracts is below this venue's minimum of {}",
                    decimal(contracts, self.size_scale),
                    decimal(self.min_size, self.size_scale),
                ),
            });
        }
        Ok(decimal(contracts, self.size_scale))
    }
}

/// Whether OKX will accept this as a `clOrdId`.
///
/// Letters and digits only, 1 to 32 characters. Narrower than Binance's,
/// so an id that works on one venue is not guaranteed on the other —
/// which is why this is checked here and not assumed from the contract.
#[must_use]
pub fn valid_client_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `long`, `short`, or absent.
const fn pos_side(side: PositionSide) -> Option<&'static str> {
    match side {
        PositionSide::OneWay => None,
        PositionSide::Long => Some("long"),
        PositionSide::Short => Some("short"),
    }
}

/// The JSON body for a placement.
///
/// Built by hand, in a fixed field order, because the body is signed:
/// two serialisations that differ only in key order produce two
/// different signatures, and only one of them is the one that was sent.
#[must_use]
pub fn order_body(order: &NewOrder, instrument: &Instrument, size: &str) -> String {
    let mut fields = vec![
        format!("\"instId\":\"{}\"", order.symbol),
        format!(
            "\"tdMode\":\"{}\"",
            // Cross margin. Named rather than defaulted because the
            // venue's own default differs by account and an order that
            // silently opened an isolated position would be margined
            // against a balance the caller did not intend.
            "cross"
        ),
        format!("\"clOrdId\":\"{}\"", order.client_id),
        format!(
            "\"side\":\"{}\"",
            match order.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            }
        ),
        format!("\"sz\":\"{size}\""),
    ];
    if let Some(leg) = pos_side(order.position_side) {
        fields.push(format!("\"posSide\":\"{leg}\""));
    }
    match order.limit_price {
        None => fields.push("\"ordType\":\"market\"".to_string()),
        Some(price) => {
            fields.push(format!(
                "\"ordType\":\"{}\"",
                match order.tif {
                    // OKX expresses time in force as an order type
                    // rather than as a separate field, so a limit order
                    // that must not rest is a different `ordType`, not a
                    // limit order with a flag.
                    TimeInForce::GoodTilCancel => "limit",
                    TimeInForce::ImmediateOrCancel => "ioc",
                    TimeInForce::FillOrKill => "fok",
                }
            ));
            fields.push(format!(
                "\"px\":\"{}\"",
                decimal(price.0, instrument.price_scale)
            ));
        }
    }
    if order.reduce_only {
        fields.push("\"reduceOnly\":true".to_string());
    }
    format!("{{{}}}", fields.join(","))
}

/// Read the envelope's `code`, which is a string even though it is a
/// number.
fn envelope_code(body: &str) -> Option<String> {
    field_str(body, "code")
}

/// The first element of `data`, which is where a single-order response
/// puts its answer.
fn first_datum(body: &str) -> Option<String> {
    let start = body.find("\"data\"")?;
    let array = body[start..].find('[')? + start;
    objects(&body[array..]).into_iter().next()
}

/// What the venue's answer meant.
///
/// `status` is taken but deliberately not trusted on its own: on this
/// venue a refusal arrives as 200. It is used only to recognise the
/// transport-level failures that never reach the envelope at all.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    // 5xx and 429 are the venue saying nothing useful about the order.
    // Whether it landed is unknown, and unknown is not rejected.
    if status >= 500 || status == 429 {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }

    let datum = first_datum(body);
    let order_code = datum.as_deref().and_then(|d| field_str(d, "sCode"));

    match (envelope_code(body).as_deref(), order_code.as_deref()) {
        // "API endpoint request timeout (does not mean that the request
        // was successful or failed, please check the request result)" —
        // the venue's own words for 50004. Unknown, not refused.
        (Some("50004"), _) | (_, Some("50004")) => Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("request timed out at the venue: {}", truncate(body)),
        }),
        // The only shape that means the order exists.
        (Some("0"), Some("0") | None) => ack_from(body, client_id),
        // The request was fine, the order was not. The per-order message
        // is the useful one; the envelope's is usually empty.
        (Some("0" | "1" | "2"), Some(code)) => Placed::Rejected(Reject {
            code: code.parse::<i64>().ok(),
            message: datum
                .as_deref()
                .and_then(|d| field_str(d, "sMsg"))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| truncate(body)),
        }),
        // A whole-request refusal with no per-order detail: a bad
        // signature, a missing header, a rate limit stated in the body.
        (Some(code), None) if code != "0" => Placed::Rejected(Reject {
            code: code.parse::<i64>().ok(),
            message: field_str(body, "msg")
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| truncate(body)),
        }),
        // No envelope at all. Not this venue's answer, so nothing here
        // can be concluded about the order.
        _ => Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unrecognised response: {}", truncate(body)),
        }),
    }
}

/// Build the acknowledgement from a successful response.
#[must_use]
pub fn ack_from(body: &str, client_id: &str) -> Placed {
    let Some(datum) = first_datum(body) else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("accepted with no order in it: {}", truncate(body)),
        });
    };
    let venue_id = field_str(&datum, "ordId").unwrap_or_default();
    Placed::Accepted(OrderAck {
        venue_id,
        // Echoed when the venue gives it back, and the caller's own
        // otherwise: the id the caller chose is the handle, and losing
        // it because a response omitted a field would defeat the point
        // of having chosen it.
        client_id: field_str(&datum, "clOrdId")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| client_id.to_string()),
        status: field_str(&datum, "state").unwrap_or_else(|| "live".to_string()),
        executed_qty: field_str(&datum, "accFillSz").unwrap_or_else(|| "0".to_string()),
    })
}

/// Keep a diagnostic readable when a venue returns a page of HTML.
fn truncate(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= 300 {
        return trimmed.to_string();
    }
    let mut cut = 300;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &trimmed[..cut])
}

// ---------------------------------------------------------------------
// Transport.
// ---------------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Okx {
    /// Send a signed request and return its body.
    ///
    /// A non-2xx status is returned as [`VenueError::Venue`] carrying the
    /// body, because on this venue the body is where the reason lives —
    /// and because a 200 can still be a refusal, the caller must read the
    /// body in either case.
    fn send(&self, method: &str, path: &str, body: &str) -> Result<String, VenueError> {
        let timestamp = iso_timestamp(now_ms() + self.clock_offset_ms);
        let signature = sign(self.creds.secret_bytes(), &timestamp, method, path, body);
        let Some(passphrase) = self.creds.passphrase() else {
            // Not a signature problem, and the venue would report it as
            // one. Caught here so the message names the missing secret.
            return Err(VenueError::Transport(
                "OKX needs a passphrase as well as a key and a secret; \
                 set OQ_VENUE_PASSPHRASE or use Credentials::with_passphrase"
                    .to_string(),
            ));
        };
        let url = format!("{}{path}", self.base);

        // One list, applied by both branches. ureq's GET and POST
        // builders are different types, so the request cannot be built
        // once — but the *headers* can be, and they must be: writing
        // them out twice is how `x-simulated-trading` ends up on one
        // path and not the other, and the path without it is live money.
        let mut headers: Vec<(&str, &str)> = vec![
            ("OK-ACCESS-KEY", self.creds.key()),
            ("OK-ACCESS-SIGN", &signature),
            ("OK-ACCESS-TIMESTAMP", &timestamp),
            ("OK-ACCESS-PASSPHRASE", passphrase),
            ("Content-Type", "application/json"),
        ];
        if self.simulated {
            headers.push(("x-simulated-trading", "1"));
        }

        let sent = if method == "POST" {
            let mut r = self.agent.post(&url);
            for (k, v) in &headers {
                r = r.header(*k, *v);
            }
            r.send(body)
        } else {
            let mut r = self.agent.get(&url);
            for (k, v) in &headers {
                r = r.header(*k, *v);
            }
            r.call()
        };
        let mut response = sent.map_err(|e| VenueError::Transport(e.to_string()))?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| VenueError::Transport(e.to_string()))?;

        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(VenueError::Venue { status, body: text })
        }
    }
}

impl Execution for Okx {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        if !valid_client_id(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable on this venue: 1-32 characters of \
                     [A-Za-z0-9]. Note this is narrower than the other venue's, so an \
                     id that works there is not guaranteed here",
                    order.client_id
                ),
            });
        }
        if let Some(price) = order.limit_price
            && !instrument.price_on_grid(price)
        {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "price {} is not a multiple of the tick size ({} in units of 1e-{}); \
                     snap it deliberately rather than having it moved",
                    decimal(price.0, instrument.price_scale),
                    instrument.price_tick,
                    instrument.price_scale
                ),
            });
        }
        if order.reduce_only && order.position_side.is_hedged() {
            return Placed::Rejected(Reject {
                code: None,
                message: "reduceOnly and a hedged position side are mutually exclusive: \
                          a hedged account expresses a close by naming the leg"
                    .to_string(),
            });
        }
        // `order.qty` is a count of the *instrument's* units, and the
        // instrument says what one unit is worth: `contract_size` is the
        // asset itself on a venue whose contract is one coin, and 0.01
        // BTC here. So a quantity is a number of contracts on both
        // venues and means different amounts of coin on each — which is
        // the whole reason `Instrument` carries a contract size. A
        // caller holding a coin amount converts with `Listing::size_text`
        // before building the order, not here: converting inside the
        // send would hide a rounding from the layer that sized the risk.
        if !instrument.qty_on_grid(order.qty) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "size {} is not a multiple of the lot size ({} in units of 1e-{}) — \
                     note this venue counts contracts, and one contract is {} of the \
                     underlying",
                    decimal(order.qty.0, instrument.qty_scale),
                    instrument.qty_step,
                    instrument.qty_scale,
                    decimal(instrument.contract_size, 8),
                ),
            });
        }
        if order.qty.0 <= 0 {
            return Placed::Rejected(Reject {
                code: None,
                message: "an order must have a positive size".to_string(),
            });
        }
        let contracts = decimal(order.qty.0, instrument.qty_scale);

        let body = order_body(order, instrument, &contracts);
        match self.send("POST", "/api/v5/trade/order", &body) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        // A POST, not a DELETE, and the ids go in the body rather than
        // the query — so the signature covers them.
        let body = format!("{{\"instId\":\"{symbol}\",\"clOrdId\":\"{client_id}\"}}");
        match self.send("POST", "/api/v5/trade/cancel-order", &body) {
            Ok(text) => classify(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(&self, symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let path = format!("/api/v5/trade/order?instId={symbol}&clOrdId={client_id}");
        let text = self.send("GET", &path, "")?;
        // An error envelope other than "does not exist" is the venue
        // failing to answer, delivered as HTTP 200. Read as "no such
        // order" it licensed a resend of an order that may be resting.
        if let Some(code) = envelope_code(&text)
            && code != "0"
            && code != "51603"
        {
            return Err(malformed("order status", &text));
        }
        Ok(order_from_query(&text, client_id))
    }
}

/// Read an order out of a status query, or conclude there is none.
///
/// `None` means the venue has no such order — which, after an
/// [`Placed::Unknown`], is the answer that says the order never landed
/// and may be sent again. A malformed answer is *not* `None`: it is an
/// error, because "no such order" and "I could not tell" lead to
/// opposite actions and only one of them is safe to guess at.
///
/// # Errors
/// When the body is not this venue's shape.
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    // 51603 is "order does not exist", which is an answer rather than a
    // failure and the only code that licenses a resend.
    if envelope_code(body).as_deref() == Some("51603") {
        return None;
    }
    let datum = first_datum(body)?;
    if field_str(&datum, "ordId").is_none_or(|s| s.is_empty()) {
        return None;
    }
    match ack_from(body, client_id) {
        Placed::Accepted(ack) => Some(ack),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Public reads. No credentials, which is the point: the parts of an
// adapter that can be proved against the real venue without an account
// should be, and these are they.
// ---------------------------------------------------------------------

impl Okx {
    /// Fetch one instrument's listing from the venue.
    ///
    /// Unsigned: this is public data, and requiring credentials to read
    /// it would mean the one part of the adapter that *can* be checked
    /// against the real venue without an account could not be.
    ///
    /// # Errors
    /// Transport failures, and a listing this build cannot read.
    pub fn listing(&self, inst_id: &str) -> Result<Listing, VenueError> {
        let path = format!("/api/v5/public/instruments?instType=SWAP&instId={inst_id}");
        parse_listing(&self.public_get(&path)?, inst_id)
    }

    /// The venue's mark price for one instrument, at `scale`.
    ///
    /// # Errors
    /// Transport failures, and a price this build cannot read.
    pub fn mark_price(&self, inst_id: &str, scale: u8) -> Result<oq_types::PriceTicks, VenueError> {
        let path = format!("/api/v5/public/mark-price?instType=SWAP&instId={inst_id}");
        let body = self.public_get(&path)?;
        let datum = first_datum(&body)
            .ok_or_else(|| VenueError::Transport(format!("no mark price: {}", truncate(&body))))?;
        let raw = field_str(&datum, "markPx")
            .ok_or_else(|| VenueError::Transport("mark price has no markPx".to_string()))?;
        let (value, places) = parse_decimal(&raw)
            .ok_or_else(|| VenueError::Transport(format!("markPx is {raw:?}")))?;
        rescale(value, places, scale)
            .map(oq_types::PriceTicks)
            .ok_or_else(|| {
                VenueError::Transport(format!(
                    "mark price {raw} does not fit {scale} decimal places"
                ))
            })
    }

    fn public_get(&self, path: &str) -> Result<String, VenueError> {
        let url = format!("{}{path}", self.base);
        let mut request = self.agent.get(&url);
        // Demo trading lists its own contracts, and they do not always
        // match production's. Reading production's table and trading
        // against demo is how a precision that was never wrong starts
        // being rejected.
        if self.simulated {
            request = request.header("x-simulated-trading", "1");
        }
        let mut response = request
            .call()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(VenueError::Venue { status, body: text })
        }
    }
}

impl Okx {
    /// A GET that carries no signature.
    ///
    /// Public data needs no key, and asking for it with one means a
    /// warm-up can fail for a reason that has nothing to do with the
    /// market. The simulated header still goes on: demo trading has its
    /// own book, and a run that read live prices and traded demo would
    /// be comparing two different markets without saying so.
    fn get_public(&self, path: &str, query: &str) -> Result<String, VenueError> {
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let mut request = self.agent.get(&url);
        if self.simulated {
            request = request.header("x-simulated-trading", "1");
        }
        let mut response = request
            .call()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(VenueError::Venue { status, body: text })
        }
    }

    /// Both legs of one instrument, as the venue holds them.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports, or a payload this
    /// build cannot read.
    pub fn positions(
        &self,
        inst_id: &str,
    ) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
        let body = self.send(
            "GET",
            &format!("/api/v5/account/positions?instType=SWAP&instId={inst_id}"),
            "",
        )?;
        parse_positions(&body)
    }

    /// The account's balance in one settlement currency.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports, or a payload this
    /// build cannot read.
    pub fn balances(&self, ccy: &str) -> Result<crate::binance::AccountSnapshot, VenueError> {
        let read_at = now_ms();
        let body = self.send("GET", &format!("/api/v5/account/balance?ccy={ccy}"), "")?;
        parse_balance(&body, ccy, read_at)
    }

    /// Everything resting on one instrument.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports, or a payload this
    /// build cannot read.
    pub fn open_orders(&self, inst_id: &str) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
        let body = self.send(
            "GET",
            &format!("/api/v5/trade/orders-pending?instType=SWAP&instId={inst_id}"),
            "",
        )?;
        parse_open_orders(&body)
    }

    /// Whether this account reports a long and a short separately.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports, or a `posMode` this
    /// build does not know.
    pub fn is_hedged_account(&self) -> Result<bool, VenueError> {
        let body = self.send("GET", "/api/v5/account/config", "")?;
        parse_position_mode(&body)
    }

    /// Measure the venue's clock against this one, and keep the offset.
    ///
    /// The estimate is the midpoint of the round trip, which is the best
    /// a single request can do and is why the trip is kept beside it: an
    /// offset taken across a slow link is a wide interval reported as a
    /// number, and a caller refused for a stale timestamp needs to know
    /// which of the two it is looking at.
    ///
    /// # Errors
    /// Whatever the transport reports, or a time this build cannot read.
    pub fn sync_clock(&mut self) -> Result<i64, VenueError> {
        let before = now_ms();
        let body = self.get_public("/api/v5/public/time", "")?;
        let after = now_ms();
        let venue = parse_server_time(&body)?;
        let trip = after - before;
        self.round_trip_ms = trip;
        self.clock_offset_ms = venue - (before + trip / 2);
        Ok(self.clock_offset_ms)
    }

    /// The round trip measured when the clock was last synced, or zero
    /// before it has been.
    #[must_use]
    pub const fn round_trip_ms(&self) -> i64 {
        self.round_trip_ms
    }
}

// ---------------------------------------------------------------------
// Account reads.
//
// Pure, for the same reason the order path's classifiers are: a parser
// that needs a socket gets tested against the shapes somebody imagined
// rather than the ones the venue sends.
// ---------------------------------------------------------------------

/// The `data` array of a v5 envelope, split into its objects.
///
/// `code` is checked here rather than at every call site, because on
/// this venue a failure arrives as HTTP 200 with a non-zero code — the
/// same trap [`classify`] exists for on the order path. A read that
/// skipped it would hand back an empty `data`, and an empty list of
/// positions is indistinguishable from a flat account.
fn envelope(body: &str, what: &'static str) -> Result<Vec<String>, VenueError> {
    let Some(code) = field_str(body, "code") else {
        return Err(malformed(what, body));
    };
    if code != "0" {
        return Err(VenueError::Venue {
            status: 200,
            body: body.to_string(),
        });
    }
    let Some(data) = array_field(body, "data") else {
        return Err(malformed(what, body));
    };
    Ok(objects(&data))
}

/// OKX's leg name in the vocabulary the rest of this workspace reads.
///
/// Not cosmetic, and not a place for a permissive fallback. `oq-live`
/// matches on these strings — `book.rs` reads `FILLED`, `run.rs` reads
/// `CANCELED` — so an adapter passing its own spelling through produces
/// a caller that never sees an order end. Two adapters that disagree
/// about a word do not fail to compile; they fail by booking a position
/// that is not there. `None` for anything unrecognised, so a venue that
/// adds a mode is a refused read rather than a silent `BOTH`.
fn leg_of(pos_side: &str) -> Option<&'static str> {
    match pos_side {
        "long" => Some("LONG"),
        "short" => Some("SHORT"),
        // One-way netting, which this workspace spells `BOTH`.
        "net" => Some("BOTH"),
        _ => None,
    }
}

/// OKX's order side in the same shared vocabulary.
fn side_of(side: &str) -> Option<&'static str> {
    match side {
        "buy" => Some("BUY"),
        "sell" => Some("SELL"),
        _ => None,
    }
}

/// OKX's order state in the same shared vocabulary.
///
/// `mmp_canceled` folds into `CANCELED` because that is what it is: an
/// order withdrawn by the venue's market-maker protection. The caller
/// needs to stop expecting a fill, and the reason it stopped belongs in
/// a log rather than in a state machine that has no branch for it.
fn status_of(state: &str) -> Option<&'static str> {
    match state {
        "live" => Some("NEW"),
        "partially_filled" => Some("PARTIALLY_FILLED"),
        "filled" => Some("FILLED"),
        "canceled" | "mmp_canceled" => Some("CANCELED"),
        _ => None,
    }
}

/// Read `/api/v5/account/positions`.
///
/// Sizes stay in contracts, which is the unit `Instrument` counts in on
/// this venue.
///
/// # Sign comes from `posSide`, not from `pos`
///
/// Under hedging the venue reports a long and a short as separate legs,
/// and the sign of `pos` on the short leg is a detail of the venue this
/// adapter declines to depend on: the leg name already says which
/// direction it is, so the sign is taken from the name and the magnitude
/// from the number. A venue that flipped that convention would then be a
/// venue this still reads correctly.
///
/// A flat leg is kept rather than dropped. A leg reported at zero and a
/// leg the venue does not mention are the same fact to everything above,
/// and dropping the first once told a model that a position it had just
/// closed was absent.
///
/// # Errors
/// When the envelope is a failure, or a field this build needs cannot be
/// read. Named per field: the usual cause is a renamed key and the fix
/// depends on which one.
pub fn parse_positions(body: &str) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
    let mut out = Vec::new();
    for item in envelope(body, "positions")? {
        let Some(inst_id) = field_str(&item, "instId") else {
            return Err(malformed("position instId", &item));
        };
        let Some(pos_side) = field_str(&item, "posSide") else {
            return Err(malformed("position posSide", &item));
        };
        let Some(leg) = leg_of(&pos_side) else {
            return Err(malformed("position posSide", &item));
        };
        let Some(pos) = field_str(&item, "pos") else {
            return Err(malformed("position pos", &item));
        };
        // An empty `pos` is the venue declining to say, which is not the
        // same as a zero it did say. Nothing is claimed about a leg that
        // was not reported.
        if pos.trim().is_empty() {
            continue;
        }
        // The venue's own text, kept exactly. `pos` counts contracts and
        // so does `Instrument`'s quantity on this venue — `place` says
        // so: "a quantity is a number of contracts on both venues". A
        // conversion to coins here would be read back through
        // `instrument.qty_scale` as if it were contracts, off by the
        // contract size in the direction nothing downstream can see.
        let magnitude = pos.trim().to_string();
        let amount_text = match leg {
            "SHORT" => format!("-{}", magnitude.trim_start_matches('-')),
            "LONG" => magnitude.trim_start_matches('-').to_string(),
            // Netting keeps whatever sign the venue gave it: here the
            // number is the direction, because the name is not.
            _ => magnitude,
        };
        let Ok(amount) = amount_text.parse::<f64>() else {
            return Err(malformed("position size", &item));
        };
        // A flat leg has no average price and no unrealized profit, and
        // the venue says so with an empty string rather than a zero.
        // That is an absence, not an unreadable number.
        let entry_text = field_str(&item, "avgPx").unwrap_or_default();
        let entry_price = if entry_text.trim().is_empty() {
            0.0
        } else {
            entry_text
                .parse::<f64>()
                .map_err(|_| malformed("position avgPx", &item))?
        };
        let upl = field_str(&item, "upl").unwrap_or_default();
        let unrealized = if upl.trim().is_empty() {
            0.0
        } else {
            upl.parse::<f64>()
                .map_err(|_| malformed("position upl", &item))?
        };
        out.push(crate::binance::PositionSnapshot {
            symbol: inst_id,
            position_side: leg.to_string(),
            amount,
            amount_text,
            entry_text,
            entry_price,
            unrealized,
        });
    }
    Ok(out)
}

/// Read `/api/v5/account/balance` for one settlement currency.
///
/// `read_at_ms` is the local clock at the moment of the read, not the
/// venue's `uTime`: that field says when the account last changed, and a
/// caller asking how fresh its picture is wants the first.
///
/// # Errors
/// When the envelope is a failure, the currency is not in the response,
/// or one of the three balances cannot be read. A missing balance is an
/// error and never a zero — a zero is a number a risk gate acts on.
pub fn parse_balance(
    body: &str,
    ccy: &str,
    read_at_ms: i64,
) -> Result<crate::binance::AccountSnapshot, VenueError> {
    let items = envelope(body, "balance")?;
    let Some(details) = items.first().and_then(|i| array_field(i, "details")) else {
        return Err(malformed("balance details", body));
    };
    let Some(entry) = objects(&details)
        .into_iter()
        .find(|d| field_str(d, "ccy").as_deref() == Some(ccy))
    else {
        return Err(malformed("balance for the settlement currency", body));
    };
    let read = |key: &'static str| -> Result<f64, VenueError> {
        field_str(&entry, key)
            .filter(|v| !v.trim().is_empty())
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| malformed(key, &entry))
    };
    Ok(crate::binance::AccountSnapshot {
        wallet_balance: read("cashBal")?,
        unrealized: read("upl")?,
        margin_balance: read("eq")?,
        read_at_ms,
    })
}

/// Read `/api/v5/trade/orders-pending`.
///
/// Sizes arrive in contracts and stay in them.
///
/// # Errors
/// When the envelope is a failure, or a field this build needs cannot be
/// read — including a side or a state this build does not know, which is
/// refused rather than guessed.
pub fn parse_open_orders(body: &str) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
    let mut out = Vec::new();
    for item in envelope(body, "open orders")? {
        let Some(symbol) = field_str(&item, "instId") else {
            return Err(malformed("order instId", &item));
        };
        // Read as a number to catch a malformed one, kept as the text
        // it arrived as.
        let Some(order_id) = field_str(&item, "ordId").filter(|v| v.parse::<i64>().is_ok()) else {
            return Err(malformed("order ordId", &item));
        };
        let client_order_id = field_str(&item, "clOrdId").unwrap_or_default();
        let Some(side) = field_str(&item, "side").and_then(|v| side_of(&v)) else {
            return Err(malformed("order side", &item));
        };
        let Some(position_side) = field_str(&item, "posSide").and_then(|v| leg_of(&v)) else {
            return Err(malformed("order posSide", &item));
        };
        let Some(status) = field_str(&item, "state").and_then(|v| status_of(&v)) else {
            return Err(malformed("order state", &item));
        };
        // A market order rests at no price, and the venue writes that as
        // an empty string. Zero is the honest reading of "no price"
        // here, and it is what the field means to a caller listing what
        // is resting.
        let price = field_str(&item, "px")
            .filter(|v| !v.trim().is_empty())
            .map_or(Ok(0.0), |v| {
                v.parse::<f64>().map_err(|_| malformed("order px", &item))
            })?;
        let qty = |key: &'static str| -> Result<f64, VenueError> {
            let raw = field_str(&item, key).unwrap_or_default();
            if raw.trim().is_empty() {
                return Ok(0.0);
            }
            // Contracts, like every other size this venue reports.
            raw.parse::<f64>().map_err(|_| malformed(key, &item))
        };
        out.push(crate::binance::OpenOrder {
            symbol,
            order_id,
            client_order_id,
            side: side.to_string(),
            position_side: position_side.to_string(),
            price,
            orig_qty: qty("sz")?,
            executed_qty: qty("accFillSz")?,
            status: status.to_string(),
        });
    }
    Ok(out)
}

/// Whether the account is in hedge mode, from `/api/v5/account/config`.
///
/// # Errors
/// When the envelope is a failure, or `posMode` is missing or is a value
/// this build does not know. Not defaulted: starting a hedged strategy
/// against a netting account is the kind of mistake that is only visible
/// after it has closed a position it meant to open.
pub fn parse_position_mode(body: &str) -> Result<bool, VenueError> {
    let items = envelope(body, "account config")?;
    let Some(mode) = items.first().and_then(|i| field_str(i, "posMode")) else {
        return Err(malformed("posMode", body));
    };
    match mode.as_str() {
        "long_short_mode" => Ok(true),
        "net_mode" => Ok(false),
        _ => Err(malformed("posMode", body)),
    }
}

/// The venue's clock, from `/api/v5/public/time`.
///
/// # Errors
/// When the envelope is a failure or `ts` cannot be read.
pub fn parse_server_time(body: &str) -> Result<i64, VenueError> {
    let items = envelope(body, "server time")?;
    items
        .first()
        .and_then(|i| field_str(i, "ts"))
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| malformed("server time", body))
}

#[cfg(test)]
mod account_reads {
    use super::*;

    #[test]
    fn a_failure_arrives_as_http_200_and_is_not_an_empty_account() {
        // The trap this venue sets. An adapter that read `data` without
        // reading `code` would report a flat account here, and a flat
        // account is an instruction to open a position.
        let body = r#"{"code":"50011","msg":"Too many requests","data":[]}"#;
        let e = parse_positions(body).expect_err("a refusal is not a flat account");
        assert!(
            matches!(e, VenueError::Venue { status: 200, .. }),
            "a non-zero code must surface as the venue refusing: {e:?}"
        );
    }

    #[test]
    fn a_position_reads_back_through_the_instruments_own_scale() {
        // The defect this test exists for: sizes were converted into
        // coins here, and `oq-live` reads them back with
        // `instrument.qty_scale` — which on this venue counts
        // *contracts*, because `Execution::place` sends `order.qty` as a
        // contract count. Five contracts came back as "0.05000000", and
        // 0.05 read at a contract scale is not five of anything.
        //
        // BTC-USDT-SWAP: one contract is 0.01 BTC and the lot size is
        // 0.01 contracts, so the instrument's quantity scale is 2.
        const QTY_SCALE: u8 = 2;
        let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP",
            "posSide":"long","pos":"5","avgPx":"78313.4","upl":"-0.5"}]}"#;
        let legs = parse_positions(body).expect("a readable position");
        // What `adopted_lots` does to the text this returns.
        let lots = crate::klines::scaled(&legs[0].amount_text, QTY_SCALE)
            .expect("the venue's text is a number");
        assert_eq!(
            lots, 500,
            "five contracts at a scale of two is 500 lots; anything else \
             means the unit changed somewhere between here and the book"
        );
    }

    #[test]
    fn a_legs_direction_comes_from_its_name_not_from_the_sign() {
        // The same short leg, reported both ways round. This adapter
        // reads both identically, so a venue that changes which one it
        // sends does not silently invert a position.
        for pos in ["5", "-5"] {
            let body = format!(
                r#"{{"code":"0","msg":"","data":[{{"instType":"SWAP","instId":"BTC-USDT-SWAP","posSide":"short","pos":"{pos}","avgPx":"76880.6","upl":"-21.5"}}]}}"#
            );
            let legs = parse_positions(&body).expect("a readable position");
            assert_eq!(legs.len(), 1);
            assert_eq!(legs[0].position_side, "SHORT");
            assert_eq!(legs[0].amount_text, "-5", "pos was {pos:?}");
            assert!((legs[0].amount - -5.0).abs() < 1e-12, "pos was {pos:?}");
            assert!((legs[0].entry_price - 76_880.6).abs() < 1e-9);
        }
    }

    #[test]
    fn a_flat_leg_is_kept_and_an_unreported_one_is_not_invented() {
        // A leg at zero is a fact about the account. Dropping it once
        // told a model that a position it had just closed was absent,
        // which is the failure `account.rs` records.
        let body = r#"{"code":"0","msg":"","data":[
            {"instId":"BTC-USDT-SWAP","posSide":"long","pos":"0","avgPx":"","upl":""},
            {"instId":"BTC-USDT-SWAP","posSide":"short","pos":"","avgPx":"","upl":""}
        ]}"#;
        let legs = parse_positions(body).expect("a readable position");
        assert_eq!(legs.len(), 1, "the empty leg is silence, not a zero");
        assert_eq!(legs[0].position_side, "LONG");
        assert_eq!(legs[0].amount, 0.0);
        // No average price on a flat leg is an absence, not a bad number.
        assert_eq!(legs[0].entry_price, 0.0);
        assert_eq!(legs[0].entry_text, "");
    }

    #[test]
    fn a_position_mode_this_build_does_not_know_is_refused() {
        let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","posSide":"sideways","pos":"1"}]}"#;
        assert!(
            parse_positions(body).is_err(),
            "an unrecognised leg must not become BOTH"
        );
    }

    #[test]
    fn a_balance_that_cannot_be_read_is_an_error_and_never_a_zero() {
        // `upl` absent. Zero is a number a risk gate acts on, so the
        // read fails instead of inventing one.
        let body = r#"{"code":"0","msg":"","data":[{"totalEq":"5218","details":[
            {"ccy":"USDT","eq":"5196.35","cashBal":"5218.03","availBal":"3225.6"}]}]}"#;
        assert!(parse_balance(body, "USDT", 1).is_err());
    }

    #[test]
    fn a_balance_is_read_for_the_currency_that_was_asked_for() {
        let body = r#"{"code":"0","msg":"","data":[{"details":[
            {"ccy":"BTC","eq":"1","cashBal":"1","upl":"0"},
            {"ccy":"USDT","eq":"5196.35","cashBal":"5218.03","upl":"-21.68"}]}]}"#;
        let snap = parse_balance(body, "USDT", 42).expect("a readable balance");
        assert!((snap.wallet_balance - 5218.03).abs() < 1e-9);
        assert!((snap.unrealized - -21.68).abs() < 1e-9);
        assert!((snap.margin_balance - 5196.35).abs() < 1e-9);
        // The local read time, not the venue's account-update time.
        assert_eq!(snap.read_at_ms, 42);
        // A currency the account does not hold is not a zero balance.
        assert!(parse_balance(body, "ETH", 1).is_err());
    }

    #[test]
    fn an_orders_vocabulary_is_translated_rather_than_passed_through() {
        // `oq-live` matches on FILLED and CANCELED. An adapter that sent
        // `live` through would produce a caller that never sees an order
        // end.
        let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP",
            "ordId":"312269865356374016","clOrdId":"oq0001","px":"78000","sz":"5",
            "accFillSz":"1","side":"buy","posSide":"long","state":"partially_filled"}]}"#;
        let orders = parse_open_orders(body).expect("a readable order");
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].side, "BUY");
        assert_eq!(orders[0].position_side, "LONG");
        assert_eq!(orders[0].status, "PARTIALLY_FILLED");
        assert_eq!(orders[0].order_id, "312269865356374016");
        // Sizes are contracts here too.
        assert!((orders[0].orig_qty - 5.0).abs() < 1e-12);
        assert!((orders[0].executed_qty - 1.0).abs() < 1e-12);
    }

    #[test]
    fn an_order_state_this_build_does_not_know_is_refused() {
        let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ordId":"1",
            "clOrdId":"a","px":"1","sz":"1","accFillSz":"0","side":"buy","posSide":"long",
            "state":"something_new"}]}"#;
        assert!(
            parse_open_orders(body).is_err(),
            "an unknown state must not be reported as resting"
        );
    }

    #[test]
    fn hedge_mode_is_read_and_an_unknown_mode_is_refused() {
        let hedged = r#"{"code":"0","msg":"","data":[{"posMode":"long_short_mode","uid":"1"}]}"#;
        let netting = r#"{"code":"0","msg":"","data":[{"posMode":"net_mode","uid":"1"}]}"#;
        assert!(parse_position_mode(hedged).expect("a readable mode"));
        assert!(!parse_position_mode(netting).expect("a readable mode"));
        let unknown = r#"{"code":"0","msg":"","data":[{"posMode":"whatever","uid":"1"}]}"#;
        assert!(parse_position_mode(unknown).is_err());
    }

    #[test]
    fn the_venue_clock_is_read_from_its_own_envelope() {
        let body = r#"{"code":"0","msg":"","data":[{"ts":"1597026383085"}]}"#;
        assert_eq!(
            parse_server_time(body).expect("a readable time"),
            1_597_026_383_085
        );
    }
}

/// Read `/api/v5/market/candles`.
///
/// Two differences from the venue this workspace met first, and both are
/// silent when missed.
///
/// **The rows arrive newest first.** A warm-up fed backwards computes
/// its indicators on time running the wrong way and returns numbers
/// rather than an error, so they are reversed here — once, where the
/// venue's order is known.
///
/// **`vol` counts contracts**, like every other size on this venue, and
/// is kept that way: `qty_scale` counts contracts too.
///
/// Field positions happen to match the first venue's — timestamp, open,
/// high, low, close, volume — which is worth saying out loud, because it
/// means an index copied from there is right for the wrong reason.
///
/// # Errors
/// When the envelope is a failure, or a row this build needs cannot be
/// read.
pub fn parse_candles(
    body: &str,
    price_scale: u8,
    qty_scale: u8,
) -> Result<Vec<crate::klines::Kline>, VenueError> {
    let Some(code) = field_str(body, "code") else {
        return Err(malformed("candles", body));
    };
    if code != "0" {
        return Err(VenueError::Venue {
            status: 200,
            body: body.to_string(),
        });
    }
    let Some(data) = array_field(body, "data") else {
        return Err(malformed("candles", body));
    };
    // `rows` splits an array of arrays and counts from the outside, so
    // it wants the brackets `array_field` just removed. Put them back
    // rather than teaching it a second shape.
    let rows = crate::klines::rows(&format!("[{data}]"));
    if rows.is_empty() {
        return Err(malformed("candles", body));
    }
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.len() < 6 {
            return Err(malformed("candle row", body));
        }
        let at = |i: usize, scale: u8| -> Result<i64, VenueError> {
            crate::klines::scaled(&r[i], scale).ok_or_else(|| malformed("candle field", body))
        };
        out.push(crate::klines::Kline {
            open_ms: r[0]
                .parse::<i64>()
                .map_err(|_| malformed("candle timestamp", body))?,
            high: at(2, price_scale)?,
            low: at(3, price_scale)?,
            close: at(4, price_scale)?,
            volume: at(5, qty_scale)?,
        });
    }
    // Oldest first, which is the order a warm-up replays in.
    out.reverse();
    Ok(out)
}

impl Okx {
    /// Recent one-minute bars, for a warm-up.
    ///
    /// Unsigned: history is public, so a warm-up cannot fail for a
    /// reason that has anything to do with this account's keys.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports.
    pub fn recent_bars(
        &self,
        inst_id: &str,
        minutes: usize,
    ) -> Result<Vec<crate::klines::Kline>, VenueError> {
        let listing = self.listing(inst_id)?;
        // The venue caps a page at 300. Clamped rather than paged: a
        // silent second request would make the range that comes back
        // differ from the one that was asked for.
        let limit = minutes.clamp(1, 300);
        let body = self.get_public(
            "/api/v5/market/candles",
            &format!("instId={inst_id}&bar=1m&limit={limit}"),
        )?;
        parse_candles(&body, listing.price_scale, listing.size_scale)
    }
}

/// The shape the engine trades against, from a listing.
///
/// Pure, so the unit decision is testable without a socket — and it is
/// the decision most worth pinning: `sized` rather than `linear`,
/// because a quantity here counts contracts and `contract_size` says
/// what one is worth. `linear` would declare that a quantity *is* the
/// underlying, which is the hundredfold error stated as a type.
#[must_use]
pub fn instrument_of(listing: &Listing) -> Instrument {
    Instrument::sized(
        listing.price_scale,
        listing.size_scale,
        listing.contract_value,
    )
    .with_grid(listing.price_tick, listing.lot_size)
}

impl crate::account::Account for Okx {
    fn id(&self) -> &'static str {
        // Matches the market-data side's name for the same venue, so a
        // run's records and its archive file under one name.
        "okx-swap"
    }

    fn id_rules(&self) -> crate::broker::IdRules {
        crate::broker::IdRules::OKX
    }

    fn recent_bars(
        &self,
        symbol: &str,
        minutes: usize,
    ) -> Result<Vec<crate::klines::Kline>, VenueError> {
        Self::recent_bars(self, symbol, minutes)
    }

    fn sync_clock(&mut self) -> Result<i64, VenueError> {
        Self::sync_clock(self)
    }

    fn round_trip_ms(&self) -> i64 {
        Self::round_trip_ms(self)
    }

    /// The listing, as the shape the engine trades against.
    ///
    /// `sized` rather than `linear`: a quantity here is a count of
    /// contracts and `contract_size` is what one is worth, which is the
    /// distinction `Execution::place` depends on.
    ///
    /// The listing's `min_size` has nowhere to go — `Instrument` carries
    /// a minimum notional, and a minimum contract count is not one.
    /// `Listing::size_text` still enforces it for a caller converting a
    /// coin amount, so the floor is checked on the path that has the
    /// listing and unchecked on the path that has only this. Recorded
    /// rather than papered over with a converted number that would be
    /// wrong whenever the price moved.
    fn instrument(&self, symbol: &str) -> Result<Instrument, String> {
        let listing = self.listing(symbol).map_err(|e| e.to_string())?;
        Ok(instrument_of(&listing))
    }

    fn is_hedged(&self) -> Result<bool, VenueError> {
        self.is_hedged_account()
    }

    fn positions(&self, symbol: &str) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
        Self::positions(self, symbol)
    }

    /// The settlement currency is USDT, because these are USDT-margined
    /// swaps. A USDC-margined or coin-margined account settles in
    /// something else and would need this to say so rather than to
    /// report a balance the account does not hold.
    fn balances(&self) -> Result<crate::binance::AccountSnapshot, VenueError> {
        Self::balances(self, "USDT")
    }

    fn open_orders(&self, symbol: &str) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
        Self::open_orders(self, symbol)
    }

    fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        self.user_stream()
    }

    /// Nothing to renew.
    ///
    /// This venue authenticates the socket rather than issuing a bearer
    /// token, so there is no key with an expiry and no request that
    /// extends one. A no-op rather than an error: the caller's schedule
    /// is correct, there is simply nothing for it to do here.
    fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        Ok(())
    }

    /// Nothing to close, for the same reason. The stream ends when the
    /// socket does.
    fn close_user_stream(&self) -> Result<(), VenueError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------
// The private channel.
//
// The socket opens unauthenticated and stays that way until it is told
// otherwise, which is the difference `UserStream::with_opening` exists
// for. Everything here is pure: the frames are text and the answers are
// a function of one message.
// ---------------------------------------------------------------------

/// Where the private channel lives.
///
/// Demo trading is a *different host* here, unlike the REST side where
/// it is the same host and a header. Two mechanisms for one distinction
/// is the venue's choice, not this adapter's, and the cost of getting it
/// wrong is opposite in each direction: the wrong host fails loudly, the
/// missing header trades live money.
pub const PRIVATE_WS_LIVE: &str = "wss://ws.okx.com:8443/ws/v5/private";
/// The demo trading private channel.
pub const PRIVATE_WS_DEMO: &str = "wss://wspap.okx.com:8443/ws/v5/private";

/// The `login` frame, signed.
///
/// The signature covers `timestamp + "GET" + "/users/self/verify"` with
/// an empty body, and `timestamp` is **seconds** since the epoch — not
/// the ISO text the REST side signs. One venue, two timestamp formats,
/// and a frame signed with the wrong one is refused with a message that
/// says only that the login failed.
#[must_use]
pub fn login_frame(key: &str, passphrase: &str, secret: &[u8], now_seconds: i64) -> String {
    let timestamp = now_seconds.to_string();
    let signature = sign(secret, &timestamp, "GET", "/users/self/verify", "");
    format!(
        r#"{{"op":"login","args":[{{"apiKey":"{key}","passphrase":"{passphrase}","timestamp":"{timestamp}","sign":"{signature}"}}]}}"#
    )
}

/// What one message says about a `login`.
///
/// A refusal arrives as an `error` event carrying a code, in the same
/// place the confirmation would, which is why the caller is given three
/// outcomes and not two.
#[must_use]
pub fn login_answer(message: &str) -> Handshake {
    let event = field_str(message, "event");
    match event.as_deref() {
        Some("login") => {
            // A `login` event with a non-zero code is a refusal wearing
            // the confirmation's name.
            if field_str(message, "code").as_deref() == Some("0") {
                Handshake::Confirmed
            } else {
                Handshake::Refused
            }
        }
        Some("error") => Handshake::Refused,
        // Connection-count notices and anything else the venue chooses
        // to say first.
        _ => Handshake::Unrelated,
    }
}

/// Subscribe to this account's order updates for one instrument type.
#[must_use]
pub fn subscribe_orders_frame(inst_type: &str) -> String {
    format!(r#"{{"op":"subscribe","args":[{{"channel":"orders","instType":"{inst_type}"}}]}}"#)
}

/// What one message says about that subscription.
///
/// Waited on rather than assumed. A subscription that silently failed
/// leaves a socket that is logged in, connected, and delivering
/// nothing — which is the failure this whole handshake exists to make
/// impossible to mistake for a quiet account.
#[must_use]
pub fn subscribe_answer(message: &str) -> Handshake {
    match field_str(message, "event").as_deref() {
        Some("subscribe") => Handshake::Confirmed,
        Some("error") => Handshake::Refused,
        _ => Handshake::Unrelated,
    }
}

/// OKX's reader.
///
/// Holds nothing. It briefly held a contract value, on the belief that a
/// fill's size had to be converted into coins — see the commit that
/// removed it. Sizes stay in contracts, because that is the unit
/// `Instrument` counts in on this venue, so there is nothing to look up.
#[derive(Debug, Clone, Copy, Default)]
pub struct Events;

impl crate::exec::Events for Events {
    fn read(&self, message: &str) -> Vec<UserEvent> {
        parse_user_events(message)
    }
}

/// Every account event one frame from the `orders` channel carries.
///
/// A list because this venue batches: `data` is an array, and one frame
/// can report several fills. Returning the first would lose the rest,
/// and a lost fill is a position that never existed.
#[must_use]
pub fn parse_user_events(message: &str) -> Vec<UserEvent> {
    // A frame carrying `event` is the venue talking about the
    // subscription — an acknowledgement, an error, a connection count —
    // not about the account.
    if field_str(message, "event").is_some() {
        return Vec::new();
    }
    let Some(channel) = field_str(message, "channel") else {
        return Vec::new();
    };
    if channel != "orders" {
        // An account event this build does not map. Kept rather than
        // dropped: a venue that adds a channel should produce something
        // a reader can see.
        return vec![UserEvent::Other {
            kind: channel,
            payload: message.to_string(),
        }];
    }
    let Some(data) = array_field(message, "data") else {
        return Vec::new();
    };
    objects(&data)
        .into_iter()
        .map(|item| match read_order_update(&item) {
            Some(update) => UserEvent::Order(update),
            // Unreadable, not absent. The payload survives so the
            // difference stays visible downstream.
            None => UserEvent::Other {
                kind: "orders".to_string(),
                payload: item,
            },
        })
        .collect()
}

/// One entry of the `orders` channel's `data` array.
fn read_order_update(item: &str) -> Option<OrderUpdate> {
    // The venue's own text, kept. Contracts, like every other size here,
    // and `run.rs` reads it back through `instrument.qty_scale`, which
    // counts contracts on this venue too.
    let qty = |key: &str| -> String {
        let raw = field_str(item, key).unwrap_or_default();
        if raw.trim().is_empty() {
            return "0".to_string();
        }
        raw
    };
    // The venue's own orders say so in `category`; everything else is
    // `normal`, `twap` or `ddh`, all placed from the account.
    let initiator = match field_str(item, "category").as_deref() {
        Some("full_liquidation" | "partial_liquidation") => crate::exec::Initiator::Liquidation,
        Some("adl") => crate::exec::Initiator::Adl,
        Some("delivery") => crate::exec::Initiator::Settlement,
        _ => crate::exec::Initiator::Account,
    };
    Some(OrderUpdate {
        initiator,
        symbol: field_str(item, "instId")?,
        client_id: field_str(item, "clOrdId").unwrap_or_default(),
        venue_id: field_str(item, "ordId")?,
        status: status_of(&field_str(item, "state")?)?.to_string(),
        last_qty: qty("fillSz"),
        cumulative_qty: qty("accFillSz"),
        last_price: field_str(item, "fillPx")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "0".to_string()),
        side: side_of(&field_str(item, "side")?)?.to_string(),
        position_side: leg_of(&field_str(item, "posSide")?)?.to_string(),
        // `T` is taker, `M` is maker. Absent means this update is not a
        // fill, and taker is the safe reading: crediting a maker rebate
        // to an order that paid is the error that flatters a backtest.
        maker: field_str(item, "execType").as_deref() == Some("M"),
        // Empty on an update that is not a fill. That and a zero must
        // both be `None`, or a deduplication table acquires an entry
        // that swallows every subsequent non-fill.
        trade_id: field_str(item, "tradeId")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|id| *id > 0),
        event_ms: field_str(item, "uTime")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or_default(),
    })
}

impl Okx {
    /// The private channel, with everything that has to be said on it.
    ///
    /// There is no key to renew: this venue authenticates the socket
    /// rather than issuing a bearer token, so the `UserStream`'s key is
    /// empty and renewal is a no-op. That is a real difference and not
    /// an omission — a stream here dies with its connection, not with a
    /// clock.
    ///
    /// # Errors
    /// When the credentials carry no passphrase, which this venue needs
    /// and which is not a signature problem however the venue reports it.
    pub fn user_stream(&self) -> Result<UserStream, VenueError> {
        let Some(passphrase) = self.creds.passphrase() else {
            return Err(VenueError::Transport(
                "OKX needs a passphrase as well as a key and a secret; \
                 set OQ_VENUE_PASSPHRASE or use Credentials::with_passphrase"
                    .to_string(),
            ));
        };
        let url = if self.simulated {
            PRIVATE_WS_DEMO
        } else {
            PRIVATE_WS_LIVE
        };
        let seconds = (now_ms() + self.clock_offset_ms) / 1_000;
        Ok(
            UserStream::new(url.to_string(), String::new(), std::sync::Arc::new(Events))
                .with_opening(vec![
                    Opening::awaited(
                        login_frame(
                            self.creds.key(),
                            passphrase,
                            self.creds.secret_bytes(),
                            seconds,
                        ),
                        login_answer,
                    ),
                    Opening::awaited(subscribe_orders_frame("SWAP"), subscribe_answer),
                ]),
        )
    }
}

#[cfg(test)]
mod account_stream {
    use super::*;
    use crate::exec::Events as _;

    /// One frame, two fills. The reason `read` answers a list.
    const TWO_FILLS: &str = r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[
        {"instId":"BTC-USDT-SWAP","ordId":"1","clOrdId":"oq1","state":"partially_filled",
         "side":"buy","posSide":"long","fillSz":"1","fillPx":"78000","accFillSz":"1",
         "tradeId":"501","execType":"M","uTime":"1700000000001"},
        {"instId":"BTC-USDT-SWAP","ordId":"1","clOrdId":"oq1","state":"filled",
         "side":"buy","posSide":"long","fillSz":"4","fillPx":"78010","accFillSz":"5",
         "tradeId":"502","execType":"T","uTime":"1700000000002"}]}"#;

    /// The venue's own orders say so in `category`.
    #[test]
    fn orders_the_venue_placed_itself_are_named() {
        use crate::exec::Initiator;
        for (category, want) in [
            ("normal", Initiator::Account),
            ("twap", Initiator::Account),
            ("full_liquidation", Initiator::Liquidation),
            ("partial_liquidation", Initiator::Liquidation),
            ("adl", Initiator::Adl),
            ("delivery", Initiator::Settlement),
        ] {
            let frame = TWO_FILLS.replacen(
                r#""clOrdId":"oq1","#,
                &format!(r#""clOrdId":"oq1","category":"{category}","#),
                1,
            );
            let UserEvent::Order(u) = &parse_user_events(&frame)[0] else {
                panic!("an order update");
            };
            assert_eq!(u.initiator, want, "{category}");
        }
    }

    #[test]
    fn a_frame_with_two_fills_produces_two_events() {
        let events = parse_user_events(TWO_FILLS);
        assert_eq!(
            events.len(),
            2,
            "returning the first would lose a fill, which is a position that never existed"
        );
        let (first, second) = (&events[0], &events[1]);
        let (UserEvent::Order(a), UserEvent::Order(b)) = (first, second) else {
            panic!("both entries are order updates: {events:?}");
        };
        // Sizes are contracts on the wire and the underlying here.
        assert_eq!(a.last_qty, "1");
        assert_eq!(b.last_qty, "4");
        assert_eq!(b.cumulative_qty, "5");
        // The vocabulary is translated, as it is on the REST side.
        assert_eq!(a.status, "PARTIALLY_FILLED");
        assert_eq!(b.status, "FILLED");
        assert_eq!(a.side, "BUY");
        assert_eq!(a.position_side, "LONG");
        // execType M is maker, T is taker.
        assert!(a.maker);
        assert!(!b.maker);
        assert_eq!(a.trade_id, Some(501));
        assert_eq!(b.event_ms, 1_700_000_000_002);
    }

    #[test]
    fn the_reader_keeps_the_venues_own_sizes() {
        // It used to convert these into coins, which was wrong: `run.rs`
        // reads a fill's size back through `instrument.qty_scale`, and
        // on this venue that scale counts contracts. The conversion was
        // off by the contract size, in the direction nothing downstream
        // could see.
        let events = Events.read(TWO_FILLS);
        let UserEvent::Order(a) = &events[0] else {
            panic!("an order update");
        };
        assert_eq!(a.last_qty, "1", "the venue said one contract");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_subscription_acknowledgement_is_not_an_account_event() {
        // It arrives on the same socket and must produce nothing.
        assert!(
            parse_user_events(r#"{"event":"subscribe","arg":{"channel":"orders"}}"#).is_empty()
        );
        assert!(parse_user_events(r#"{"event":"error","code":"60012"}"#).is_empty());
    }

    #[test]
    fn an_update_with_no_trade_id_is_not_given_one() {
        // A cancellation carries an empty tradeId. Mapping it to zero
        // would give a deduplication table an entry that swallows every
        // later non-fill.
        let body = r#"{"arg":{"channel":"orders"},"data":[{"instId":"BTC-USDT-SWAP",
            "ordId":"9","clOrdId":"oq9","state":"canceled","side":"sell","posSide":"short",
            "fillSz":"","fillPx":"","accFillSz":"0","tradeId":"","uTime":"1700000000003"}]}"#;
        let events = parse_user_events(body);
        let UserEvent::Order(u) = &events[0] else {
            panic!("an order update");
        };
        assert_eq!(u.trade_id, None);
        assert_eq!(u.status, "CANCELED");
        assert_eq!(u.last_qty, "0");
        assert_eq!(u.last_price, "0");
    }

    #[test]
    fn an_entry_that_cannot_be_read_survives_as_itself() {
        // Unreadable is not absent: the payload is kept so a venue that
        // changed something produces evidence rather than silence.
        let body = r#"{"arg":{"channel":"orders"},"data":[{"instId":"BTC-USDT-SWAP",
            "ordId":"9","state":"teleported","side":"buy","posSide":"long"}]}"#;
        let events = parse_user_events(body);
        assert_eq!(events.len(), 1);
        let UserEvent::Other { kind, payload } = &events[0] else {
            panic!("an unreadable entry is kept, not dropped: {events:?}");
        };
        assert_eq!(kind, "orders");
        assert!(payload.contains("teleported"));
    }

    #[test]
    fn a_channel_this_build_does_not_map_is_kept_rather_than_dropped() {
        let body = r#"{"arg":{"channel":"positions"},"data":[{"instId":"BTC-USDT-SWAP"}]}"#;
        let events = parse_user_events(body);
        let UserEvent::Other { kind, .. } = &events[0] else {
            panic!("kept as itself");
        };
        assert_eq!(kind, "positions");
    }
}

#[cfg(test)]
mod account_trait {
    use super::*;

    /// BTC-USDT-SWAP as the venue lists it: one contract is 0.01 BTC,
    /// the price grid is 0.1, the lot size is 0.01 contracts.
    fn btc_listing() -> Listing {
        Listing {
            inst_id: "BTC-USDT-SWAP".to_string(),
            contract_value: 1_000_000,
            price_scale: 1,
            price_tick: 1,
            size_scale: 2,
            lot_size: 1,
            min_size: 1,
        }
    }

    #[test]
    fn a_quantity_is_a_contract_count_and_the_instrument_says_so() {
        let i = instrument_of(&btc_listing());
        // `linear` would say a quantity is the underlying, which is the
        // hundredfold error written as a type.
        assert_eq!(
            i.contract_size, 1_000_000,
            "one contract is 0.01 BTC at CONTRACT_SCALE"
        );
        assert_eq!(i.qty_scale, 2);
        assert_eq!(i.price_scale, 1);
        assert_eq!(i.price_tick, 1);
        assert_eq!(i.qty_step, 1);
        // One lot is one hundredth of a contract, which is a ten
        // thousandth of a BTC. The engine gets that from the pair.
        assert!(i.qty_on_grid(oq_types::QtyLots(500)));
    }

    /// Newest first, which is the order this venue answers in.
    const CANDLES: &str = r#"{"code":"0","msg":"","data":[
        ["1700000120000","78100","78200","78050","78150","10","0.1","7815","1"],
        ["1700000060000","78000","78150","77950","78100","20","0.2","15620","1"]]}"#;

    #[test]
    fn candles_come_back_newest_first_and_are_replayed_oldest_first() {
        let bars = parse_candles(CANDLES, 1, 2).expect("readable candles");
        assert_eq!(bars.len(), 2);
        assert_eq!(
            bars[0].open_ms, 1_700_000_060_000,
            "a warm-up replays forwards; fed backwards it computes \
             indicators on time running the wrong way and returns a \
             number rather than an error"
        );
        assert_eq!(bars[1].open_ms, 1_700_000_120_000);
        // Prices at the listing's scale: 78150.0 is 781500 at one dp.
        assert_eq!(bars[1].close, 781_500);
        assert_eq!(bars[1].high, 782_000);
        assert_eq!(bars[1].low, 780_500);
        // Volume stays in contracts, like every other size here: 10
        // contracts at a scale of two.
        assert_eq!(bars[1].volume, 1_000);
    }

    #[test]
    fn a_candle_refusal_is_not_an_empty_history() {
        // The same HTTP-200 trap as everywhere else on this venue.
        let body = r#"{"code":"51001","msg":"Instrument ID does not exist","data":[]}"#;
        assert!(parse_candles(body, 1, 2).is_err());
        // And an envelope with no rows is a failed read, not a market
        // that has never traded.
        assert!(parse_candles(r#"{"code":"0","data":[]}"#, 1, 2).is_err());
    }

    #[test]
    fn the_adapter_names_itself_the_way_the_capture_side_does() {
        use crate::account::Account as _;
        let okx = Okx::at(Endpoint::Testnet, Credentials::new("k", "s"));
        assert_eq!(okx.id(), "okx-swap");
        // Narrower than the first venue's, so an id that works there is
        // not guaranteed here.
        assert_eq!(okx.id_rules(), crate::broker::IdRules::OKX);
        assert_eq!(okx.id_rules().max_len(), Some(32));
        // Narrower than the first venue's: no punctuation.
        assert!(!okx.id_rules().accepts("oq-1"));
        assert!(okx.id_rules().accepts("oq1"));
    }

    #[test]
    fn a_stream_here_needs_neither_renewal_nor_closing() {
        use crate::account::Account as _;
        // Not an omission: this venue authenticates the socket instead
        // of issuing a token, so there is no expiry to outrun.
        let okx = Okx::at(Endpoint::Testnet, Credentials::new("k", "s"));
        assert!(okx.keepalive_user_stream().is_ok());
        assert!(okx.close_user_stream().is_ok());
    }
}

#[cfg(test)]
mod private_channel {
    use super::*;

    #[test]
    fn a_login_is_signed_over_the_verify_path_with_a_seconds_timestamp() {
        let frame = login_frame("thekey", "thepass", b"secret", 1_538_054_050);
        // The timestamp goes in as seconds, not as the ISO text the
        // REST side signs.
        assert!(frame.contains(r#""timestamp":"1538054050""#), "{frame}");
        assert!(frame.contains(r#""apiKey":"thekey""#));
        assert!(frame.contains(r#""passphrase":"thepass""#));
        // The signature is over the documented message, and it is the
        // whole of it: a signature computed over anything else is
        // refused with a message that says only "Login failed".
        let expected = sign(b"secret", "1538054050", "GET", "/users/self/verify", "");
        assert!(
            frame.contains(&format!(r#""sign":"{expected}""#)),
            "{frame}"
        );
    }

    #[test]
    fn a_login_event_with_a_bad_code_is_a_refusal_wearing_the_confirmations_name() {
        assert_eq!(
            login_answer(r#"{"event":"login","code":"0","msg":""}"#),
            Handshake::Confirmed
        );
        assert_eq!(
            login_answer(r#"{"event":"login","code":"60009","msg":"Login failed."}"#),
            Handshake::Refused
        );
        assert_eq!(
            login_answer(r#"{"event":"error","code":"60009","msg":"Login failed."}"#),
            Handshake::Refused
        );
        // The venue says this before it says anything useful.
        assert_eq!(
            login_answer(r#"{"event":"channel-conn-count","channel":"orders","connCount":"1"}"#),
            Handshake::Unrelated
        );
        // An order update is not an answer to a login.
        assert_eq!(
            login_answer(r#"{"arg":{"channel":"orders"},"data":[{"ordId":"1"}]}"#),
            Handshake::Unrelated
        );
    }

    #[test]
    fn a_subscription_is_waited_on_rather_than_assumed() {
        assert_eq!(
            subscribe_answer(r#"{"event":"subscribe","arg":{"channel":"orders"}}"#),
            Handshake::Confirmed
        );
        assert_eq!(
            subscribe_answer(r#"{"event":"error","code":"60012","msg":"Invalid request"}"#),
            Handshake::Refused
        );
        assert_eq!(
            subscribe_answer(r#"{"arg":{"channel":"orders"},"data":[]}"#),
            Handshake::Unrelated
        );
    }

    #[test]
    fn demo_trading_is_a_different_host_here() {
        let creds = Credentials::new("k", "s")
            .with_passphrase("p")
            .expect("a non-empty passphrase");
        let demo = Okx::at(Endpoint::Testnet, creds.clone())
            .user_stream()
            .expect("a passphrase was given");
        assert_eq!(demo.url(), PRIVATE_WS_DEMO);
        let live = Okx::at(Endpoint::Live, creds)
            .user_stream()
            .expect("a passphrase was given");
        assert_eq!(live.url(), PRIVATE_WS_LIVE);
        // Login first, then the subscription that depends on it.
        assert_eq!(live.opening().len(), 2);
    }

    #[test]
    fn a_stream_without_a_passphrase_is_refused_before_it_is_opened() {
        let creds = Credentials::new("k", "s");
        assert!(
            Okx::at(Endpoint::Testnet, creds).user_stream().is_err(),
            "an unsigned login would be refused by the venue as a bad signature"
        );
    }

    #[test]
    fn a_stream_here_has_no_key_to_renew() {
        let creds = Credentials::new("k", "s")
            .with_passphrase("p")
            .expect("a non-empty passphrase");
        let stream = Okx::at(Endpoint::Testnet, creds)
            .user_stream()
            .expect("a passphrase was given");
        assert_eq!(
            stream.key(),
            "",
            "this venue authenticates the socket rather than issuing a token"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::PriceTicks;

    /// The venue's own answer for BTC-USDT-SWAP, fetched from
    /// `/api/v5/public/instruments` on 2026-08-18 and trimmed to the
    /// fields this build reads. Kept verbatim rather than hand-written:
    /// the first version of this adapter assumed whole contracts, and it
    /// was this payload's `lotSz` that said otherwise.
    const LISTING: &str = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","instType":"SWAP","ctVal":"0.01","ctValCcy":"BTC","ctMult":"1","ctType":"linear","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","settleCcy":"USDT","lever":"100","state":"live"}]}"#;

    fn listing() -> Listing {
        parse_listing(LISTING, "BTC-USDT-SWAP").expect("the venue's own listing")
    }

    fn instrument() -> Instrument {
        listing().instrument()
    }

    fn order() -> NewOrder {
        NewOrder {
            symbol: "BTC-USDT-SWAP".to_string(),
            side: Side::Buy,
            limit_price: Some(PriceTicks(600_000)),
            // Five contracts, at the venue's own size scale.
            qty: QtyLots(500),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq0001".to_string(),
            reduce_only: false,
            position_side: PositionSide::OneWay,
        }
    }

    // -- 1. a 200 is not an acceptance --------------------------------

    /// The difference this adapter exists to get right. The venue
    /// answers a refused order with HTTP 200, and an adapter that reads
    /// the status line books a position that does not exist.
    #[test]
    fn a_refusal_arriving_as_http_200_is_a_refusal() {
        let body = r#"{"code":"1","msg":"","data":[{"clOrdId":"oq0001","ordId":"","sCode":"51008","sMsg":"Order placement failed due to insufficient balance"}]}"#;
        match classify(200, body, "oq0001") {
            Placed::Rejected(r) => {
                assert_eq!(r.code, Some(51_008));
                assert!(r.message.contains("insufficient balance"), "{r:?}");
            }
            other => panic!("a 200 with sCode 51008 must be a rejection, got {other:?}"),
        }
    }

    /// And the envelope alone is not enough either: the request can
    /// succeed while the order in it was refused.
    #[test]
    fn an_envelope_that_succeeded_does_not_make_the_order_accepted() {
        let body = r#"{"code":"0","msg":"","data":[{"clOrdId":"oq0001","ordId":"","sCode":"51000","sMsg":"Parameter sz error"}]}"#;
        assert!(
            matches!(classify(200, body, "oq0001"), Placed::Rejected(_)),
            "code 0 with sCode 51000 is a refused order inside a successful request"
        );
    }

    #[test]
    fn an_accepted_order_is_accepted() {
        let body = r#"{"code":"0","msg":"","data":[{"clOrdId":"oq0001","ordId":"312269865356374016","tag":"","sCode":"0","sMsg":""}]}"#;
        match classify(200, body, "oq0001") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, "312269865356374016");
                assert_eq!(a.client_id, "oq0001");
            }
            other => panic!("expected an acceptance, got {other:?}"),
        }
    }

    /// A signature failure has no per-order detail, and must not be
    /// reported as unknown: it is final, and retrying is pointless.
    #[test]
    fn a_whole_request_refusal_is_rejected_not_unknown() {
        let body = r#"{"code":"50113","msg":"Invalid Sign","data":[]}"#;
        match classify(200, body, "oq0001") {
            Placed::Rejected(r) => {
                assert_eq!(r.code, Some(50_113));
                assert_eq!(r.message, "Invalid Sign");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    /// The one case where nothing can be concluded. A 5xx says the
    /// venue could not answer, and an order that may exist must not be
    /// reported as refused.
    #[test]
    fn a_server_error_leaves_the_order_unknown() {
        assert!(matches!(
            classify(502, "<html>bad gateway</html>", "oq0001"),
            Placed::Unknown(_)
        ));
        assert!(matches!(classify(429, "{}", "oq0001"), Placed::Unknown(_)));
    }

    /// A body that is not this venue's shape says nothing about the
    /// order, and guessing either way is worse than saying so.
    #[test]
    fn an_unrecognisable_body_is_unknown_rather_than_assumed() {
        assert!(matches!(
            classify(200, "<html>captive portal</html>", "oq0001"),
            Placed::Unknown(_)
        ));
    }

    // -- 2. testnet is a header ---------------------------------------

    /// The header is derived from the endpoint and there is no other way
    /// to set it, because a missing header here is not a broken request
    /// — it is a live one.
    #[test]
    fn the_endpoint_decides_the_simulation_header_and_nothing_else_does() {
        let creds = || {
            Credentials::new("k", "s")
                .with_passphrase("p")
                .expect("valid")
        };
        assert!(Okx::at(Endpoint::Testnet, creds()).is_simulated());
        assert!(!Okx::at(Endpoint::Live, creds()).is_simulated());
        // Even against a custom base, which exists for proxies and
        // tests, the endpoint is what decides.
        assert!(Okx::new("http://localhost:1", creds(), Endpoint::Testnet).is_simulated());
        assert!(!Okx::new("http://localhost:1", creds(), Endpoint::Live).is_simulated());
    }

    // -- 3. contracts, not coins --------------------------------------

    /// The listing is read, not written down — and reading it is what
    /// caught the assumption this adapter started with.
    #[test]
    fn the_venues_own_listing_is_read_correctly() {
        let l = listing();
        assert_eq!(l.inst_id, "BTC-USDT-SWAP");
        // ctVal "0.01" — one contract is a hundredth of a BTC.
        assert_eq!(l.contract_value, oq_types::CONTRACT_SCALE / 100);
        // tickSz "0.1"
        assert_eq!((l.price_scale, l.price_tick), (1, 1));
        // lotSz "0.01" — and this is the field that disproved "whole
        // contracts only". A hundredth of a contract is a legal size.
        assert_eq!((l.size_scale, l.lot_size), (2, 1));
        assert_eq!(l.min_size, 1, "minSz 0.01 rescaled onto the size grid");
    }

    /// A coin amount becomes the venue's `sz`, on the venue's grid.
    #[test]
    fn a_coin_quantity_becomes_the_venues_size() {
        let l = listing();
        // 0.05 BTC at 0.01 BTC per contract is 5 contracts.
        assert_eq!(l.size_text(QtyLots(500), 4).as_deref(), Ok("5.00"));
        // 0.0001 BTC is one hundredth of a contract — legal here, and
        // impossible under the whole-contract assumption this adapter
        // started with.
        assert_eq!(l.size_text(QtyLots(1), 4).as_deref(), Ok("0.01"));
    }

    /// Rounding sends a different order than the one that was risked,
    /// and the position check downstream then blames the venue.
    #[test]
    fn a_quantity_off_the_grid_is_refused_not_rounded() {
        let l = listing();
        // 0.00005 BTC is half of the smallest legal size.
        let e = l.size_text(QtyLots(5), 5).expect_err("half a lot");
        assert!(e.message.contains("size grid"), "{e:?}");
    }

    /// Below the venue's own minimum is refused here, because the venue
    /// refuses it with a message about the size rather than the floor.
    #[test]
    fn a_size_below_the_venues_minimum_is_refused() {
        let mut l = listing();
        l.min_size = 100; // pretend the venue wants a whole contract
        let e = l.size_text(QtyLots(1), 4).expect_err("below the floor");
        assert!(e.message.contains("minimum"), "{e:?}");
    }

    /// A listing this build cannot read is one it must not trade
    /// against, and the error names the field rather than the venue.
    #[test]
    fn an_unreadable_listing_is_refused_by_field() {
        let broken = r#"{"code":"0","data":[{"instId":"X-SWAP","ctVal":"nonsense","tickSz":"0.1","lotSz":"1","minSz":"1"}]}"#;
        let e = parse_listing(broken, "X-SWAP").expect_err("ctVal is not a number");
        assert!(format!("{e}").contains("ctVal"), "{e}");
        assert!(
            parse_listing(LISTING, "ETH-USDT-SWAP").is_err(),
            "not in this payload"
        );
    }

    /// An order sized in contracts is the one the venue receives; a
    /// caller holding coins converts first, deliberately.
    #[test]
    fn an_order_carries_contracts_and_says_so_when_it_cannot() {
        let l = listing();
        let i = l.instrument();
        assert!(i.qty_on_grid(QtyLots(500)), "5.00 contracts is on the grid");
        assert!(i.qty_on_grid(QtyLots(1)), "0.01 contracts is too");
        // And the instrument still knows what a contract is worth, which
        // is the only thing that keeps the two venues' quantities apart.
        assert_eq!(i.contract_size, oq_types::CONTRACT_SCALE / 100);
    }

    // -- 4. signing ---------------------------------------------------

    /// The signature covers exactly what is sent, in the order it is
    /// sent, or it is a signature for a different request.
    #[test]
    fn the_signature_covers_the_timestamp_method_path_and_body() {
        let ts = "2026-08-18T02:03:04.567Z";
        let body = r#"{"instId":"BTC-USDT-SWAP"}"#;
        let got = sign(b"secret", ts, "POST", "/api/v5/trade/order", body);
        let expected = crate::b64::encode(&hmac_sha256(
            b"secret",
            format!("{ts}POST/api/v5/trade/order{body}").as_bytes(),
        ));
        assert_eq!(got, expected);
        // And it must differ if any one part differs.
        assert_ne!(got, sign(b"secret", ts, "GET", "/api/v5/trade/order", body));
        assert_ne!(got, sign(b"secret", ts, "POST", "/api/v5/trade/order", ""));
    }

    /// RFC 4648 vectors. A base64 that is wrong in its padding produces
    /// a signature the venue rejects, and the message names the
    /// signature rather than the encoder.
    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(crate::b64::encode(b""), "");
        assert_eq!(crate::b64::encode(b"f"), "Zg==");
        assert_eq!(crate::b64::encode(b"fo"), "Zm8=");
        assert_eq!(crate::b64::encode(b"foo"), "Zm9v");
        assert_eq!(crate::b64::encode(b"foob"), "Zm9vYg==");
        assert_eq!(crate::b64::encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(crate::b64::encode(b"foobar"), "Zm9vYmFy");
        // High bytes, since a digest is not ASCII.
        assert_eq!(crate::b64::encode(&[0xff, 0xfe, 0xfd]), "//79");
    }

    /// The venue accepts one timestamp format and rejects every other.
    #[test]
    fn the_timestamp_is_the_format_the_venue_accepts() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_timestamp(1), "1970-01-01T00:00:00.001Z");
        // A leap day, which is where a hand-written calendar breaks.
        assert_eq!(iso_timestamp(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        assert_eq!(iso_timestamp(1_755_484_984_567), "2025-08-18T02:43:04.567Z");
        // A century that is not a leap year.
        assert_eq!(iso_timestamp(4_102_444_800_000), "2100-01-01T00:00:00.000Z");
    }

    // -- the body -----------------------------------------------------

    #[test]
    fn a_limit_order_names_its_price_and_its_resting_rule() {
        let b = order_body(&order(), &instrument(), "5.00");
        assert!(b.contains(r#""instId":"BTC-USDT-SWAP""#), "{b}");
        assert!(b.contains(r#""sz":"5.00""#), "{b}");
        assert!(b.contains(r#""px":"60000.0""#), "{b}");
        assert!(b.contains(r#""ordType":"limit""#), "{b}");
        assert!(
            !b.contains("posSide"),
            "a one-way account names no leg: {b}"
        );
    }

    /// Time in force is an order type on this venue, not a flag, so a
    /// fill-or-kill limit is a different `ordType` rather than a limit
    /// with an extra field.
    #[test]
    fn time_in_force_becomes_the_order_type() {
        let mut o = order();
        o.tif = TimeInForce::FillOrKill;
        assert!(order_body(&o, &instrument(), "5.00").contains(r#""ordType":"fok""#));
        o.tif = TimeInForce::ImmediateOrCancel;
        assert!(order_body(&o, &instrument(), "5.00").contains(r#""ordType":"ioc""#));
    }

    #[test]
    fn a_market_order_carries_no_price() {
        let mut o = order();
        o.limit_price = None;
        let b = order_body(&o, &instrument(), "5.00");
        assert!(b.contains(r#""ordType":"market""#), "{b}");
        assert!(!b.contains("\"px\""), "{b}");
    }

    #[test]
    fn a_hedged_order_names_its_leg() {
        let mut o = order();
        o.position_side = PositionSide::Short;
        assert!(order_body(&o, &instrument(), "5.00").contains(r#""posSide":"short""#));
    }

    /// The body is signed, so its field order is part of the signature.
    /// Two serialisations that differ only in key order are two
    /// different requests and only one of them was signed.
    #[test]
    fn the_body_is_built_in_a_fixed_order() {
        let a = order_body(&order(), &instrument(), "5.00");
        let b = order_body(&order(), &instrument(), "5.00");
        assert_eq!(a, b);
        let inst_at = a.find("instId").expect("present");
        let sz_at = a.find("\"sz\"").expect("present");
        assert!(inst_at < sz_at, "field order must be stable: {a}");
    }

    // -- client ids ---------------------------------------------------

    /// Narrower than the other venue's, which is exactly why it is
    /// checked here rather than assumed from the shared contract.
    #[test]
    fn the_client_id_rule_is_this_venues_and_not_the_other_ones() {
        assert!(valid_client_id("oq0001"));
        assert!(valid_client_id(&"a".repeat(32)));
        assert!(!valid_client_id(&"a".repeat(33)));
        assert!(!valid_client_id(""));
        // Legal on Binance, refused here.
        assert!(!valid_client_id("oq-0001"));
        assert!(!valid_client_id("oq.0001"));
        assert!(!valid_client_id("oq_0001"));
    }

    // -- status queries -----------------------------------------------

    /// "No such order" is the answer that licenses a resend after an
    /// unknown, so it has to be distinguishable from "I could not tell".
    #[test]
    fn a_missing_order_is_reported_as_missing() {
        let body = r#"{"code":"51603","msg":"Order does not exist","data":[]}"#;
        assert_eq!(order_from_query(body, "oq0001"), None);
    }

    #[test]
    fn an_existing_order_comes_back_with_its_state() {
        let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ordId":"312269865356374016","clOrdId":"oq0001","state":"live","accFillSz":"0","sz":"5"}]}"#;
        let ack = order_from_query(body, "oq0001").expect("the order exists");
        assert_eq!(ack.venue_id, "312269865356374016");
        assert_eq!(ack.status, "live");
        assert_eq!(ack.executed_qty, "0");
    }

    /// The venue's status word is carried through unmapped. A state this
    /// build has never heard of must surface as itself rather than being
    /// forced into the nearest known variant.
    #[test]
    fn an_unfamiliar_state_is_carried_through_rather_than_mapped() {
        let body = r#"{"code":"0","msg":"","data":[{"ordId":"1","clOrdId":"oq0001","state":"mmp_canceled","accFillSz":"0"}]}"#;
        let ack = order_from_query(body, "oq0001").expect("exists");
        assert_eq!(ack.status, "mmp_canceled");
    }

    /// "Does not mean that the request was successful or failed" — the
    /// venue's own description of 50004. Unknown, not refused.
    #[test]
    fn a_request_timeout_at_the_venue_is_unknown() {
        for body in [
            r#"{"code":"50004","msg":"API endpoint request timeout","data":[]}"#,
            r#"{"code":"1","msg":"","data":[{"clOrdId":"oq0001","ordId":"","sCode":"50004","sMsg":"timeout"}]}"#,
        ] {
            let p = classify(200, body, "oq0001");
            assert!(matches!(p, Placed::Unknown(_)), "{body}: got {p:?}");
        }
    }
}
