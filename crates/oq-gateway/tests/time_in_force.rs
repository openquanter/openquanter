//! A limit order's time in force reaches every venue that can express
//! it, and is refused by one that cannot.
//!
//! Five adapters sent every limit order without one, and each venue's
//! default is good-til-cancelled — so an immediate-or-cancel order rested
//! on the book, an order its strategy believed could not.

use oq_gateway::exec::PositionSide;
use oq_gateway::{NewOrder, backpack, bitget, deribit, kraken};
use oq_types::{Instrument, PriceTicks, QtyLots, Side, TimeInForce};

const ALL: [TimeInForce; 3] = [
    TimeInForce::GoodTilCancel,
    TimeInForce::ImmediateOrCancel,
    TimeInForce::FillOrKill,
];

fn limit(tif: TimeInForce) -> NewOrder {
    NewOrder {
        symbol: "BTCUSDT".to_string(),
        side: Side::Buy,
        limit_price: Some(PriceTicks(7_831_340)),
        qty: QtyLots(20),
        tif,
        client_id: "12345".to_string(),
        reduce_only: false,
        position_side: PositionSide::OneWay,
    }
}

fn instrument() -> Instrument {
    Instrument::linear(2, 4)
}

#[test]
fn deribit_states_every_time_in_force() {
    for (tif, word) in
        ALL.into_iter()
            .zip(["good_til_cancelled", "immediate_or_cancel", "fill_or_kill"])
    {
        let (_, query) = deribit::order_request(&limit(tif), &instrument());
        assert!(query.contains(&format!("&time_in_force={word}")), "{query}");
    }
}

#[test]
fn bitget_states_every_time_in_force() {
    for (tif, word) in ALL.into_iter().zip(["gtc", "ioc", "fok"]) {
        let body = bitget::order_body(&limit(tif), &instrument(), "USDT-FUTURES");
        assert!(
            body.contains(&format!(r#""timeInForce":"{word}""#)),
            "{body}"
        );
    }
}

#[test]
fn backpack_states_every_time_in_force() {
    for (tif, word) in ALL.into_iter().zip(["GTC", "IOC", "FOK"]) {
        let params = backpack::order_params(&limit(tif), &instrument());
        assert!(
            params.contains(&("timeInForce", word.to_string())),
            "{params:?}"
        );
        let mut keys: Vec<_> = params.iter().map(|(k, _)| *k).collect();
        let before = keys.clone();
        keys.sort_unstable();
        assert_eq!(keys, before, "still sorted, as the signature requires");
    }
}

#[test]
fn kraken_expresses_ioc_and_refuses_fill_or_kill() {
    let gtc = kraken::order_body(&limit(TimeInForce::GoodTilCancel), &instrument()).expect("body");
    assert!(gtc.starts_with("orderType=lmt&"), "{gtc}");
    let ioc =
        kraken::order_body(&limit(TimeInForce::ImmediateOrCancel), &instrument()).expect("body");
    assert!(ioc.starts_with("orderType=ioc&"), "{ioc}");
    assert!(kraken::order_body(&limit(TimeInForce::FillOrKill), &instrument()).is_err());
}

/// A market order carries no time in force anywhere: the venues execute
/// one immediately whatever it says.
#[test]
fn a_market_order_is_unchanged() {
    let mut order = limit(TimeInForce::ImmediateOrCancel);
    order.limit_price = None;
    let (_, query) = deribit::order_request(&order, &instrument());
    assert!(!query.contains("time_in_force"), "{query}");
    let body = bitget::order_body(&order, &instrument(), "USDT-FUTURES");
    assert!(!body.contains("timeInForce"), "{body}");
    assert!(
        !backpack::order_params(&order, &instrument())
            .iter()
            .any(|(k, _)| *k == "timeInForce")
    );
    assert_eq!(
        kraken::order_body(&order, &instrument())
            .expect("body")
            .split('&')
            .next(),
        Some("orderType=mkt")
    );
}
