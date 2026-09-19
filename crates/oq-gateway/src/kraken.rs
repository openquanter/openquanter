//! Kraken Futures.
//!
//! The third venue, and the one that broke two assumptions this crate
//! had been carrying since it met only Binance.
//!
//! # 1. An order id is a name
//!
//! `179f9af8-e45e-469d-b3e9-2fd4675cb7d0`. `venue_id` was an `i64` until
//! this venue was read, which would have meant refusing it or inventing
//! a number for it — and an invented id cannot be quoted back in a
//! support ticket. Nothing joins on it; `L4` makes `client_id` the join
//! key exactly so a venue's own handle can be whatever the venue likes.
//!
//! # 2. A client order id is globally unique here, and that is a gift
//!
//! `L4` records that Binance's `clientOrderId` is unique only among
//! *open* orders, so it is not an idempotency token, and that all three
//! surveyed projects treated it as one anyway. Kraken's is unique across
//! the account's history — up to 100 characters, and a repeat is refused
//! by name with `clientOrderIdAlreadyExist`.
//!
//! That makes the hardest case in the order path answerable. After a
//! [`Placed::Unknown`] — a timeout, where nobody knows whether the order
//! landed — a resend on this venue either places the order or is refused
//! because the first one did land. The venue decides, rather than the
//! caller inferring from a position that may not have updated yet.
//!
//! # 3. The signature is two hashes and a decode
//!
//! `SHA-256(postData + nonce + path)`, then HMAC-**SHA-512** under the
//! *base64-decoded* secret, then base64 of that. Three places to go
//! wrong, and the venue reports all of them as one invalid signature.
//!
//! # 4. Rejections arrive inside HTTP 200
//!
//! `result` is `"success"` and the refusal is in `sendStatus.status`,
//! with a `REJECT` event carrying the reason. The third of four families
//! to answer this way, which is why [`classify`] reads the body.
//!
//! # What this has not done
//!
//! **It has not been run against Kraken.** Every pure function here is
//! tested against payloads taken from the venue's documented shapes, and
//! that is not the same as having placed an order. The Binance adapter
//! was written to the same standard and its first real run found five
//! defects no unit test reached; the OKX adapter's first defect was
//! found by writing the layer above it. Assume this one has its own.
//!
//! One detail is flagged as a guess rather than a reading: the path that
//! goes into the signature is written here as the one the venue's
//! examples use, and a first real run is what confirms it.

use core::time::Duration;

use oq_hash::hmac::hmac_sha512;
use oq_types::Instrument;

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{Endpoint, Execution, NewOrder, OrderAck, Placed, Reject, Unresolved, decimal};
use crate::json::{array_field, field_str, object_containing, objects, raw_field};

/// A client for one Kraken Futures deployment.
pub struct Kraken {
    base: String,
    creds: Credentials,
    /// The secret, decoded once at construction.
    ///
    /// Decoded here rather than per request so a malformed secret fails
    /// when the client is built, not on the first order — and so the
    /// decode is not repeated on a path that runs every minute.
    secret: Vec<u8>,
    agent: ureq::Agent,
}

impl Kraken {
    /// Live futures.
    pub const MAINNET: &'static str = "https://futures.kraken.com/derivatives";
    /// The demo environment.
    pub const TESTNET: &'static str = "https://demo-futures.kraken.com/derivatives";

    /// Build a client against a named deployment.
    ///
    /// # Errors
    /// When the secret is not valid base64. Refused here rather than at
    /// the first request, because the venue reports a wrongly decoded
    /// key as an invalid signature and that sends the reader to the
    /// algorithm instead of to the key.
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Result<Self, String> {
        let base = match endpoint {
            Endpoint::Testnet => Self::TESTNET,
            Endpoint::Live => Self::MAINNET,
        };
        Self::new(base, creds)
    }

    /// Build a client against `base`.
    ///
    /// # Errors
    /// When the secret is not valid base64.
    pub fn new(base: impl Into<String>, creds: Credentials) -> Result<Self, String> {
        let secret = core::str::from_utf8(creds.secret_bytes())
            .ok()
            .and_then(crate::b64::decode)
            .ok_or_else(|| "this venue's API secret is base64 and this one is not".to_string())?;
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(45)))
            .http_status_as_error(false)
            .build();
        Ok(Self {
            base: base.into(),
            creds,
            secret,
            agent: config.into(),
        })
    }
}

// ---------------------------------------------------------------------
// Pure: everything below decides what to send and what an answer meant.
// ---------------------------------------------------------------------

/// The `Authent` header.
///
/// Five steps, from the venue's own description: concatenate
/// `postData + nonce + path`, SHA-256 it, take the base64-decoded
/// secret, HMAC-SHA-512 the digest under it, and base64 the result.
///
/// `secret` is the *decoded* bytes. Passing the encoded text would sign
/// with the wrong key and be reported as an invalid signature, which is
/// why the decode happens once at construction and this takes bytes.
#[must_use]
pub fn authent(secret: &[u8], post_data: &str, nonce: &str, path: &str) -> String {
    let message = format!("{post_data}{nonce}{path}");
    // SHA-256 first, over the concatenation. `hmac_sha256_hex` is not
    // what is wanted here — this is a plain digest, not a MAC.
    let mut digest = oq_hash::Sha256::new();
    digest.update(message.as_bytes());
    let hashed = digest.finalize();
    crate::b64::encode(&hmac_sha512(secret, &hashed))
}

/// The form parameters for a new order.
///
/// A market order is `mkt` and carries no price; a limit order is `lmt`
/// and carries one. Written out rather than defaulted, because a market
/// order sent with a stale limit price is an order at a price nobody
/// chose.
#[must_use]
pub fn order_body(order: &NewOrder, instrument: &Instrument) -> String {
    let side = match order.side {
        oq_types::Side::Buy => "buy",
        oq_types::Side::Sell => "sell",
    };
    let size = decimal(order.qty.0, instrument.qty_scale);
    let mut body = match order.limit_price {
        Some(price) => format!(
            "orderType=lmt&limitPrice={}",
            decimal(price.0, instrument.price_scale)
        ),
        None => "orderType=mkt".to_string(),
    };
    body.push_str(&format!(
        "&symbol={}&side={side}&size={size}&cliOrdId={}",
        order.symbol, order.client_id
    ));
    if order.reduce_only {
        body.push_str("&reduceOnly=true");
    }
    body
}

/// What a `/sendorder` answer meant.
///
/// The refusal is inside a 200 with `result: "success"`, in
/// `sendStatus.status`. Anything other than `placed` is the venue
/// declining, and the reason lives in an `orderEvents` entry rather than
/// beside the status.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    // A transport-level failure, where the order may or may not have
    // landed. Three outcomes, not two.
    if !(200..300).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }
    match field_str(body, "result").as_deref() {
        Some("success") => {}
        // The envelope itself failed. `error` names it.
        Some("error") => {
            return Placed::Rejected(Reject {
                code: None,
                message: field_str(body, "error")
                    .unwrap_or_else(|| format!("refused: {}", truncate(body))),
            });
        }
        // Not this venue's answer at all — a proxy's error page, a
        // captive portal. Unknown, because nothing can be concluded
        // from bytes this adapter cannot read: the order may be
        // resting. Calling it a rejection would invite a resend into a
        // position that already exists, which is the failure the
        // three-outcome contract exists to prevent.
        _ => {
            return Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: format!("unreadable answer: {}", truncate(body)),
            });
        }
    }
    let Some(send_status) = object_containing(body, "\"status\"") else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("accepted with no status in it: {}", truncate(body)),
        });
    };
    let venue_id = field_str(&send_status, "order_id").unwrap_or_default();
    let status_word = field_str(&send_status, "status").unwrap_or_default();
    if status_word == "placed" || status_word == "partiallyFilled" || status_word == "filled" {
        Placed::Accepted(OrderAck {
            venue_id,
            // Echoed when the venue gives it back, the caller's own
            // otherwise: losing the handle because a field was omitted
            // would defeat the point of having chosen it.
            client_id: field_str(&send_status, "cliOrdId").unwrap_or_else(|| client_id.to_string()),
            status: status_word,
            executed_qty: field_str(&send_status, "filledSize").unwrap_or_else(|| "0".to_string()),
        })
    } else {
        Placed::Rejected(Reject {
            code: None,
            message: format!(
                "{status_word}: {}",
                // The reason sits in an order event rather than beside
                // the status, so the whole status object is carried when
                // it cannot be found.
                field_str(&send_status, "reason").unwrap_or_else(|| truncate(&send_status))
            ),
        })
    }
}

/// Read one order out of a status or open-orders response.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    let needle = format!("\"{client_id}\"");
    let entry = object_containing(body, &needle)?;
    Some(OrderAck {
        venue_id: field_str(&entry, "order_id")
            .or_else(|| field_str(&entry, "orderId"))
            .unwrap_or_default(),
        client_id: client_id.to_string(),
        status: field_str(&entry, "status").unwrap_or_default(),
        executed_qty: field_str(&entry, "filledSize")
            .or_else(|| raw_field(&entry, "filledSize"))
            .unwrap_or_else(|| "0".to_string()),
    })
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

// ---------------------------------------------------------------------
// Account reads.
// ---------------------------------------------------------------------

/// Kraken's order state in the shared vocabulary.
#[must_use]
pub fn status_of(state: &str) -> Option<&'static str> {
    match state {
        "untouched" => Some("NEW"),
        "partiallyFilled" => Some("PARTIALLY_FILLED"),
        _ => None,
    }
}

/// Read `/accounts`, for the multi-collateral (`flex`) account.
///
/// # The mapping is checked rather than assumed
///
/// The response carries `balanceValue`, `portfolioValue`,
/// `collateralValue`, `marginEquity`, `totalUnrealized` and more;
/// `AccountSnapshot` carries a wallet balance, an unrealized P&L and a
/// margin balance. Which name means which cannot be settled by reading
/// the names, and a wrong balance is worse than a missing one — this
/// crate already learned that when an unreadable balance was becoming
/// `0.0`, "which is a number a risk gate acts on".
///
/// So the reading is stated as an identity and then enforced:
/// `portfolioValue` must equal `balanceValue + totalUnrealized`. If the
/// venue's own numbers do not satisfy it, this interpretation is wrong
/// and the read fails instead of returning three plausible figures. A
/// guess that can be contradicted by the data is not a guess for long.
///
/// # Errors
/// When the envelope failed, a field is missing, or the identity above
/// does not hold.
pub fn parse_accounts(
    body: &str,
    read_at_ms: i64,
) -> Result<crate::binance::AccountSnapshot, VenueError> {
    if field_str(body, "result").as_deref() != Some("success") {
        return Err(crate::json::malformed("accounts", body));
    }
    let Some(flex) = object_containing(body, "\"marginEquity\"") else {
        return Err(crate::json::malformed("the flex account", body));
    };
    let read = |key: &'static str| -> Result<f64, VenueError> {
        raw_field(&flex, key)
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| crate::json::malformed(key, &flex))
    };
    let wallet = read("balanceValue")?;
    let unrealized = read("totalUnrealized")?;
    let portfolio = read("portfolioValue")?;
    // Scaled, because these are account-sized numbers and an absolute
    // epsilon would reject a large account for its own rounding.
    let tolerance = 1e-6_f64.mul_add(portfolio.abs().max(wallet.abs()), 1e-8);
    if (portfolio - (wallet + unrealized)).abs() > tolerance {
        return Err(VenueError::Malformed {
            what: "the balance fields do not mean what this build reads them to mean",
            body: format!(
                "portfolioValue {portfolio} is not balanceValue {wallet} plus \
                 totalUnrealized {unrealized}; refusing rather than reporting a \
                 balance a risk gate would act on"
            ),
        });
    }
    Ok(crate::binance::AccountSnapshot {
        wallet_balance: wallet,
        unrealized,
        margin_balance: portfolio,
        read_at_ms,
    })
}

/// Read `/openpositions`.
///
/// Sizes and prices are JSON numbers here rather than decimal strings,
/// so the raw text is what gets kept — the digits the venue sent.
/// Direction comes from `side` and the magnitude from `size`, the same
/// rule the OKX adapter uses and for the same reason: a venue that
/// changes which one carries the sign is then one this still reads.
///
/// # Errors
/// When the envelope failed or a field cannot be read.
pub fn parse_positions(body: &str) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
    if field_str(body, "result").as_deref() != Some("success") {
        return Err(crate::json::malformed("positions", body));
    }
    let Some(list) = array_field(body, "openPositions") else {
        // No `openPositions` member at all is a flat account, not a
        // failed read: the venue omits it rather than sending [].
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in objects(&list) {
        let Some(side) = field_str(&item, "side") else {
            return Err(crate::json::malformed("position side", &item));
        };
        let leg = match side.as_str() {
            "long" => "LONG",
            "short" => "SHORT",
            _ => return Err(crate::json::malformed("position side", &item)),
        };
        let Some(size) = raw_field(&item, "size") else {
            return Err(crate::json::malformed("position size", &item));
        };
        let magnitude = size.trim_start_matches('-').to_string();
        let amount_text = if leg == "SHORT" {
            format!("-{magnitude}")
        } else {
            magnitude
        };
        let Ok(amount) = amount_text.parse::<f64>() else {
            return Err(crate::json::malformed("position size", &item));
        };
        let entry_text = raw_field(&item, "price").unwrap_or_default();
        out.push(crate::binance::PositionSnapshot {
            symbol: field_str(&item, "symbol").unwrap_or_default(),
            position_side: leg.to_string(),
            amount,
            amount_text,
            entry_price: entry_text.parse::<f64>().unwrap_or_default(),
            entry_text,
            unrealized: raw_field(&item, "unrealizedPnl")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Read `/openorders`.
///
/// # Errors
/// When the envelope failed, or an order carries a state this build
/// does not know — refused rather than guessed, because an unknown
/// state reported as resting is an order the book waits on forever.
pub fn parse_open_orders(body: &str) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
    if field_str(body, "result").as_deref() != Some("success") {
        return Err(crate::json::malformed("open orders", body));
    }
    let Some(list) = array_field(body, "openOrders") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in objects(&list) {
        let Some(status) = field_str(&item, "status").and_then(|s| status_of(&s)) else {
            return Err(crate::json::malformed("order status", &item));
        };
        let Some(side) = field_str(&item, "side") else {
            return Err(crate::json::malformed("order side", &item));
        };
        let side = match side.as_str() {
            "buy" => "BUY",
            "sell" => "SELL",
            _ => return Err(crate::json::malformed("order side", &item)),
        };
        let number = |key: &str| -> f64 {
            raw_field(&item, key)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default()
        };
        let filled = number("filledSize");
        out.push(crate::binance::OpenOrder {
            symbol: field_str(&item, "symbol").unwrap_or_default(),
            order_id: field_str(&item, "order_id").unwrap_or_default(),
            client_order_id: field_str(&item, "cliOrdId").unwrap_or_default(),
            side: side.to_string(),
            // One leg per contract here; the venue reports no hedged
            // legs, so every order is on the only position there is.
            position_side: "BOTH".to_string(),
            price: number("limitPrice"),
            // `unfilledSize` is what is left, not what was asked for.
            orig_qty: number("unfilledSize") + filled,
            executed_qty: filled,
            status: status.to_string(),
        });
    }
    Ok(out)
}

/// Read one contract out of `/instruments`.
///
/// # Errors
/// When the instrument is absent or a field this build needs cannot be
/// read.
pub fn parse_instrument(body: &str, symbol: &str) -> Result<Instrument, String> {
    let Some(entry) = object_containing(body, &format!("\"{symbol}\"")) else {
        return Err(format!("no instrument {symbol} in this listing"));
    };
    let tick_text = raw_field(&entry, "tickSize").ok_or("no tickSize")?;
    let (tick, price_scale) = decimal_and_scale(&tick_text).ok_or("unreadable tickSize")?;
    let contract_text = raw_field(&entry, "contractSize").ok_or("no contractSize")?;
    let contract: i64 = contract_text
        .parse::<f64>()
        .map(|v| (v * 1e8) as i64)
        .map_err(|_| "unreadable contractSize")?;
    // Sizes here are whole contracts, so the quantity scale is zero and
    // the step is one. `contract_size` is what one of them is worth,
    // which on an inverse contract is a number of dollars.
    Ok(Instrument::sized(price_scale, 0, contract).with_grid(tick, 1))
}

/// A decimal like `0.5` as an integer at the scale it implies.
fn decimal_and_scale(text: &str) -> Option<(i64, u8)> {
    let text = text.trim();
    let (int_part, frac) = text.split_once('.').unwrap_or((text, ""));
    let scale = u8::try_from(frac.len()).ok()?;
    let digits = format!("{int_part}{frac}");
    digits.parse::<i64>().ok().map(|v| (v, scale))
}

impl Kraken {
    /// The account's balances.
    ///
    /// # Errors
    /// Whatever the venue reports, or a balance reading this build
    /// cannot confirm.
    pub fn balances(&self) -> Result<crate::binance::AccountSnapshot, VenueError> {
        let read_at = crate::binance::now_ms();
        let body = self.send("/api/v3/accounts", "")?;
        parse_accounts(&body, read_at)
    }

    /// Open positions.
    ///
    /// # Errors
    /// Whatever the venue reports.
    pub fn positions(&self) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
        let body = self.send("/api/v3/openpositions", "")?;
        parse_positions(&body)
    }

    /// Everything resting.
    ///
    /// # Errors
    /// Whatever the venue reports.
    pub fn open_orders(&self) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
        let body = self.send("/api/v3/openorders", "")?;
        parse_open_orders(&body)
    }

    /// One contract's precision and size.
    ///
    /// # Errors
    /// Whatever the venue reports, or a listing this build cannot read.
    pub fn instrument(&self, symbol: &str) -> Result<Instrument, String> {
        let body = self
            .send("/api/v3/instruments", "")
            .map_err(|e| e.to_string())?;
        parse_instrument(&body, symbol)
    }
}

impl Kraken {
    /// Send a signed request and return its body.
    ///
    /// Parameters go in the query string and the signature covers them
    /// there, which is what `postData` means on this venue.
    fn send(&self, path: &str, params: &str) -> Result<String, VenueError> {
        // Milliseconds, which the venue documents as a good nonce. It is
        // optional here — unlike on the spot API — but sending one costs
        // nothing and a monotonic value is what makes a replay visible.
        let nonce = crate::binance::now_ms().to_string();
        let signature = authent(&self.secret, params, &nonce, path);
        let url = if params.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{params}", self.base)
        };
        let mut response = self
            .agent
            .post(&url)
            .header("APIKey", self.creds.key())
            .header("Authent", &signature)
            .header("Nonce", &nonce)
            .send("")
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

impl Execution for Kraken {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        if !crate::broker::IdRules::KRAKEN.accepts(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable here: up to 100 characters of \
                     letters, digits and punctuation",
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
                    "price {} is not on this contract's tick grid of {} at {} dp",
                    decimal(price.0, instrument.price_scale),
                    instrument.price_tick,
                    instrument.price_scale
                ),
            });
        }
        let params = order_body(order, instrument);
        match self.send("/api/v3/sendorder", &params) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    fn cancel(&self, _symbol: &str, client_id: &str) -> Placed {
        // By the caller's own id rather than the venue's. It is the
        // handle that survives a placement whose answer never came back,
        // and on this venue it is unique across the account's history,
        // so it names one order for good.
        let params = format!("cliOrdId={client_id}");
        match self.send("/api/v3/cancelorder", &params) {
            Ok(text) => classify(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(&self, _symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let body = self.send("/api/v3/orders/status", &format!("cliOrdIds={client_id}"))?;
        Ok(order_from_query(&body, client_id))
    }
}

#[cfg(test)]
mod account_reads {
    use super::*;

    /// The shape the venue's own SDK documents, with the zeroes
    /// replaced by numbers that make the identity testable.
    const ACCOUNTS: &str = r#"{"result":"success","accounts":{"cash":{"type":"cashAccount"},
        "flex":{"currencies":{},"initialMargin":100.0,"maintenanceMargin":50.0,
        "balanceValue":5000.0,"portfolioValue":4950.0,"collateralValue":5000.0,"pnl":0.0,
        "unrealizedFunding":0.0,"totalUnrealized":-50.0,"totalUnrealizedAsMargin":-50.0,
        "availableMargin":4850.0,"marginEquity":4950.0,"type":"multiCollateralMarginAccount"}},
        "serverTime":"2023-04-04T17:56:49.027Z"}"#;

    #[test]
    fn the_balance_mapping_is_checked_against_the_venues_own_numbers() {
        let snap = parse_accounts(ACCOUNTS, 42).expect("a readable account");
        assert!((snap.wallet_balance - 5000.0).abs() < 1e-9);
        assert!((snap.unrealized - -50.0).abs() < 1e-9);
        assert!((snap.margin_balance - 4950.0).abs() < 1e-9);
        assert_eq!(snap.read_at_ms, 42);
    }

    #[test]
    fn a_mapping_the_numbers_contradict_is_refused_rather_than_reported() {
        // The whole point. If `portfolioValue` is not `balanceValue`
        // plus `totalUnrealized`, this build has the fields wrong — and
        // three plausible numbers from a wrong reading is exactly the
        // failure that made an unreadable balance an error rather than
        // a zero.
        let wrong = ACCOUNTS.replace("\"portfolioValue\":4950.0", "\"portfolioValue\":9999.0");
        let e = parse_accounts(&wrong, 1).expect_err("the identity must hold");
        assert!(
            format!("{e}").contains("do not mean what this build reads them to mean"),
            "{e}"
        );
    }

    #[test]
    fn a_legs_direction_comes_from_its_name() {
        let body = r#"{"result":"success","openPositions":[
            {"side":"short","symbol":"PI_XBTUSD","price":9392.749993345933,"size":10000,
             "unrealizedPnl":-607250.006654067},
            {"side":"long","symbol":"PF_XBTUSD","price":9399.75,"size":20000,
             "unrealizedPnl":1199500.66}],"serverTime":"2020-07-22T14:39:12.376Z"}"#;
        let legs = parse_positions(body).expect("readable positions");
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].position_side, "SHORT");
        assert_eq!(legs[0].amount_text, "-10000");
        // A JSON number, kept as the digits the venue sent rather than
        // reformatted through a float.
        assert_eq!(legs[0].entry_text, "9392.749993345933");
        assert_eq!(legs[1].position_side, "LONG");
        assert_eq!(legs[1].amount_text, "20000");
    }

    #[test]
    fn an_account_with_no_positions_is_flat_and_not_unreadable() {
        // The venue omits the member rather than sending an empty list.
        let body = r#"{"result":"success","serverTime":"2020-07-22T14:39:12.376Z"}"#;
        assert!(parse_positions(body).expect("a flat account").is_empty());
    }

    #[test]
    fn a_resting_orders_original_size_is_what_is_left_plus_what_filled() {
        // `unfilledSize` is the remainder, not the original: reporting
        // it as `orig_qty` would make a partially filled order look
        // smaller than it was placed.
        let body = r#"{"result":"success","openOrders":[{"order_id":"2ce038ae-c144-4de7-a0f1-82f7f4fca864",
            "symbol":"pi_ethusd","side":"buy","orderType":"lmt","limitPrice":1200,
            "unfilledSize":70,"status":"untouched","filledSize":30,"reduceOnly":false}],
            "serverTime":"2023-04-07T15:18:04.699Z"}"#;
        let orders = parse_open_orders(body).expect("readable orders");
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_id, "2ce038ae-c144-4de7-a0f1-82f7f4fca864");
        assert_eq!(orders[0].side, "BUY");
        assert_eq!(orders[0].status, "NEW");
        assert!((orders[0].executed_qty - 30.0).abs() < 1e-9);
        assert!((orders[0].orig_qty - 100.0).abs() < 1e-9);
    }

    #[test]
    fn an_order_state_this_build_does_not_know_is_refused() {
        let body = r#"{"result":"success","openOrders":[{"order_id":"a","symbol":"pi_ethusd",
            "side":"buy","limitPrice":1,"unfilledSize":1,"status":"teleported","filledSize":0}]}"#;
        assert!(parse_open_orders(body).is_err());
    }

    #[test]
    fn an_instrument_carries_its_tick_and_what_a_contract_is_worth() {
        let body = r#"{"result":"success","instruments":[{"symbol":"pi_xbtusd",
            "type":"futures_inverse","tickSize":0.5,"contractSize":1,"tradeable":true}]}"#;
        let i = parse_instrument(body, "pi_xbtusd").expect("a readable instrument");
        // 0.5 is five at one decimal place.
        assert_eq!(i.price_scale, 1);
        assert_eq!(i.price_tick, 5);
        // Whole contracts.
        assert_eq!(i.qty_scale, 0);
        assert_eq!(i.qty_step, 1);
        // One contract is one dollar on an inverse future.
        assert_eq!(i.contract_size, 100_000_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_two_hashes_and_a_decode() {
        // Deterministic, so the five steps are pinned. A change to any
        // of them — the order of the concatenation, SHA-512 for
        // SHA-256, the encoded secret instead of the decoded one —
        // moves this, and the venue would have reported all of them
        // identically as an invalid signature.
        let secret = crate::b64::decode("c2VjcmV0").expect("valid base64");
        assert_eq!(secret, b"secret");
        let a = authent(&secret, "size=1", "1700000000000", "/api/v3/sendorder");
        let b = authent(&secret, "size=1", "1700000000001", "/api/v3/sendorder");
        assert_ne!(a, b, "the nonce participates");
        assert_ne!(
            a,
            authent(&secret, "size=2", "1700000000000", "/api/v3/sendorder"),
            "the body participates"
        );
        assert_ne!(
            a,
            authent(&secret, "size=1", "1700000000000", "/api/v3/cancelorder"),
            "the path participates"
        );
        // Signing with the *encoded* secret is the mistake this guards
        // against: it produces a valid-looking signature for a key the
        // venue does not have.
        assert_ne!(
            a,
            authent(b"c2VjcmV0", "size=1", "1700000000000", "/api/v3/sendorder")
        );
    }

    #[test]
    fn a_placed_order_is_accepted_and_keeps_its_uuid() {
        let body = r#"{"result":"success","sendStatus":{"order_id":"179f9af8-e45e-469d-b3e9-2fd4675cb7d0","status":"placed","cliOrdId":"oq1","receivedTime":"2019-09-05T16:33:50.734Z"},"serverTime":"2019-09-05T16:33:50.734Z"}"#;
        match classify(200, body, "oq1") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, "179f9af8-e45e-469d-b3e9-2fd4675cb7d0");
                assert_eq!(a.client_id, "oq1");
                assert_eq!(a.status, "placed");
            }
            other => panic!("a placed order is an acceptance: {other:?}"),
        }
    }

    #[test]
    fn a_refusal_inside_a_200_is_a_rejection_and_not_an_acceptance() {
        // The trap this venue shares with two of the other three: the
        // status line says success and the order was never placed.
        let body = r#"{"result":"success","sendStatus":{"order_id":"","status":"iocWouldNotExecute","cliOrdId":"oq1","orderEvents":[]},"serverTime":"2019-09-05T16:33:50.734Z"}"#;
        match classify(200, body, "oq1") {
            Placed::Rejected(r) => assert!(
                r.message.contains("iocWouldNotExecute"),
                "the venue's own word for it: {}",
                r.message
            ),
            other => panic!("an unplaced order is not an acceptance: {other:?}"),
        }
    }

    #[test]
    fn an_envelope_that_failed_is_refused_by_its_own_error() {
        let body = r#"{"result":"error","error":"invalidArgument","serverTime":"2019-09-05T16:33:50.734Z"}"#;
        match classify(200, body, "oq1") {
            Placed::Rejected(r) => assert_eq!(r.message, "invalidArgument"),
            other => panic!("an error envelope is a rejection: {other:?}"),
        }
    }

    #[test]
    fn an_answer_this_adapter_cannot_read_concludes_nothing() {
        // Caught by the conformance suite the first time it ran against
        // this adapter, which had been reporting a captive portal's
        // HTML as a refusal. A refusal invites a resend; the order may
        // be resting.
        match classify(200, "<html>captive portal</html>", "oq1") {
            Placed::Unknown(u) => assert_eq!(u.client_id, "oq1"),
            other => panic!("unreadable bytes are not a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_request_that_never_answered_is_unknown_rather_than_failed() {
        // A timeout does not mean the order failed; it means nobody
        // knows, and folding that into an error is what produces
        // duplicate positions.
        match classify(504, "<html>gateway timeout</html>", "oq1") {
            Placed::Unknown(u) => assert_eq!(u.client_id, "oq1"),
            other => panic!("a 504 is not a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_client_id_may_be_long_here() {
        use crate::broker::IdRules;
        assert!(IdRules::KRAKEN.accepts("oq1"));
        assert!(IdRules::KRAKEN.accepts(&"a".repeat(100)));
        assert!(!IdRules::KRAKEN.accepts(&"a".repeat(101)));
        // Longer than either of the first two venues allows, which is
        // the direction that matters: an id built for them fits here.
        assert!(IdRules::KRAKEN.accepts(&"a".repeat(36)));
    }
}
