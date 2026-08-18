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
        accepted_venue_id: 283_194_212,
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
        accepted_venue_id: 312_269_865_356_374_016,
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
            venue_id: 1,
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

/// Both adapters, reported together — the form this suite is actually
/// used in: adding a venue means adding a row here.
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
    ];
    for (venue, r) in &reports {
        println!("  {}", r.summary_line(venue));
    }
    assert_eq!(reports.len(), 2, "two venues ship; both must be driven");
    assert!(reports.iter().all(|(_, r)| r.conforms()));
}
