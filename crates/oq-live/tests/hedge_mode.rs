//! A hedged account's two legs must not cancel.
//!
//! The venue reports whether an account keeps long and short apart. The
//! books were built without ever being told, so they netted regardless,
//! and on a hedged account two legs of equal size cancelled: the books
//! reported flat, the liquidation check ran against nothing, and any
//! strategy asking what it held was told zero — while the venue charged
//! margin on both.
//!
//! The failure has a shape that hides it. A hedged account is usually
//! *approximately* balanced, so the netted number is usually small, and
//! a small number reads as a small position rather than as two large
//! ones cancelling.

use oq_core::PositionMode;
use oq_live::books::Books;
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, Instrument, InstrumentId, Nanos, PriceTicks, QtyLots, Side, Stamp};

fn books(mode: PositionMode) -> Books {
    // The contract derived from the instrument, as the runner now does.
    // With the constant this replaces, twenty lots at this price came to
    // a hundred times their real notional and the adopted position was
    // liquidated on the spot — by the model, while the venue still held
    // it. That is how the two defects interact, and why neither could be
    // fixed alone.
    let instrument = Instrument::linear(2, 4);
    Books::new(
        InstrumentId::new(1),
        Contract::of(&instrument).expect("a representable tick"),
        TierTable::example_btcusdt(),
        Cash::from_units(10_000),
        mode,
    )
}

fn tick() -> oq_engine::Tick {
    oq_engine::Tick::trades_only(Stamp::new(3, 3), 6_837_400, 6_837_400, 6_837_400)
}

#[test]
fn a_hedged_accounts_two_legs_do_not_cancel() {
    let mut b = books(PositionMode::Hedge);
    b.adopt(Side::Buy, QtyLots(20), PriceTicks(6_837_492), Nanos(1));
    b.adopt(Side::Sell, QtyLots(20), PriceTicks(6_837_336), Nanos(2));

    let ctx = b.context(tick());
    assert_eq!(ctx.position, QtyLots(20), "the long is still there");
    // Signed, so the two are distinguishable even at equal size — which
    // is the case that netting turns into nothing.
    assert_eq!(ctx.short_position, QtyLots(-20), "and so is the short");
    assert_ne!(
        ctx.entry,
        PriceTicks(0),
        "each leg keeps its own basis, or there is nothing to take profit against"
    );
}

/// The net of a hedged account is its two legs summed, not subtracted.
///
/// These are the numbers off a live testnet account the first time
/// anything compared the two views: 160 lots long, 40 short, and the
/// venue's own net of 120. Subtracting a leg that is already signed
/// reported 200 — a drift of eighty on an account nothing was wrong
/// with, which then condemned a healthy stream every third check.
///
/// It hid because both callers were missing. `Books::reconcile` had no
/// call sites at all, and the fuzz tests that reach `net_position` run
/// a netting account, where the short leg is zero and subtracting it is
/// the same as adding it.
#[test]
fn a_hedged_accounts_net_is_its_two_legs_summed() {
    let mut b = books(PositionMode::Hedge);
    b.adopt(Side::Buy, QtyLots(160), PriceTicks(7_774_340), Nanos(1));
    b.adopt(Side::Sell, QtyLots(40), PriceTicks(7_745_390), Nanos(2));

    assert_eq!(
        b.net_position(),
        QtyLots(120),
        "160 long against 40 short is 120, not 200"
    );
    assert_eq!(
        b.legs(),
        (QtyLots(160), QtyLots(-40)),
        "the short leg is signed, which is what makes summing correct"
    );
    assert_eq!(
        b.reconcile(QtyLots(120), Nanos(3)),
        None,
        "and so it agrees with the venue that says the same thing"
    );
}

/// And a netting account's do, which is what the mode is for.
#[test]
fn a_netting_accounts_two_legs_cancel() {
    let mut b = books(PositionMode::OneWay);
    b.adopt(Side::Buy, QtyLots(20), PriceTicks(6_837_492), Nanos(1));
    b.adopt(Side::Sell, QtyLots(20), PriceTicks(6_837_336), Nanos(2));

    assert_eq!(
        b.context(tick()).position,
        QtyLots(0),
        "netted, because that is the mode"
    );
}

/// The contract comes from the instrument, not from a constant.
///
/// A hand-written scale stays plausible while being wrong: prices parse,
/// quantities parse, and every notional is off by a factor nothing
/// reports. Four decimal places of quantity and two of price make one
/// tick-lot worth a hundred cash units; the constant in use said ten
/// thousand.
#[test]
fn the_contract_is_derived_and_not_assumed() {
    assert_eq!(
        Contract::of(&Instrument::linear(2, 4))
            .expect("representable")
            .tick_cash,
        100,
    );
    // And a different deployment of the same symbol is a different
    // number, which is the whole reason it cannot be a constant.
    assert_eq!(
        Contract::of(&Instrument::linear(2, 3))
            .expect("representable")
            .tick_cash,
        1_000,
    );
}
