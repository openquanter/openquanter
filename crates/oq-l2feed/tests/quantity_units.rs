//! A size means different things on different venues, and the
//! instrument definition is the only place that says which.
//!
//! Both venues captured here send a bare decimal string for size, in a
//! field with a two-letter name, over a websocket. Nothing in a message
//! distinguishes one bitcoin from one contract worth a hundredth of
//! one. Only the instrument definition does, and it did not carry the
//! distinction until a hundredfold error had somewhere to hide.
//!
//! This test walks the path a consumer walks — venue, symbol,
//! instrument, cash per tick — and pins the answer for both, because
//! the failure it guards against produces no error and no warning: the
//! number parses, sums, and is wrong.

use oq_l2feed::venue;
use oq_types::CONTRACT_SCALE;

#[test]
fn a_venue_that_quotes_the_asset_and_one_that_quotes_contracts_disagree() {
    let binance = venue::by_id("binance-perp").expect("venue");
    let okx = venue::by_id("okx-swap").expect("venue");

    let b = binance.instrument("BTCUSDT").expect("listed");
    let o = okx.instrument("BTCUSDT").expect("listed");

    // Binance USD-M: a quantity of 1 is one bitcoin.
    assert_eq!(
        b.contract_size, CONTRACT_SCALE,
        "a quantity here is the asset itself"
    );

    // OKX swaps: a size of 1 is one contract, worth 0.01 BTC.
    assert_eq!(
        o.contract_size,
        CONTRACT_SCALE / 100,
        "a size here counts contracts of a hundredth of the asset"
    );

    // The consequence, stated in the unit that matters. Reading an OKX
    // size as though it were an amount of the asset overstates every
    // notional built from it by exactly this factor.
    let ratio = b.contract_size / o.contract_size;
    assert_eq!(ratio, 100);
}

#[test]
fn cash_per_tick_can_agree_while_the_contracts_do_not() {
    let binance = venue::by_id("binance-perp").expect("venue");
    let okx = venue::by_id("okx-swap").expect("venue");

    let b = binance.instrument("BTCUSDT").expect("listed");
    let o = okx.instrument("BTCUSDT").expect("listed");

    // Measured, not assumed — this assertion was written the other way
    // round first and the test disproved it. The two venues differ in
    // quoting precision *and* in what a size counts, and on BTC the two
    // differences cancel exactly: 0.01 USDT x 0.001 BTC and 0.1 USDT x
    // 0.0001 BTC are both a hundred-thousandth of a USDT.
    assert_ne!((b.price_scale, b.qty_scale), (o.price_scale, o.qty_scale));
    assert_ne!(b.contract_size, o.contract_size);
    assert_eq!(b.tick_cash(), o.tick_cash());

    // Which is the reason the contract size is stored rather than
    // inferred. Cash per tick is a product of three things, so equal
    // products say nothing about the factors: anything reading a size
    // as an amount of the asset because the money came out right would
    // be right here by luck and wrong on the next contract.
    assert_eq!(b.tick_cash(), Some(1_000));
}

#[test]
fn a_contract_size_cannot_be_recovered_from_cash_per_tick() {
    // The same conclusion stated as the general fact, so it survives a
    // relisting that ends the coincidence above.
    let quoting_the_asset = oq_types::Instrument::linear(2, 3);
    let quoting_contracts = oq_types::Instrument::sized(1, 2, CONTRACT_SCALE / 100);

    assert_eq!(quoting_the_asset.tick_cash(), quoting_contracts.tick_cash());
    assert_ne!(
        quoting_the_asset.contract_size,
        quoting_contracts.contract_size
    );
}
