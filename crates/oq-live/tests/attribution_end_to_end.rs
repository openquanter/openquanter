//! From a live session to the report the project exists to produce.
//!
//! `WHY.md` compresses this project into one sentence — every cent
//! between a backtest and the live run, accounted for — and two pieces
//! were built separately: `oq_live::shadow` observes a session and
//! reports where it and the venue disagree, and
//! `oq_parity::attribution` decomposes a gap by cause. Nothing turned
//! one into the other. The shadow produced divergences; the
//! decomposition wanted evidence.
//!
//! These tests are the join, checked end to end: a session runs, the
//! venue behaves imperfectly, and a report comes out the far side with
//! the causes named and a residual that was earned rather than assumed.

use oq_live::shadow::{Shadow, submitted};
use oq_margin::{Contract, TierTable};
use oq_parity::attribution::{Component, attribute};
use oq_parity::manifest::RunManifest;
use oq_types::{
    Cash, Instrument, InstrumentId, Nanos, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp,
};

const SEC: i64 = 1_000_000_000;

fn shadow() -> Shadow {
    Shadow::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(100_000),
    )
}

/// One tick on one lot is one cash unit, so the report's figures can be
/// read without arithmetic.
fn instrument() -> Instrument {
    Instrument {
        price_scale: 0,
        qty_scale: 0,
        contract_size: 1,
        price_tick: 1,
        qty_step: 1,
        min_notional: Cash(0),
    }
}

fn tick(ns: i64, price: i64) -> oq_engine::Tick {
    oq_engine::Tick {
        stamp: Stamp::new(ns, ns),
        last: PriceTicks(price),
        high: PriceTicks(price),
        low: PriceTicks(price),
        bid: PriceTicks(price - 1),
        ask: PriceTicks(price + 1),
        volume: QtyLots(0),
    }
}

fn manifest() -> RunManifest {
    RunManifest::from_content("commit", b"session", b"config", "live-session")
}

/// Buy at market and let the model fill it.
fn buy(s: &mut Shadow, id: u64, ns: i64, price: i64) {
    s.on_tick(tick(ns, price));
    s.apply(&submitted(
        OrderId(id),
        Side::Buy,
        None,
        QtyLots(1),
        Offset::Open,
        Stamp::new(ns, ns),
    ));
    s.on_tick(tick(ns, price));
}

/// **The join.** A session where the venue filled worse than the model
/// expected produces a report naming slippage, rather than a number
/// somebody has to explain.
#[test]
fn a_session_where_the_venue_filled_worse_becomes_a_report_naming_slippage() {
    let mut s = shadow();
    buy(&mut s, 1, SEC, 6_000_000);
    // The venue filled ten ticks above the prevailing price.
    s.on_venue_fill(
        OrderId(1),
        Side::Buy,
        PriceTicks(6_000_010),
        QtyLots(1),
        Nanos(SEC),
    );
    s.finish(Nanos(10 * SEC));

    let evidence = s.evidence(Some((Cash(0), Cash(0))), Some((Cash(0), Cash(0))));
    assert_eq!(evidence.matched.len(), 1, "the pair must have been kept");

    // The model filled at the ask, one tick above the prevailing price;
    // the venue filled ten above it. So the account is nine worse off
    // than the model, and that nine is what the decomposition has to
    // account for — not the ten, which is the venue's distance from the
    // *market* rather than from the model.
    let report = attribute(manifest(), &instrument(), Cash(-9), Cash(0), &evidence);

    let by = |c: Component| {
        report
            .components
            .iter()
            .find(|(k, _)| *k == c)
            .and_then(|(_, v)| v.amount())
            .expect("measured")
    };
    assert_eq!(
        by(Component::Slippage),
        Cash(-10),
        "paid ten above the prevailing price: {}",
        report.render()
    );
    assert_eq!(
        by(Component::Latency),
        Cash(1),
        "and the model's own fill was a tick above it, which is the other \
         direction: {}",
        report.render()
    );
    assert_eq!(
        report.residual,
        Some(Cash(0)),
        "the two account for the whole gap: {}",
        report.render()
    );
}

/// A pair that agreed is still evidence. A decomposition that only saw
/// the divergent pairs would be attributing a gap against a subset of
/// the trades that caused it.
#[test]
fn a_fill_the_venue_and_the_model_agreed_on_is_still_carried() {
    let mut s = shadow();
    buy(&mut s, 1, SEC, 6_000_000);
    let model_price = s.divergences().first().map_or(6_000_001, |_| 6_000_001);
    s.on_venue_fill(
        OrderId(1),
        Side::Buy,
        PriceTicks(model_price),
        QtyLots(1),
        Nanos(SEC),
    );
    s.finish(Nanos(10 * SEC));

    let evidence = s.evidence(Some((Cash(0), Cash(0))), Some((Cash(0), Cash(0))));
    assert_eq!(
        evidence.matched.len(),
        1,
        "an agreeing pair is evidence of zero slippage, not an absence of evidence"
    );
    assert!(
        s.divergences().is_empty(),
        "and it produced no divergence: {:?}",
        s.divergences()
    );
}

/// A fill the model made and the venue never did becomes a queue
/// position component — the model claimed edge the account never had.
#[test]
fn a_fill_the_venue_never_made_becomes_a_queue_component() {
    let mut s = shadow();
    buy(&mut s, 1, SEC, 6_000_000);
    // The venue says nothing, ever.
    s.finish(Nanos(60 * SEC));

    let evidence = s.evidence(Some((Cash(0), Cash(0))), Some((Cash(0), Cash(0))));
    assert_eq!(evidence.unmatched.len(), 1);
    assert!(
        !evidence.unmatched[0].at_venue,
        "this one was the model's, not the venue's"
    );

    let report = attribute(manifest(), &instrument(), Cash(0), Cash(0), &evidence);
    let queue = report
        .components
        .iter()
        .find(|(c, _)| *c == Component::QueuePosition)
        .and_then(|(_, v)| v.amount())
        .expect("measured");
    let _ = queue;
    assert!(
        report.render().contains("(modelled)"),
        "and it is marked as a model rather than a measurement: {}",
        report.render()
    );
}

/// FR-ATTRIB-6, through the whole chain. A shadow does not see funding
/// or fees, so it takes them as arguments; passing `None` says nobody
/// looked, and the report must decline to produce a residual rather
/// than reporting a gap explained by causes nobody measured.
#[test]
fn a_session_with_no_fee_statement_produces_no_residual() {
    let mut s = shadow();
    buy(&mut s, 1, SEC, 6_000_000);
    s.on_venue_fill(
        OrderId(1),
        Side::Buy,
        PriceTicks(6_000_010),
        QtyLots(1),
        Nanos(SEC),
    );
    s.finish(Nanos(10 * SEC));

    let report = attribute(
        manifest(),
        &instrument(),
        Cash(-10),
        Cash(0),
        // Nobody read the fee statement.
        &s.evidence(Some((Cash(0), Cash(0))), None),
    );

    assert_eq!(report.residual, None);
    assert!(!report.is_complete());
    assert_eq!(report.unavailable().len(), 1);
    assert!(
        report.render().contains("NO RESIDUAL"),
        "{}",
        report.render()
    );
}

/// Evidence taken before the grace period elapses has a hole in it:
/// fills still waiting are neither matched nor reported unmatched. The
/// method documents that; this is what makes it true.
#[test]
fn evidence_taken_before_finishing_is_incomplete_and_finishing_completes_it() {
    let mut s = shadow().with_grace(Nanos(60 * SEC));
    buy(&mut s, 1, SEC, 6_000_000);

    let early = s.evidence(None, None);
    assert!(
        early.matched.is_empty() && early.unmatched.is_empty(),
        "the fill is still inside the grace period"
    );

    s.finish(Nanos(2 * SEC));
    let late = s.evidence(None, None);
    assert_eq!(late.unmatched.len(), 1, "and finishing flushes it");
}
