//! Bitget, on the Unified Trading Account API.
//!
//! The fourth venue, and the second one this survey caught mid-move.
//!
//! # It is the v3 API or nothing
//!
//! Bitget began migrating classic accounts to a Unified Trading Account
//! on 2026-09-15, in batches. Their own upgrade guide states the
//! consequence: **a UTA key cannot call the classic endpoints**. So the
//! `/api/v2/mix/*` paths an adapter would have targeted a week earlier
//! are the ones that stop working for every account that gets migrated.
//! This adapter speaks v3 only, and does not carry the classic paths as
//! a fallback — a fallback here would be a path that works until the
//! account is upgraded and then fails at a moment nobody chose.
//!
//! # The family is the signature, and only the signature
//!
//! The signing is OKX's, restated: base64 of `HMAC-SHA256` over
//! `timestamp + METHOD + requestPath + queryString + body`, under a
//! key/secret/passphrase triple. Everything else differs.
//!
//! - **The timestamp is milliseconds**, where OKX's is ISO 8601. One
//!   venue, one format each, and the wrong one is refused as a bad
//!   signature rather than as a bad timestamp.
//! - **The success code is `"00000"`**, not `"0"`. An adapter that
//!   copied OKX's envelope check would read every success as a failure
//!   — which is the safe direction, and still wrong.
//! - **`data` is an object**, not an array.
//! - **Sizes are in coins.** USDT- and USDC-margined futures take `qty`
//!   in the base coin, which is what `Instrument` counts in when
//!   `contract_size` is one. OKX counts contracts. Sharing a signature
//!   with a venue says nothing about sharing a unit.
//!
//! # Rejections arrive inside HTTP 200
//!
//! The fourth venue here and the third to do it this way. `code` is the
//! whole answer and the status line says nothing.
//!
//! # What this has not done
//!
//! **It has not been run against Bitget.** The place-order endpoint and
//! its response are read from the venue's documentation; the cancel and
//! status paths are written to the same naming and are **a guess until
//! someone runs them** — they are marked below. Every pure function is
//! tested against documented shapes, which is not the same as having
//! placed an order.

use core::time::Duration;

use oq_hash::hmac::hmac_sha256;
use oq_types::Instrument;

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{Endpoint, Execution, NewOrder, OrderAck, Placed, Reject, Unresolved, decimal};
use crate::json::{array_field, field_str, object_containing};

/// A client for Bitget's unified account API.
pub struct Bitget {
    base: String,
    creds: Credentials,
    agent: ureq::Agent,
    /// Which product line this client trades.
    ///
    /// Carried because every v3 call names it and a default would be a
    /// silent choice between USDT- and coin-margined contracts, which
    /// settle in different assets.
    category: &'static str,
    /// Whether this client trades Bitget's demo account.
    ///
    /// Bitget's demo shares the production host and is selected per
    /// request by a header. `Endpoint` was dropped here, so a client asked
    /// for the test deployment sent every order to the real account — the
    /// one thing `Endpoint` exists to prevent.
    demo: bool,
}

impl Bitget {
    /// The one host. There is no separate demo domain.
    pub const HOST: &'static str = "https://api.bitget.com";

    /// USDT-margined perpetuals.
    #[must_use]
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Self {
        // No separate demo host: the test deployment is the same host
        // with `paptrading: 1` on every private request, and demo keys.
        Self {
            demo: matches!(endpoint, Endpoint::Testnet),
            base: Self::HOST.to_string(),
            creds,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(45)))
                .http_status_as_error(false)
                .build()
                .into(),
            category: "USDT-FUTURES",
        }
    }
}

// ---------------------------------------------------------------------
// Pure.
// ---------------------------------------------------------------------

/// The `ACCESS-SIGN` header.
///
/// `timestamp` is milliseconds since the epoch, as text. OKX signs the
/// same shape with an ISO 8601 stamp, and a client that carried the
/// habit across gets an invalid-signature error naming neither.
#[must_use]
pub fn sign(secret: &[u8], timestamp: &str, method: &str, path: &str, body: &str) -> String {
    let message = format!("{timestamp}{method}{path}{body}");
    crate::b64::encode(&hmac_sha256(secret, message.as_bytes()))
}

/// The headers a private request carries.
///
/// `demo` adds `paptrading: 1`, which is how Bitget tells its demo
/// account from the real one on a shared host.
#[must_use]
pub fn headers<'a>(
    key: &'a str,
    signature: &'a str,
    timestamp: &'a str,
    passphrase: &'a str,
    demo: bool,
) -> Vec<(&'static str, &'a str)> {
    let mut h = vec![
        ("ACCESS-KEY", key),
        ("ACCESS-SIGN", signature),
        ("ACCESS-TIMESTAMP", timestamp),
        ("ACCESS-PASSPHRASE", passphrase),
        ("Content-Type", "application/json"),
    ];
    if demo {
        h.push(("paptrading", "1"));
    }
    h
}

/// The body for a new order.
#[must_use]
pub fn order_body(order: &NewOrder, instrument: &Instrument, category: &str) -> String {
    let side = match order.side {
        oq_types::Side::Buy => "buy",
        oq_types::Side::Sell => "sell",
    };
    // Coins, not contracts: USDT- and USDC-margined futures take `qty`
    // in the base coin.
    let qty = decimal(order.qty.0, instrument.qty_scale);
    let mut body = format!(
        r#"{{"category":"{category}","symbol":"{}","side":"{side}","qty":"{qty}","clientOid":"{}""#,
        order.symbol, order.client_id
    );
    match order.limit_price {
        Some(price) => {
            body.push_str(&format!(
                r#","orderType":"limit","price":"{}""#,
                decimal(price.0, instrument.price_scale)
            ));
        }
        None => body.push_str(r#","orderType":"market""#),
    }
    if order.reduce_only {
        body.push_str(r#","reduceOnly":"YES""#);
    }
    body.push('}');
    body
}

/// The code this venue says success with.
///
/// Five zeroes. OKX says `"0"`, and an adapter that copied that check
/// would read every success here as a failure.
pub const SUCCESS: &str = "00000";

/// What a place-order answer meant.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    if !(200..300).contains(&status) && body.trim().is_empty() {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status} with no body"),
        });
    }
    let Some(code) = field_str(body, "code") else {
        // Not this venue's answer at all — a proxy page, a captive
        // portal. Nothing can be concluded: the order may be resting,
        // and calling it a refusal invites a resend into a position
        // that already exists.
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unreadable answer: {}", truncate(body)),
        });
    };
    if code != SUCCESS {
        return Placed::Rejected(Reject {
            code: code.parse::<i64>().ok(),
            message: field_str(body, "msg").unwrap_or_else(|| truncate(body)),
        });
    }
    let Some(data) = object_containing(body, "\"orderId\"") else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("accepted with no order in it: {}", truncate(body)),
        });
    };
    Placed::Accepted(OrderAck {
        venue_id: field_str(&data, "orderId").unwrap_or_default(),
        // Echoed when given back, the caller's own otherwise: the id the
        // caller chose is the handle, and losing it because a field was
        // omitted would defeat the point of having chosen it.
        client_id: field_str(&data, "clientOid").unwrap_or_else(|| client_id.to_string()),
        // This venue acknowledges without a state word. `accepted` is
        // this adapter's, and is marked as such rather than invented to
        // look like the venue's.
        status: "accepted".to_string(),
        executed_qty: "0".to_string(),
    })
}

/// Read one order out of a query response.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    if field_str(body, "code").as_deref() != Some(SUCCESS) {
        return None;
    }
    // `data` is a list here even though placement answers with an
    // object, so both shapes are tried rather than assumed.
    let haystack = array_field(body, "data").unwrap_or_else(|| body.to_string());
    let entry = object_containing(&haystack, &format!("\"{client_id}\""))?;
    Some(OrderAck {
        venue_id: field_str(&entry, "orderId").unwrap_or_default(),
        client_id: client_id.to_string(),
        status: field_str(&entry, "status")
            .or_else(|| field_str(&entry, "state"))
            .unwrap_or_default(),
        executed_qty: field_str(&entry, "filledQty")
            .or_else(|| field_str(&entry, "baseVolume"))
            .unwrap_or_else(|| "0".to_string()),
    })
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

impl Bitget {
    /// Send a signed request and return its body.
    fn send(&self, method: &str, path: &str, body: &str) -> Result<String, VenueError> {
        let Some(passphrase) = self.creds.passphrase() else {
            return Err(VenueError::Transport(
                "Bitget needs a passphrase as well as a key and a secret; \
                 set OQ_VENUE_PASSPHRASE or use Credentials::with_passphrase"
                    .to_string(),
            ));
        };
        let timestamp = crate::binance::now_ms().to_string();
        let signature = sign(self.creds.secret_bytes(), &timestamp, method, path, body);
        let url = format!("{}{path}", self.base);
        // One header list for both verbs, for the reason the OKX
        // adapter states: writing them twice is how one path ends up
        // missing one.
        let headers = headers(
            self.creds.key(),
            &signature,
            &timestamp,
            passphrase,
            self.demo,
        );
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

impl Execution for Bitget {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
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
        let body = order_body(order, instrument, self.category);
        match self.send("POST", "/api/v3/trade/place-order", &body) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    /// Withdraw an order.
    ///
    /// **The path is a guess.** Place-order is read from the venue's
    /// documentation; this one follows its naming and has not been
    /// confirmed. A wrong path here is a signed request to an endpoint
    /// that does not exist, which this venue answers with a code that
    /// reads like a bad parameter.
    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        let body = format!(
            r#"{{"category":"{}","symbol":"{symbol}","clientOid":"{client_id}"}}"#,
            self.category
        );
        match self.send("POST", "/api/v3/trade/cancel-order", &body) {
            Ok(text) => classify(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    /// Ask about an order by the id the caller gave it.
    ///
    /// **The path is a guess**, for the same reason as `cancel`.
    fn order_status(&self, symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let body = self.send(
            "GET",
            &format!(
                "/api/v3/trade/order-info?category={}&symbol={symbol}&clientOid={client_id}",
                self.category
            ),
            "",
        )?;
        Ok(order_from_query(&body, client_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_success_code_is_five_zeroes_and_not_one() {
        // An adapter that copied OKX's envelope check would read every
        // success here as a failure. Safe direction, still wrong.
        assert_eq!(SUCCESS, "00000");
        let accepted = r#"{"code":"00000","msg":"success","requestTime":1695806875837,"data":{"clientOid":"oq1","orderId":"121211212122"}}"#;
        match classify(200, accepted, "oq1") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, "121211212122");
                assert_eq!(a.client_id, "oq1");
            }
            other => panic!("a 00000 is an acceptance: {other:?}"),
        }
        // `"0"` is OKX's, and is not success here.
        let zero = r#"{"code":"0","msg":"success","data":{"orderId":"1","clientOid":"oq1"}}"#;
        assert!(matches!(classify(200, zero, "oq1"), Placed::Rejected(_)));
    }

    #[test]
    fn a_refusal_arrives_inside_a_200() {
        let body = r#"{"code":"40762","msg":"The order size is greater than the max open size","requestTime":1695806875837,"data":null}"#;
        match classify(200, body, "oq1") {
            Placed::Rejected(r) => {
                assert_eq!(r.code, Some(40762));
                assert!(r.message.contains("max open size"), "{}", r.message);
            }
            other => panic!("a non-zero code is a refusal: {other:?}"),
        }
    }

    #[test]
    fn an_answer_this_adapter_cannot_read_concludes_nothing() {
        // Not a refusal: a refusal invites a resend and the order may be
        // resting.
        match classify(200, "<html>captive portal</html>", "oq1") {
            Placed::Unknown(u) => assert_eq!(u.client_id, "oq1"),
            other => panic!("unreadable bytes are not a refusal: {other:?}"),
        }
    }

    #[test]
    fn the_signature_is_over_milliseconds_not_an_iso_stamp() {
        // The habit most likely to be carried over from the venue that
        // shares this scheme. The venue reports it as a bad signature,
        // naming neither the timestamp nor the format.
        let millis = sign(
            b"secret",
            "1695806875837",
            "POST",
            "/api/v3/trade/place-order",
            "{}",
        );
        let iso = sign(
            b"secret",
            "2023-09-27T09:27:55.837Z",
            "POST",
            "/api/v3/trade/place-order",
            "{}",
        );
        assert_ne!(millis, iso);
        // And each input participates.
        assert_ne!(
            millis,
            sign(
                b"secret",
                "1695806875837",
                "GET",
                "/api/v3/trade/place-order",
                "{}"
            )
        );
        assert_ne!(
            millis,
            sign(
                b"secret",
                "1695806875837",
                "POST",
                "/api/v3/trade/place-order",
                ""
            )
        );
    }

    #[test]
    fn an_order_body_carries_coins_and_the_product_line() {
        use oq_types::{PriceTicks, QtyLots, Side, TimeInForce};
        let order = NewOrder {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            limit_price: Some(PriceTicks(7_831_340)),
            qty: QtyLots(20),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq1".to_string(),
            reduce_only: false,
            position_side: crate::exec::PositionSide::OneWay,
        };
        // Coins here, unlike the venue that shares this signature.
        let instrument = Instrument::linear(2, 4);
        let body = order_body(&order, &instrument, "USDT-FUTURES");
        assert!(body.contains(r#""category":"USDT-FUTURES""#), "{body}");
        assert!(body.contains(r#""qty":"0.0020""#), "{body}");
        assert!(body.contains(r#""orderType":"limit""#), "{body}");
        assert!(body.contains(r#""price":"78313.40""#), "{body}");
        // A market order carries no price: one sent with a stale limit
        // is an order at a price nobody chose.
        let market = NewOrder {
            limit_price: None,
            ..order
        };
        let body = order_body(&market, &instrument, "USDT-FUTURES");
        assert!(body.contains(r#""orderType":"market""#), "{body}");
        assert!(!body.contains("price"), "{body}");
    }
}

#[cfg(test)]
mod demo {
    use super::headers;

    /// The test deployment is chosen by a header on the shared host.
    /// Without it, a client asked for the test deployment traded the real
    /// account.
    #[test]
    fn a_demo_client_marks_every_private_request() {
        let demo = headers("k", "s", "1", "p", true);
        assert!(demo.contains(&("paptrading", "1")));
        let real = headers("k", "s", "1", "p", false);
        assert!(!real.iter().any(|(k, _)| *k == "paptrading"));
    }
}
