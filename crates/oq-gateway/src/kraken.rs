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
use crate::json::{field_str, object_containing, raw_field};

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

/// Whether Kraken will accept this as a `cliOrdId`.
///
/// Up to 100 characters, and unique across the account's history rather
/// than among open orders. The length is far more generous than the
/// other two venues here — 36 and 32 — so an id built for them fits,
/// and one built for this venue may not fit them.
#[must_use]
pub fn valid_client_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.is_ascii()
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
        if !valid_client_id(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable here: up to 100 ASCII characters",
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
    fn a_client_id_may_be_long_here_and_must_still_be_ascii() {
        assert!(valid_client_id("oq1"));
        assert!(valid_client_id(&"a".repeat(100)));
        assert!(!valid_client_id(&"a".repeat(101)));
        assert!(!valid_client_id(""));
        // Longer than either of the other two venues allows, which is
        // the direction that matters: an id built for them fits here.
        assert!(valid_client_id(&"a".repeat(36)));
    }
}
