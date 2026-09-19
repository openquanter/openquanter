//! Deribit, over JSON-RPC.
//!
//! The sixth venue, and where Coinbase's perpetuals went: International
//! Exchange stopped serving derivatives on 2026-09-09 and moved them to
//! a Deribit-powered gateway. An adapter for the INTX REST API would
//! have been an adapter to a switched-off service, which is what
//! reading the venue's own documentation before writing code bought.
//!
//! # A different protocol family, not a REST variation
//!
//! Every other venue here is REST: a path, a verb, parameters, a JSON
//! body. This one is **JSON-RPC** — an envelope carrying `jsonrpc`,
//! `id`, `method` and `params`, with the answer in `result` or in
//! `error`. Over HTTP the method is the path and the params are the
//! query string, which makes it look like REST until something fails:
//! a refusal is `error.code` inside an envelope, and the transport
//! status is about the transport.
//!
//! # `label` is a first-class client order id
//!
//! Up to 64 characters, and `/private/get_order_state_by_label` reads an
//! order back by it. `L4` wants an id the caller chooses and can
//! reconstruct; on most venues that id is something the venue tolerates,
//! and here it is something the venue indexes.
//!
//! # Amounts are in USD on a perpetual
//!
//! A third unit, after coins and contracts: `amount` and `filled_amount`
//! are USD for perpetual and inverse futures, and the base coin for
//! linear futures. They are also JSON *numbers* rather than decimal
//! strings. The adapter keeps the venue's own text, as everywhere else,
//! and `Instrument::contract_size` is what tells the layers above what
//! one unit is worth — but a caller pointing this at an inverse
//! contract is counting dollars, and nothing in the type says so.
//!
//! # Basic authentication, deliberately
//!
//! The venue offers three: an OAuth2 bearer token from `/public/auth`,
//! HTTP Basic, and a signature scheme. This uses Basic, because a
//! bearer token has a lifetime and a refresh, and a token that expires
//! mid-run is a new failure mode on the path that places orders. Basic
//! sends the credential on every request, which is the cost, and every
//! request is already over TLS.
//!
//! # What this has not done
//!
//! **It has not been run against Deribit.** The envelope, the order
//! fields and the error shape are read from the venue's published
//! OpenAPI specification rather than from prose, which is the best
//! evidence any adapter here has started with. It is still not a placed
//! order.

use core::time::Duration;

use oq_types::Instrument;

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{Endpoint, Execution, NewOrder, OrderAck, Placed, Reject, Unresolved, decimal};
use crate::json::{field_i64, field_str, object_containing, raw_field};

/// A client for one Deribit deployment.
pub struct Deribit {
    base: String,
    /// `Basic <base64(client_id:client_secret)>`, built once.
    authorization: String,
    agent: ureq::Agent,
}

impl Deribit {
    /// Production.
    pub const MAINNET: &'static str = "https://www.deribit.com/api/v2";
    /// The test deployment, which is where anything new goes first.
    pub const TESTNET: &'static str = "https://test.deribit.com/api/v2";

    /// Build a client against a named deployment.
    #[must_use]
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Self {
        let base = match endpoint {
            Endpoint::Testnet => Self::TESTNET,
            Endpoint::Live => Self::MAINNET,
        };
        let secret = core::str::from_utf8(creds.secret_bytes())
            .unwrap_or_default()
            .to_string();
        Self {
            base: base.to_string(),
            authorization: format!(
                "Basic {}",
                crate::b64::encode(format!("{}:{secret}", creds.key()).as_bytes())
            ),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(45)))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }
}

// ---------------------------------------------------------------------
// Pure.
// ---------------------------------------------------------------------

/// Deribit's order state in the vocabulary the rest of this reads.
///
/// `cancelled` has two `l`s here, as it does on one other venue, and
/// one in the shared spelling. An untriggered or triggered conditional
/// order is working rather than finished, so both read as `NEW`: the
/// distinction matters to a strategy that placed a stop and not to a
/// book deciding whether the order is still out there.
#[must_use]
pub fn status_of(state: &str) -> Option<&'static str> {
    match state {
        "open" | "untriggered" | "triggered" => Some("NEW"),
        "filled" => Some("FILLED"),
        "cancelled" => Some("CANCELED"),
        "rejected" => Some("REJECTED"),
        _ => None,
    }
}

/// Deribit's direction in the same vocabulary.
#[must_use]
pub fn side_of(direction: &str) -> Option<&'static str> {
    match direction {
        "buy" => Some("BUY"),
        "sell" => Some("SELL"),
        _ => None,
    }
}

/// Whether Deribit will accept this as a `label`.
#[must_use]
pub fn valid_client_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= 64
}

/// The query string for a new order.
///
/// `buy` and `sell` are different methods here rather than a `side`
/// parameter, so the caller's side chooses the path. Returned together
/// so the two cannot drift apart.
#[must_use]
pub fn order_request(order: &NewOrder, instrument: &Instrument) -> (&'static str, String) {
    let method = match order.side {
        oq_types::Side::Buy => "private/buy",
        oq_types::Side::Sell => "private/sell",
    };
    let amount = decimal(order.qty.0, instrument.qty_scale);
    let mut query = format!(
        "instrument_name={}&amount={amount}&label={}",
        order.symbol, order.client_id
    );
    match order.limit_price {
        Some(price) => {
            query.push_str(&format!(
                "&type=limit&price={}",
                decimal(price.0, instrument.price_scale)
            ));
        }
        None => query.push_str("&type=market"),
    }
    if order.reduce_only {
        query.push_str("&reduce_only=true");
    }
    (method, query)
}

/// What a buy or sell answer meant.
///
/// The envelope decides, not the status line. A refusal is
/// `error.code` with a message beside it; an acceptance carries
/// `result.order`.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    if (500..600).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }
    // An error envelope, which this venue sends with a 4xx and — for
    // some codes — with a 200. Read the body either way.
    if let Some(error) = object_containing(body, "\"code\"")
        && body.contains("\"error\"")
    {
        return Placed::Rejected(Reject {
            code: field_i64(&error, "code"),
            message: field_str(&error, "message")
                .unwrap_or_else(|| format!("refused: {}", truncate(body))),
        });
    }
    let Some(order) = object_containing(body, "\"order_id\"") else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unreadable answer: {}", truncate(body)),
        });
    };
    let Some(state) = field_str(&order, "order_state") else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("an order with no state: {}", truncate(&order)),
        });
    };
    // `rejected` arrives inside a successful envelope: the call worked
    // and the order did not. It is a refusal, not an acceptance whose
    // status happens to say otherwise.
    if state == "rejected" {
        return Placed::Rejected(Reject {
            code: None,
            message: format!("the venue rejected the order: {}", truncate(&order)),
        });
    }
    if status_of(&state).is_none() {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("an order state this build does not know: {state}"),
        });
    }
    Placed::Accepted(OrderAck {
        venue_id: field_str(&order, "order_id").unwrap_or_default(),
        // Echoed as `label`. The caller's own when the venue omits it,
        // because losing the handle to a missing field would defeat the
        // point of having chosen it.
        client_id: field_str(&order, "label")
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| client_id.to_string()),
        status: state,
        // A number in the payload rather than a decimal string, so it
        // is read raw: the digits the venue sent are the ones kept.
        executed_qty: raw_field(&order, "filled_amount").unwrap_or_else(|| "0".to_string()),
    })
}

/// Read one order out of a state-by-label response.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    if body.contains("\"error\"") {
        return None;
    }
    let entry = object_containing(body, &format!("\"{client_id}\""))?;
    let state = field_str(&entry, "order_state")?;
    status_of(&state)?;
    Some(OrderAck {
        venue_id: field_str(&entry, "order_id").unwrap_or_default(),
        client_id: client_id.to_string(),
        status: state,
        executed_qty: raw_field(&entry, "filled_amount").unwrap_or_else(|| "0".to_string()),
    })
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

impl Deribit {
    /// Call a method. Over HTTP the method is the path.
    fn call(&self, method: &str, query: &str) -> Result<String, VenueError> {
        let url = if query.is_empty() {
            format!("{}/{method}", self.base)
        } else {
            format!("{}/{method}?{query}", self.base)
        };
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", &self.authorization)
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

impl Execution for Deribit {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        if !valid_client_id(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable here: a label is at most 64 characters",
                    order.client_id
                ),
            });
        }
        let (method, query) = order_request(order, instrument);
        match self.call(method, &query) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    /// Withdraw every order carrying this label.
    ///
    /// By label rather than by order id, for the reason `L4` gives: the
    /// id the caller chose is the handle that survives a placement whose
    /// answer never arrived. This venue indexes on it, so the cancel
    /// needs nothing the caller might not have.
    fn cancel(&self, _symbol: &str, client_id: &str) -> Placed {
        match self.call("private/cancel_by_label", &format!("label={client_id}")) {
            Ok(text) => classify(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(&self, _symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let body = self.call(
            "private/get_order_state_by_label",
            &format!("label={client_id}"),
        )?;
        Ok(order_from_query(&body, client_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCEPTED: &str = r#"{"jsonrpc":"2.0","id":5275,"result":{"trades":[],"order":{"time_in_force":"good_til_cancelled","reduce_only":false,"price":78313.5,"post_only":false,"order_type":"limit","order_state":"open","order_id":"ETH-100234","max_show":0.002,"last_update_timestamp":1550659803407,"label":"oq1","is_liquidation":false,"instrument_name":"BTC-PERPETUAL","filled_amount":0,"direction":"buy","creation_timestamp":1550659803407,"amount":10}}}"#;

    #[test]
    fn an_order_is_read_out_of_the_rpc_envelope() {
        match classify(200, ACCEPTED, "oq1") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, "ETH-100234");
                assert_eq!(a.client_id, "oq1", "the label is the caller's handle");
                assert_eq!(a.status, "open");
                assert_eq!(a.executed_qty, "0", "a number, kept as the digits sent");
            }
            other => panic!("an open order is an acceptance: {other:?}"),
        }
    }

    #[test]
    fn a_refusal_is_the_error_member_and_not_the_status_line() {
        // JSON-RPC puts the failure in the envelope. Some of these
        // arrive with a 200.
        let body =
            r#"{"jsonrpc":"2.0","id":8163,"error":{"message":"not_enough_funds","code":10009}}"#;
        match classify(200, body, "oq1") {
            Placed::Rejected(r) => {
                assert_eq!(r.code, Some(10009));
                assert_eq!(r.message, "not_enough_funds");
            }
            other => panic!("an error member is a refusal: {other:?}"),
        }
    }

    #[test]
    fn an_order_the_venue_rejected_is_not_an_acceptance() {
        // The call succeeded and the order did not. Reading only the
        // envelope would book a position that does not exist.
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"order":{"order_id":"ETH-1","order_state":"rejected","label":"oq1","filled_amount":0}}}"#;
        match classify(200, body, "oq1") {
            Placed::Rejected(r) => assert!(r.message.contains("rejected"), "{}", r.message),
            other => panic!("a rejected order is a refusal: {other:?}"),
        }
    }

    #[test]
    fn an_answer_this_adapter_cannot_read_concludes_nothing() {
        match classify(200, "<html>captive portal</html>", "oq1") {
            Placed::Unknown(u) => assert_eq!(u.client_id, "oq1"),
            other => panic!("unreadable bytes are not a refusal: {other:?}"),
        }
    }

    #[test]
    fn cancelled_with_two_ls_becomes_canceled_with_one() {
        assert_eq!(status_of("cancelled"), Some("CANCELED"));
        assert_eq!(status_of("open"), Some("NEW"));
        assert_eq!(status_of("filled"), Some("FILLED"));
        // A conditional order that has not fired is still working.
        assert_eq!(status_of("untriggered"), Some("NEW"));
        assert_eq!(status_of("triggered"), Some("NEW"));
        assert_eq!(status_of("sideways"), None);
        assert_eq!(side_of("buy"), Some("BUY"));
    }

    #[test]
    fn the_side_chooses_the_method_rather_than_a_parameter() {
        use oq_types::{PriceTicks, QtyLots, Side, TimeInForce};
        let order = NewOrder {
            symbol: "BTC-PERPETUAL".to_string(),
            side: Side::Sell,
            limit_price: Some(PriceTicks(7_831_340)),
            qty: QtyLots(100),
            tif: TimeInForce::GoodTilCancel,
            client_id: "oq1".to_string(),
            reduce_only: true,
            position_side: crate::exec::PositionSide::OneWay,
        };
        let (method, query) = order_request(&order, &Instrument::linear(2, 0));
        assert_eq!(method, "private/sell", "a sell is a different method here");
        assert!(query.contains("label=oq1"), "{query}");
        assert!(query.contains("type=limit"), "{query}");
        assert!(query.contains("price=78313.40"), "{query}");
        assert!(query.contains("reduce_only=true"), "{query}");
        let buy = NewOrder {
            side: Side::Buy,
            limit_price: None,
            ..order
        };
        let (method, query) = order_request(&buy, &Instrument::linear(2, 0));
        assert_eq!(method, "private/buy");
        assert!(query.contains("type=market"), "{query}");
        assert!(!query.contains("price="), "{query}");
    }

    #[test]
    fn a_label_is_sixty_four_characters() {
        assert!(valid_client_id("oq1"));
        assert!(valid_client_id(&"a".repeat(64)));
        assert!(!valid_client_id(&"a".repeat(65)));
        assert!(!valid_client_id(""));
    }
}
