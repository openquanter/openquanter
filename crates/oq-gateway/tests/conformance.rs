//! Both execution adapters, driven through the contract.
//!
//! `FR-VENUE-2`. The suite is in `oq_gateway::conformance`; this is what
//! makes it true of the adapters that ship, using payloads each venue
//! actually sent.
//!
//! # The two venues disagree about almost everything, and both conform
//!
//! Binance answers a refusal with an HTTP status. OKX answers one with
//! HTTP 200 and a body carrying two codes — the envelope's and the
//! order's — and a request can succeed while the order inside it was
//! refused. An adapter that read the status alone would pass a suite
//! written around Binance and lose money on OKX.
//!
//! That is why the suite asks each adapter what its own bytes mean
//! rather than carrying fixtures of its own. What it checks is the
//! meaning, and meaning is the only thing two venues have in common.

use oq_gateway::conformance::{Responses, check};

/// Payloads Binance sent, and what each one means.
fn binance() -> Responses {
    Responses {
        venue: "binance-perp",
        client_id: "oq-1",
        accepted: r#"{"orderId":283194212,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"oq-1","price":"60000","avgPrice":"0.00","origQty":"0.002","executedQty":"0"}"#,
        accepted_venue_id: "283194212",
        rejected: (
            400,
            r#"{"code":-4014,"msg":"Price not increased by tick size."}"#,
        ),
        rejected_code: Some(-4014),
        unavailable: (503, "<html>service unavailable</html>"),
        absent: r#"{"code":-2013,"msg":"Order does not exist."}"#,
        present: r#"{"orderId":283194212,"symbol":"BTCUSDT","status":"FILLED","clientOrderId":"oq-1","executedQty":"0.002"}"#,
        foreign: "<html>captive portal</html>",
    }
}

/// Payloads OKX sent, and what each one means.
fn okx() -> Responses {
    Responses {
        venue: "okx-swap",
        client_id: "oq0001",
        accepted: r#"{"code":"0","msg":"","data":[{"clOrdId":"oq0001","ordId":"312269865356374016","tag":"","sCode":"0","sMsg":""}]}"#,
        accepted_venue_id: "312269865356374016",
        rejected: (
            200,
            r#"{"code":"1","msg":"","data":[{"clOrdId":"oq0001","ordId":"","sCode":"51008","sMsg":"Order placement failed due to insufficient balance"}]}"#,
        ),
        rejected_code: Some(51_008),
        unavailable: (502, "<html>bad gateway</html>"),
        absent: r#"{"code":"51603","msg":"Order does not exist","data":[]}"#,
        present: r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ordId":"312269865356374016","clOrdId":"oq0001","state":"live","accFillSz":"0","sz":"5"}]}"#,
        foreign: "<html>captive portal</html>",
    }
}

/// Payloads Kraken Futures documents, and what each one means.
///
/// Documented shapes, not captured ones — the module header says so and
/// this is where that costs something: if the venue's real refusal
/// differs from the one written here, this suite passes and the adapter
/// is still wrong. It is the same standard the other two were written
/// to before they were run.
fn kraken() -> Responses {
    Responses {
        venue: "kraken-futures",
        client_id: "oq0001",
        accepted: r#"{"result":"success","sendStatus":{"order_id":"179f9af8-e45e-469d-b3e9-2fd4675cb7d0","status":"placed","cliOrdId":"oq0001","receivedTime":"2019-09-05T16:33:50.734Z"},"serverTime":"2019-09-05T16:33:50.734Z"}"#,
        // A name, not a number. The whole reason `venue_id` is text.
        accepted_venue_id: "179f9af8-e45e-469d-b3e9-2fd4675cb7d0",
        rejected: (
            200,
            r#"{"result":"success","sendStatus":{"order_id":"","status":"insufficientAvailableFunds","cliOrdId":"oq0001","orderEvents":[]},"serverTime":"2019-09-05T16:33:50.734Z"}"#,
        ),
        // The venue names its refusals in words rather than in codes.
        rejected_code: None,
        unavailable: (502, "<html>bad gateway</html>"),
        absent: r#"{"result":"success","orders":[],"serverTime":"2019-09-05T16:33:50.734Z"}"#,
        present: r#"{"result":"success","orders":[{"order_id":"179f9af8-e45e-469d-b3e9-2fd4675cb7d0","cliOrdId":"oq0001","status":"untouched","filledSize":0}],"serverTime":"2019-09-05T16:33:50.734Z"}"#,
        foreign: "<html>captive portal</html>",
    }
}

#[test]
fn the_kraken_adapter_conforms() {
    let r = check(
        &kraken(),
        oq_gateway::kraken::classify,
        oq_gateway::kraken::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("kraken-futures"));
}

/// Payloads Bitget's unified-account documentation shows.
///
/// Documented shapes again, and here the exposure is larger than usual:
/// the place-order response is read from the venue's docs, but the
/// query shapes are written to its naming rather than quoted. The
/// adapter's module header says which is which.
fn bitget() -> Responses {
    Responses {
        venue: "bitget-uta",
        client_id: "oq0001",
        accepted: r#"{"code":"00000","msg":"success","requestTime":1695806875837,"data":{"clientOid":"oq0001","orderId":"121211212122"}}"#,
        accepted_venue_id: "121211212122",
        rejected: (
            200,
            r#"{"code":"40762","msg":"The order size is greater than the max open size","requestTime":1695806875837,"data":null}"#,
        ),
        rejected_code: Some(40_762),
        unavailable: (502, "<html>bad gateway</html>"),
        absent: r#"{"code":"00000","msg":"success","requestTime":1695806875837,"data":[]}"#,
        present: r#"{"code":"00000","msg":"success","requestTime":1695806875837,"data":[{"orderId":"121211212122","clientOid":"oq0001","status":"live","filledQty":"0"}]}"#,
        foreign: "<html>captive portal</html>",
    }
}

#[test]
fn the_bitget_adapter_conforms() {
    let r = check(
        &bitget(),
        oq_gateway::bitget::classify,
        oq_gateway::bitget::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("bitget-uta"));
}

/// Payloads from Backpack's published OpenAPI specification.
///
/// Better evidence than the other adapters started with — a schema
/// rather than a prose example — and still not a placed order. The
/// client id is a number in every one of them, which is the constraint
/// `IdRules::BACKPACK` exists for.
fn backpack() -> Responses {
    Responses {
        venue: "backpack",
        client_id: "7",
        accepted: r#"{"id":"114905014","clientId":7,"symbol":"SOL_USDC","side":"Bid","quantity":"1","executedQuantity":"0","price":"100","status":"New","createdAt":1614550000000}"#,
        accepted_venue_id: "114905014",
        rejected: (
            400,
            r#"{"code":"INVALID_ORDER","message":"Order quantity is below the minimum"}"#,
        ),
        // Its codes are words rather than numbers.
        rejected_code: None,
        unavailable: (502, "<html>bad gateway</html>"),
        absent: "[]",
        present: r#"[{"id":"114905014","clientId":7,"symbol":"SOL_USDC","side":"Bid","quantity":"1","executedQuantity":"0","price":"100","status":"New","createdAt":1614550000000}]"#,
        foreign: "<html>captive portal</html>",
    }
}

#[test]
fn the_backpack_adapter_conforms() {
    let r = check(
        &backpack(),
        oq_gateway::backpack::classify,
        oq_gateway::backpack::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("backpack"));
}

/// Payloads from Deribit's published OpenAPI specification.
///
/// The envelope is JSON-RPC rather than REST, so "the status line is
/// not the answer" is structural here rather than a venue's quirk: the
/// refusal is an `error` member and some of them arrive with a 200.
fn deribit() -> Responses {
    Responses {
        venue: "deribit",
        client_id: "oq0001",
        accepted: r#"{"jsonrpc":"2.0","id":5275,"result":{"trades":[],"order":{"order_id":"ETH-100234","order_state":"open","label":"oq0001","instrument_name":"BTC-PERPETUAL","direction":"buy","price":78313.5,"amount":10,"filled_amount":0}}}"#,
        accepted_venue_id: "ETH-100234",
        rejected: (
            200,
            r#"{"jsonrpc":"2.0","id":8163,"error":{"message":"not_enough_funds","code":10009}}"#,
        ),
        rejected_code: Some(10_009),
        unavailable: (502, "<html>bad gateway</html>"),
        absent: r#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
        present: r#"{"jsonrpc":"2.0","id":1,"result":[{"order_id":"ETH-100234","order_state":"open","label":"oq0001","instrument_name":"BTC-PERPETUAL","direction":"buy","price":78313.5,"amount":10,"filled_amount":0}]}"#,
        foreign: "<html>captive portal</html>",
    }
}

#[test]
fn the_deribit_adapter_conforms() {
    let r = check(
        &deribit(),
        oq_gateway::deribit::classify,
        oq_gateway::deribit::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("deribit"));
}

#[test]
fn the_binance_adapter_conforms() {
    let r = check(
        &binance(),
        oq_gateway::binance::classify,
        oq_gateway::binance::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("binance-perp"));
    assert!(r.checks >= 6, "the suite must actually have run: {r:?}");
}

#[test]
fn the_okx_adapter_conforms() {
    let r = check(
        &okx(),
        oq_gateway::okx::classify,
        oq_gateway::okx::order_from_query,
    );
    assert!(r.conforms(), "{}", r.summary_line("okx-swap"));
    assert!(r.checks >= 6);
}

/// The suite has to be able to fail, or passing means nothing.
///
/// An adapter that folds "the venue could not answer" into a rejection
/// is the specific defect the three-outcome contract exists to prevent:
/// a caller that believes nothing landed sends the order again, and that
/// is how a position doubles.
#[test]
fn an_adapter_that_calls_an_unanswered_request_a_refusal_is_caught() {
    fn wrong(status: u16, body: &str, client_id: &str) -> oq_gateway::exec::Placed {
        if (200..300).contains(&status) {
            oq_gateway::binance::classify(status, body, client_id)
        } else {
            oq_gateway::exec::Placed::Rejected(oq_gateway::exec::Reject {
                code: None,
                message: format!("HTTP {status}"),
            })
        }
    }

    let r = check(&binance(), wrong, oq_gateway::binance::order_from_query);
    assert!(
        !r.conforms(),
        "the suite passed an adapter that doubles positions"
    );
    assert!(
        r.failures
            .iter()
            .any(|f| f.contains("how a position doubles")),
        "{:?}",
        r.failures
    );
}

/// And the other direction: an adapter that reads OKX's refusal as an
/// acceptance, which is what happens if it trusts the HTTP status.
#[test]
fn an_adapter_that_trusts_okxs_http_status_is_caught() {
    fn trusting(status: u16, body: &str, client_id: &str) -> oq_gateway::exec::Placed {
        if (200..300).contains(&status) {
            oq_gateway::okx::ack_from(body, client_id)
        } else {
            oq_gateway::okx::classify(status, body, client_id)
        }
    }

    let r = check(&okx(), trusting, oq_gateway::okx::order_from_query);
    assert!(
        !r.conforms(),
        "an adapter reading OKX's 200-refusal as an acceptance passed the suite"
    );
}

/// A status query that answered "yes" for an order the venue does not
/// have would, after an unresolved placement, tell a caller not to
/// resend an order that never landed.
#[test]
fn an_adapter_that_cannot_say_no_such_order_is_caught() {
    fn always_there(body: &str, client_id: &str) -> Option<oq_gateway::exec::OrderAck> {
        if body.is_empty() {
            return None;
        }
        Some(oq_gateway::exec::OrderAck {
            venue_id: "1".to_string(),
            client_id: client_id.to_string(),
            status: "NEW".to_string(),
            executed_qty: "0".to_string(),
        })
    }

    let r = check(&binance(), oq_gateway::binance::classify, always_there);
    assert!(!r.conforms());
    assert!(
        r.failures.iter().any(|f| f.contains("safe to send again")),
        "{:?}",
        r.failures
    );
}

/// Every adapter the suite can drive, reported together — the form this
/// suite is actually used in: adding a venue means adding a row here.
/// Hyperliquid is not among them: it finds an order by the venue's id or
/// its own through the info endpoint, and has no `order_from_query` for
/// the suite's query cases to call.
#[test]
fn every_shipped_adapter_is_listed() {
    let reports = [
        (
            "binance-perp",
            check(
                &binance(),
                oq_gateway::binance::classify,
                oq_gateway::binance::order_from_query,
            ),
        ),
        (
            "okx-swap",
            check(
                &okx(),
                oq_gateway::okx::classify,
                oq_gateway::okx::order_from_query,
            ),
        ),
        (
            "kraken-futures",
            check(
                &kraken(),
                oq_gateway::kraken::classify,
                oq_gateway::kraken::order_from_query,
            ),
        ),
        (
            "bitget",
            check(
                &bitget(),
                oq_gateway::bitget::classify,
                oq_gateway::bitget::order_from_query,
            ),
        ),
        (
            "backpack",
            check(
                &backpack(),
                oq_gateway::backpack::classify,
                oq_gateway::backpack::order_from_query,
            ),
        ),
        (
            "deribit",
            check(
                &deribit(),
                oq_gateway::deribit::classify,
                oq_gateway::deribit::order_from_query,
            ),
        ),
    ];
    for (venue, r) in &reports {
        println!("  {}", r.summary_line(venue));
    }
    assert_eq!(
        reports.len(),
        6,
        "six adapters answer the suite's queries; every one must be driven"
    );
    assert!(reports.iter().all(|(_, r)| r.conforms()));
}
