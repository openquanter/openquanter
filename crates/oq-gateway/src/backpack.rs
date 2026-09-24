//! Backpack.
//!
//! The fifth venue, and the first that cannot be signed by hand.
//!
//! # A keypair, not a shared secret
//!
//! Every other venue here signs with HMAC: the key is a secret both
//! sides hold, and the primitive is deterministic bit arithmetic this
//! workspace writes itself. Backpack signs with **Ed25519** — a private
//! key that only this side holds, over a curve.
//!
//! That is where hand-writing stops, and `docs/VENUES.md` argues the
//! line: a hash is either right for every input or wrong for one a test
//! can be made to try, while curve arithmetic is constant-time field
//! math and a mistake in it leaks the key rather than failing a vector.
//! So `ed25519-dalek` is a dependency, taken deliberately, in the crate
//! whose budget note says every dependency here is one more thing
//! trusted with a secret. It cost less than expected — the tree overlaps
//! the TLS stack that was already present.
//!
//! # The client order id is a number
//!
//! `clientId` is a `uint32`. Not a string with a length limit, like the
//! other four: a *number*. `L4` requires a client order id that is
//! reconstructible, and a prefix scheme that works on every other venue
//! here composes `oq7`, which this venue refuses. `IdRules::BACKPACK`
//! states the constraint so a run fails at startup rather than at its
//! first order, and it checks the value rather than the length —
//! `9999999999` is ten digits and is not a `uint32`.
//!
//! # Signing covers a sorted query string
//!
//! `instruction=<what>&<params, sorted>&timestamp=<ms>&window=<ms>`.
//! The venue's own example is also in plain alphabetical order, because
//! `instruction` < `orderId` < `symbol` < `timestamp` < `window`
//! happens to hold. **Those two readings are not the same rule** and
//! this adapter implements the documented one — instruction first,
//! parameters sorted, timestamp and window last — because a parameter
//! beginning with `a` would sort ahead of `instruction` and separate
//! them. Flagged rather than resolved: a first real run against a
//! parameter like that is what settles it.
//!
//! # What is missing, and why
//!
//! No balance read. `/api/v1/positions` and `/api/v1/orders` have
//! documented field names; the collateral endpoints do not — the
//! public one returns risk-model parameters rather than an account's
//! equity, and the account query's response shape is not written down
//! anywhere this survey found. `AccountSnapshot` needs a wallet
//! balance, an unrealized P&L and a margin balance, and guessing which
//! of an undocumented response's fields are those three is the mistake
//! `kraken::parse_accounts` exists to avoid. One real response settles
//! it.
//!
//! # What this has not done
//!
//! **It has not been run against Backpack.** The signing scheme is read
//! from the venue's documentation and the response shapes from its
//! published OpenAPI specification, which is better evidence than most
//! of the adapters here started with and is still not a placed order.

use ed25519_dalek::{Signer, SigningKey};
use oq_types::Instrument;

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{Endpoint, Execution, NewOrder, OrderAck, Placed, Reject, Unresolved, decimal};
use crate::json::{field_str, raw_field};

/// A client for Backpack.
pub struct Backpack {
    base: String,
    /// The signing key, decoded once at construction.
    key: SigningKey,
    /// The verifying key as the venue wants it: base64, in a header.
    api_key: String,
    agent: ureq::Agent,
    /// How long a signed request stays valid, in milliseconds.
    window: u64,
}

impl Backpack {
    /// The one host.
    pub const HOST: &'static str = "https://api.backpack.exchange";

    /// Build a client.
    ///
    /// # Errors
    /// When testnet is requested, because Backpack publishes no test
    /// deployment, or when the secret is not a base64 Ed25519 seed. Refused here
    /// rather than at the first order: a key that decoded to the wrong
    /// bytes signs everything invalidly, and the venue reports that as
    /// a bad signature, which sends the reader to the algorithm instead
    /// of to the key.
    pub fn at(endpoint: Endpoint, creds: Credentials) -> Result<Self, String> {
        if endpoint == Endpoint::Testnet {
            return Err(
                "Backpack does not publish a testnet endpoint; refusing to use the live host"
                    .to_string(),
            );
        }
        let seed = core::str::from_utf8(creds.secret_bytes())
            .ok()
            .and_then(crate::b64::decode)
            .ok_or_else(|| "this venue's API secret is a base64 Ed25519 key".to_string())?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "an Ed25519 signing key is 32 bytes".to_string())?;
        Ok(Self {
            base: Self::HOST.to_string(),
            key: SigningKey::from_bytes(&seed),
            api_key: creds.key().to_string(),
            agent: crate::http::venue_agent(),
            window: 5_000,
        })
    }
}

// ---------------------------------------------------------------------
// Pure.
// ---------------------------------------------------------------------

/// The message that gets signed.
///
/// `params` arrive already sorted by key; this places `instruction`
/// first and the two time fields last, which is the documented order.
#[must_use]
pub fn signing_message(
    instruction: &str,
    params: &[(&str, String)],
    timestamp: i64,
    window: u64,
) -> String {
    let mut message = format!("instruction={instruction}");
    for (k, v) in params {
        message.push('&');
        message.push_str(k);
        message.push('=');
        message.push_str(v);
    }
    message.push_str(&format!("&timestamp={timestamp}&window={window}"));
    message
}

/// Sort parameters by key, which the venue requires before signing.
///
/// A separate step so it is visible: an unsorted parameter list signs
/// correctly against itself and is refused by the venue, and the error
/// names the signature rather than the order.
#[must_use]
pub fn sorted(mut params: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
    params.sort_by(|a, b| a.0.cmp(b.0));
    params
}

/// Backpack's order state in the vocabulary the rest of this reads.
///
/// `Cancelled` has two `l`s here and one in the shared vocabulary, which
/// is exactly the sort of difference that compiles.
#[must_use]
pub fn status_of(state: &str) -> Option<&'static str> {
    match state {
        "New" => Some("NEW"),
        "PartiallyFilled" => Some("PARTIALLY_FILLED"),
        "Filled" => Some("FILLED"),
        "Cancelled" => Some("CANCELED"),
        "Expired" => Some("EXPIRED"),
        // A trigger order that has fired is working, not finished.
        "Triggered" => Some("NEW"),
        _ => None,
    }
}

/// Backpack's side in the same vocabulary. `Bid` and `Ask`, not buy and
/// sell.
#[must_use]
pub fn side_of(side: &str) -> Option<&'static str> {
    match side {
        "Bid" => Some("BUY"),
        "Ask" => Some("SELL"),
        _ => None,
    }
}

/// What a placement answer meant.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    if (500..600).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }
    // An order carries an `id` the engine assigned. A refusal carries a
    // `code` and no order.
    let has_order = field_str(body, "status")
        .and_then(|s| status_of(&s))
        .is_some();
    if (200..300).contains(&status) && has_order {
        return Placed::Accepted(OrderAck {
            venue_id: field_str(body, "id").unwrap_or_default(),
            // The venue echoes it as a number; the contract carries text.
            client_id: raw_field(body, "clientId")
                .filter(|v| v != "null")
                .unwrap_or_else(|| client_id.to_string()),
            status: field_str(body, "status").unwrap_or_default(),
            executed_qty: field_str(body, "executedQuantity").unwrap_or_else(|| "0".to_string()),
        });
    }
    match field_str(body, "code") {
        Some(code) => Placed::Rejected(Reject {
            code: None,
            message: field_str(body, "message").unwrap_or(code),
        }),
        // Not this venue's answer at all. Nothing can be concluded: the
        // order may be resting, and a refusal invites a resend into a
        // position that already exists.
        None => Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unreadable answer: {}", truncate(body)),
        }),
    }
}

/// Read one order out of a query response.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<OrderAck> {
    let entry = crate::json::objects(body)
        .into_iter()
        .find(|entry| raw_field(entry, "clientId").as_deref() == Some(client_id))?;
    let status = field_str(&entry, "status")?;
    status_of(&status)?;
    Some(OrderAck {
        venue_id: field_str(&entry, "id").unwrap_or_default(),
        client_id: client_id.to_string(),
        status,
        executed_qty: field_str(&entry, "executedQuantity").unwrap_or_else(|| "0".to_string()),
    })
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Encode the request schema without turning numeric and boolean fields
/// into strings. The signing message still uses their textual form.
fn request_body(params: &[(&str, String)]) -> String {
    let fields = params
        .iter()
        .map(|(key, value)| {
            let value = match *key {
                "clientId" => value.clone(),
                "autoBorrow" | "autoBorrowRepay" | "autoLend" | "autoLendRedeem" | "postOnly"
                | "reduceOnly" => value.clone(),
                _ => json_string(value),
            };
            format!("{}:{value}", json_string(key))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{fields}}}")
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

// ---------------------------------------------------------------------
// Account reads.
// ---------------------------------------------------------------------

/// Read `/api/v1/positions`.
///
/// `netQuantity` is signed and there is one net position per market —
/// no hedged legs — so unlike every other adapter here the sign is
/// taken from the number, because there is no leg name to take it
/// from. Stated rather than left implicit: the rule elsewhere is
/// "direction from the name", and this is the exception the venue
/// forces.
///
/// # Errors
/// When a field this build needs cannot be read.
pub fn parse_positions(body: &str) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
    let mut out = Vec::new();
    for item in crate::json::objects(body) {
        let Some(symbol) = field_str(&item, "symbol") else {
            continue;
        };
        let Some(quantity) =
            field_str(&item, "netQuantity").or_else(|| raw_field(&item, "netQuantity"))
        else {
            return Err(VenueError::Malformed {
                what: "netQuantity",
                body: item.chars().take(200).collect(),
            });
        };
        let Ok(amount) = quantity.parse::<f64>() else {
            return Err(VenueError::Malformed {
                what: "netQuantity",
                body: item.chars().take(200).collect(),
            });
        };
        let entry_text = field_str(&item, "entryPrice")
            .or_else(|| raw_field(&item, "entryPrice"))
            .unwrap_or_default();
        out.push(crate::binance::PositionSnapshot {
            symbol,
            // One net position per market.
            position_side: "BOTH".to_string(),
            amount,
            amount_text: quantity,
            entry_price: entry_text.parse::<f64>().unwrap_or_default(),
            entry_text,
            unrealized: field_str(&item, "pnlUnrealized")
                .or_else(|| raw_field(&item, "pnlUnrealized"))
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Read `/api/v1/orders`.
///
/// # Errors
/// When an order carries a state this build does not know.
pub fn parse_open_orders(body: &str) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
    let mut out = Vec::new();
    for item in crate::json::objects(body) {
        let Some(id) = field_str(&item, "id") else {
            continue;
        };
        let Some(status) = field_str(&item, "status").and_then(|s| status_of(&s)) else {
            return Err(VenueError::Malformed {
                what: "order status",
                body: item.chars().take(200).collect(),
            });
        };
        let Some(side) = field_str(&item, "side").and_then(|s| side_of(&s)) else {
            return Err(VenueError::Malformed {
                what: "order side",
                body: item.chars().take(200).collect(),
            });
        };
        let number = |key: &str| -> f64 {
            field_str(&item, key)
                .or_else(|| raw_field(&item, key))
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default()
        };
        out.push(crate::binance::OpenOrder {
            symbol: field_str(&item, "symbol").unwrap_or_default(),
            order_id: id,
            // A number on the wire, carried as the text it arrived as.
            client_order_id: raw_field(&item, "clientId")
                .filter(|v| v != "null")
                .unwrap_or_default(),
            side: side.to_string(),
            position_side: "BOTH".to_string(),
            price: number("price"),
            orig_qty: number("quantity"),
            executed_qty: number("executedQuantity"),
            status: status.to_string(),
        });
    }
    Ok(out)
}

impl Backpack {
    /// Open positions.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports.
    pub fn positions(&self) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
        let body = self.send("GET", "/api/v1/positions", "positionQuery", &[])?;
        parse_positions(&body)
    }

    /// Everything resting, across markets or on one.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports.
    pub fn open_orders(
        &self,
        symbol: Option<&str>,
    ) -> Result<Vec<crate::binance::OpenOrder>, VenueError> {
        let params = symbol.map_or_else(Vec::new, |s| sorted(vec![("symbol", s.to_string())]));
        let body = self.send("GET", "/api/v1/orders", "orderQueryAll", &params)?;
        parse_open_orders(&body)
    }
}

impl Backpack {
    /// Send a signed request.
    fn send(
        &self,
        method: &str,
        path: &str,
        instruction: &str,
        params: &[(&'static str, String)],
    ) -> Result<String, VenueError> {
        let timestamp = crate::binance::now_ms();
        let message = signing_message(instruction, params, timestamp, self.window);
        let signature = crate::b64::encode(&self.key.sign(message.as_bytes()).to_bytes());
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let timestamp = timestamp.to_string();
        let window = self.window.to_string();
        let headers: [(&str, &str); 5] = [
            ("X-API-Key", &self.api_key),
            ("X-Signature", &signature),
            ("X-Timestamp", &timestamp),
            ("X-Window", &window),
            ("Content-Type", "application/json"),
        ];
        // The body carries the same parameters a GET puts in its query,
        // which is why `sorted` is applied to one list and used by both.
        let body = if method == "GET" {
            String::new()
        } else {
            request_body(params)
        };
        let url = if query.is_empty() || method != "GET" {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        // Three branches rather than one, because the builders are
        // different types. The header list is built once above for the
        // reason the OKX adapter states: written out per branch is how
        // one path ends up missing one.
        let sent = match method {
            "POST" => {
                let mut r = self.agent.post(&url);
                for (k, v) in &headers {
                    r = r.header(*k, *v);
                }
                r.send(&body)
            }
            "DELETE" => {
                let mut r = self.agent.delete(&url);
                for (k, v) in &headers {
                    r = r.header(*k, *v);
                }
                r.force_send_body().send(&body)
            }
            _ => {
                let mut r = self.agent.get(&url);
                for (k, v) in &headers {
                    r = r.header(*k, *v);
                }
                r.call()
            }
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

impl Execution for Backpack {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        if !crate::broker::IdRules::BACKPACK.accepts(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable here: this venue's clientId is a uint32",
                    order.client_id
                ),
            });
        }
        let params = order_params(order, instrument);

        match self.send("POST", "/api/v1/order", "orderExecute", &params) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        let params = sorted(vec![
            ("clientId", client_id.to_string()),
            ("symbol", symbol.to_string()),
        ]);
        match self.send("DELETE", "/api/v1/order", "orderCancel", &params) {
            Ok(text) => classify(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(&self, symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let params = sorted(vec![
            ("clientId", client_id.to_string()),
            ("symbol", symbol.to_string()),
        ]);
        match self.send("GET", "/api/v1/order", "orderQuery", &params) {
            Ok(body) => Ok(order_from_query(&body, client_id)),
            Err(VenueError::Venue { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod account_reads {
    use super::*;

    #[test]
    fn the_sign_comes_from_the_number_here_and_that_is_the_exception() {
        // Every other adapter takes direction from a leg name and
        // magnitude from the number. This venue reports one net
        // position per market with no leg name at all, so the number
        // is all there is.
        let body = r#"[{"symbol":"SOL_USDC_PERP","netQuantity":"-12.5","entryPrice":"98.4",
            "pnlUnrealized":"3.25","markPrice":"98.1"},
            {"symbol":"BTC_USDC_PERP","netQuantity":"0.05","entryPrice":"78000",
            "pnlUnrealized":"-1.5","markPrice":"77950"}]"#;
        let legs = parse_positions(body).expect("readable positions");
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].amount_text, "-12.5");
        assert!((legs[0].amount - -12.5).abs() < 1e-9);
        assert_eq!(legs[0].position_side, "BOTH");
        assert_eq!(legs[1].amount_text, "0.05");
        assert!((legs[1].unrealized - -1.5).abs() < 1e-9);
    }

    #[test]
    fn a_resting_order_keeps_its_numeric_client_id_as_text() {
        let body = r#"[{"id":"114905014","clientId":7,"symbol":"SOL_USDC_PERP","side":"Bid",
            "quantity":"1","executedQuantity":"0.25","price":"100","status":"PartiallyFilled",
            "createdAt":1614550000000}]"#;
        let orders = parse_open_orders(body).expect("readable orders");
        assert_eq!(orders[0].order_id, "114905014");
        assert_eq!(orders[0].client_order_id, "7", "a number, carried as text");
        assert_eq!(orders[0].status, "PARTIALLY_FILLED");
        assert_eq!(orders[0].side, "BUY");
        assert!((orders[0].executed_qty - 0.25).abs() < 1e-9);
    }

    #[test]
    fn an_order_state_this_build_does_not_know_is_refused() {
        let body = r#"[{"id":"1","clientId":1,"symbol":"X","side":"Bid","quantity":"1",
            "executedQuantity":"0","price":"1","status":"Teleported"}]"#;
        assert!(parse_open_orders(body).is_err());
    }

    #[test]
    fn an_empty_account_reads_as_empty_rather_than_failing() {
        assert!(parse_positions("[]").expect("a flat account").is_empty());
        assert!(parse_open_orders("[]").expect("nothing resting").is_empty());
    }
}

/// The signed parameters for a new order, sorted as the signature wants.
#[must_use]
pub fn order_params(order: &NewOrder, instrument: &Instrument) -> Vec<(&'static str, String)> {
    let side = match order.side {
        oq_types::Side::Buy => "Bid",
        oq_types::Side::Sell => "Ask",
    };
    let mut params: Vec<(&'static str, String)> = vec![
        ("clientId", order.client_id.clone()),
        (
            "orderType",
            if order.limit_price.is_some() {
                "Limit"
            } else {
                "Market"
            }
            .to_string(),
        ),
        ("quantity", decimal(order.qty.0, instrument.qty_scale)),
        ("side", side.to_string()),
        ("symbol", order.symbol.clone()),
    ];
    if let Some(price) = order.limit_price {
        params.push(("price", decimal(price.0, instrument.price_scale)));
        // Stated rather than left to the default, which is GTC: an
        // IOC sent as that rests.
        params.push((
            "timeInForce",
            match order.tif {
                oq_types::TimeInForce::GoodTilCancel => "GTC",
                oq_types::TimeInForce::ImmediateOrCancel => "IOC",
                oq_types::TimeInForce::FillOrKill => "FOK",
            }
            .to_string(),
        ));
    }
    if order.reduce_only {
        params.push(("reduceOnly", "true".to_string()));
    }
    sorted(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signing_message_is_the_documented_order() {
        // The venue's own example, reproduced.
        let params = sorted(vec![
            ("symbol", "BTC_USDT".to_string()),
            ("orderId", "28".to_string()),
        ]);
        assert_eq!(
            signing_message("orderCancel", &params, 1_614_550_000_000, 5_000),
            "instruction=orderCancel&orderId=28&symbol=BTC_USDT&timestamp=1614550000000&window=5000"
        );
    }

    #[test]
    fn instruction_leads_even_when_a_parameter_would_sort_ahead_of_it() {
        // Where the two readings of the documentation part company. The
        // venue's example cannot distinguish them because `instruction`
        // happens to sort first in it; a parameter beginning with `a`
        // does. This adapter follows the prose, and the module header
        // says a real run is what settles it.
        let params = sorted(vec![
            ("autoLend", "true".to_string()),
            ("symbol", "SOL_USDC".to_string()),
        ]);
        let message = signing_message("orderExecute", &params, 1, 5_000);
        assert!(
            message.starts_with("instruction=orderExecute&autoLend=true"),
            "{message}"
        );
        assert!(message.ends_with("&timestamp=1&window=5000"), "{message}");
    }

    #[test]
    fn a_client_id_here_is_a_number() {
        use crate::broker::IdRules;
        // `L4` wants a reconstructible id, and every other venue takes
        // the prefix scheme's `oq7`. This one does not.
        assert!(!IdRules::BACKPACK.accepts("oq7"));
        assert!(IdRules::BACKPACK.accepts("7"));
        assert!(IdRules::BACKPACK.accepts("4294967295"));
        // Ten digits, and still not a uint32.
        assert!(!IdRules::BACKPACK.accepts("9999999999"));
        assert!(!IdRules::BACKPACK.accepts(""));
    }

    #[test]
    fn cancelled_with_two_ls_becomes_canceled_with_one() {
        // The sort of difference that compiles. `oq-live` matches on the
        // shared spelling and would never see this order end.
        assert_eq!(status_of("Cancelled"), Some("CANCELED"));
        assert_eq!(status_of("PartiallyFilled"), Some("PARTIALLY_FILLED"));
        assert_eq!(status_of("New"), Some("NEW"));
        // A fired trigger is working, not finished.
        assert_eq!(status_of("Triggered"), Some("NEW"));
        assert_eq!(status_of("Sideways"), None);
        assert_eq!(side_of("Bid"), Some("BUY"));
        assert_eq!(side_of("Ask"), Some("SELL"));
    }

    #[test]
    fn an_order_is_read_from_the_shape_the_spec_publishes() {
        let body = r#"{"id":"114905014","clientId":7,"symbol":"SOL_USDC","side":"Bid","quantity":"1","executedQuantity":"0","price":"100","status":"New","createdAt":1614550000000}"#;
        match classify(200, body, "7") {
            Placed::Accepted(a) => {
                assert_eq!(a.venue_id, "114905014");
                assert_eq!(a.client_id, "7", "echoed as a number, carried as text");
                assert_eq!(a.status, "New");
            }
            other => panic!("a placed order is an acceptance: {other:?}"),
        }
    }

    #[test]
    fn request_json_preserves_schema_types_and_escapes_strings() {
        let body = request_body(&[
            ("clientId", "7".to_string()),
            ("reduceOnly", "true".to_string()),
            ("symbol", "SOL_\"USD".to_string()),
        ]);
        assert_eq!(
            body,
            r#"{"clientId":7,"reduceOnly":true,"symbol":"SOL_\"USD"}"#
        );
    }

    #[test]
    fn an_order_query_matches_the_client_id_field_not_an_unrelated_number() {
        let body = r#"[{"id":"wrong","clientId":70,"price":"7","status":"New"},
            {"id":"right","clientId":7,"price":"70","status":"New"}]"#;
        let order = order_from_query(body, "7").expect("the exact client id");
        assert_eq!(order.venue_id, "right");
    }

    #[test]
    fn testnet_never_falls_through_to_the_live_host() {
        let result = Backpack::at(Endpoint::Testnet, Credentials::new("key", "secret"));
        assert!(result.is_err());
        assert!(
            result
                .err()
                .expect("must be refused")
                .contains("does not publish a testnet")
        );
    }

    #[test]
    fn an_answer_this_adapter_cannot_read_concludes_nothing() {
        match classify(200, "<html>captive portal</html>", "7") {
            Placed::Unknown(u) => assert_eq!(u.client_id, "7"),
            other => panic!("unreadable bytes are not a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_signature_is_over_the_message_and_moves_with_it() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let one = key.sign(b"instruction=orderExecute&timestamp=1&window=5000");
        let two = key.sign(b"instruction=orderExecute&timestamp=2&window=5000");
        assert_ne!(one.to_bytes(), two.to_bytes());
        // And it is 64 bytes, which base64 renders in 88 characters.
        assert_eq!(crate::b64::encode(&one.to_bytes()).len(), 88);
    }
}
