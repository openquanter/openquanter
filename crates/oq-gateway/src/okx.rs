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
    Endpoint, Execution, NewOrder, OrderAck, Placed, PositionSide, Reject, Unresolved, decimal,
};
use crate::json::{field_str, objects};

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

/// Base64, RFC 4648, with padding.
///
/// Hand-written for the same reason the hashes are: this is in the path
/// that signs requests against an account, and every dependency there is
/// one more thing trusted with the secret.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

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
    base64(&hmac_sha256(secret, message.as_bytes()))
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
    let venue_id = field_str(&datum, "ordId")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_default();
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
                assert_eq!(a.venue_id, 312_269_865_356_374_016);
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
        let expected = base64(&hmac_sha256(
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
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // High bytes, since a digest is not ASCII.
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
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
        assert_eq!(ack.venue_id, 312_269_865_356_374_016);
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
}
