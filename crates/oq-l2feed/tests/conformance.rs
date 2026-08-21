//! Both adapters, driven through the same contract.
//!
//! Two venues chosen because they differ in every way the seam absorbs:
//! one puts the subscription in the URL, the other sends a frame; one
//! quotes an amount of the asset, the other a count of contracts; one
//! chains depth updates by a previous-final id, the other by a sequence
//! that is `-1` when there is none.
//!
//! If the suite only passed for one of them it would be a test of that
//! one's wire format wearing the contract's name.

use oq_l2feed::conformance::{Samples, check};
use oq_l2feed::venue;

/// A Binance USD-M depth update and trade, as recorded.
const BINANCE: Samples = Samples {
    symbol: "BTCUSDT",
    depth: br#"{"e":"depthUpdate","E":1786780800123,"T":1786780800100,"s":"BTCUSDT","U":7000,"u":7005,"pu":6999,"b":[["62000.10","1.500"]],"a":[["62000.20","2.000"]]}"#,
    depth_ids: (7000, Some(6999)),
    trade: br#"{"e":"trade","E":1786780800123,"T":1786780800100,"s":"BTCUSDT","t":481923,"p":"62000.10","q":"0.500","m":false}"#,
    // Two decimal places of price, three of quantity.
    trade_price_qty: (6_200_010, 500),
    event_time_ns: 1_786_780_800_123_000_000,
    not_a_message: br#"{"result":null,"id":1}"#,
    // Verbatim from a capture: 6312 of these in one hour of BTCUSDT,
    // among 1.45 million real trades. Well formed, id-bearing, and not
    // a trade.
    non_trade: br#"{"e":"trade","E":1787151602037,"T":1787151602037,"s":"BTCUSDT","t":7979344737,"p":"0","q":"0","X":"NA","m":true,"st":1}"#,
};

/// An OKX swap depth update and trade, as recorded. A snapshot's
/// `prevSeqId` is `-1`, which is an absence rather than a number, and the
/// sample below is an incremental update so it has a real predecessor.
const OKX: Samples = Samples {
    symbol: "BTCUSDT",
    depth: br#"{"arg":{"channel":"books","instId":"BTC-USDT-SWAP"},"action":"update","data":[{"asks":[["62000.2","2","0","1"]],"bids":[["62000.1","1","0","1"]],"ts":"1786780800123","seqId":7005,"prevSeqId":6999}]}"#,
    // The convention difference the seam exists to absorb, and the suite
    // caught it as a wrong expectation on the first run: this venue does
    // not send a first id at all. It sends the last one (`seqId`) and the
    // predecessor (`prevSeqId`), so the adapter derives the first as the
    // predecessor plus one. The other venue sends both.
    depth_ids: (7000, Some(6999)),
    trade: br#"{"arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","tradeId":"481923","px":"62000.1","sz":"0.50","side":"buy","ts":"1786780800123"}]}"#,
    // One decimal place of price, two of quantity on this contract.
    trade_price_qty: (620_001, 50),
    event_time_ns: 1_786_780_800_123_000_000,
    not_a_message: br#"{"event":"subscribe","arg":{"channel":"books"}}"#,
    // Constructed rather than captured: this venue has not been seen
    // sending one. The contract is not "reject the records your venue
    // happens to send", it is "a zero price is not a price", and an
    // adapter that would pass one through is wrong before the first
    // such message arrives rather than after.
    non_trade: br#"{"arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","tradeId":"481924","px":"0","sz":"0","side":"buy","ts":"1786780800123"}]}"#,
};

#[test]
fn the_binance_adapter_conforms_to_the_contract() {
    let v = venue::by_id("binance-perp").expect("registered");
    let report = check(v.as_ref(), &BINANCE);
    assert!(
        report.passed(),
        "{} checks, {} failures:\n  {}",
        report.checks,
        report.failures.len(),
        report.failures.join("\n  ")
    );
    assert!(report.checks > 15, "only {} checks ran", report.checks);
}

#[test]
fn the_okx_adapter_conforms_to_the_same_contract() {
    let v = venue::by_id("okx-swap").expect("registered");
    let report = check(v.as_ref(), &OKX);
    assert!(
        report.passed(),
        "{} checks, {} failures:\n  {}",
        report.checks,
        report.failures.len(),
        report.failures.join("\n  ")
    );
}

#[test]
fn the_suite_reports_every_failure_rather_than_the_first() {
    // An adapter under development is wrong in several ways at once, and a
    // suite that stopped at the first would turn one afternoon into
    // several. Driving Binance's adapter with OKX's payloads produces a
    // pile of failures on purpose.
    let v = venue::by_id("binance-perp").expect("registered");
    let mismatched = Samples {
        symbol: "BTCUSDT",
        depth: OKX.depth,
        depth_ids: OKX.depth_ids,
        trade: OKX.trade,
        trade_price_qty: OKX.trade_price_qty,
        event_time_ns: OKX.event_time_ns,
        not_a_message: BINANCE.not_a_message,
        non_trade: BINANCE.non_trade,
    };
    let report = check(v.as_ref(), &mismatched);
    assert!(!report.passed(), "the wrong venue's payloads must not pass");
    assert!(
        report.failures.len() > 1,
        "only one failure reported: {:?}",
        report.failures
    );
}

#[test]
fn an_adapter_with_no_instrument_definition_fails_before_anything_else() {
    // Everything downstream is scaled by the instrument, so parsing
    // without one is parsing at a guessed scale — and the suite says that
    // rather than reporting a pile of wrong numbers.
    let v = venue::by_id("binance-perp").expect("registered");
    let unlisted = Samples {
        symbol: "NOTACONTRACT",
        ..BINANCE
    };
    let report = check(v.as_ref(), &unlisted);
    assert!(!report.passed());
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0].contains("guessed scale"),
        "{:?}",
        report.failures
    );
}
