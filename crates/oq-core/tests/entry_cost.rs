//! Independent cash-flow checks for blended entries and partial closes.

use oq_core::kernel::Matching;
use oq_core::{Event, Kernel, PositionMode, State};
use oq_margin::{Contract, TierTable};
use oq_types::{
    Cash, Fee, Fill, InstrumentId, Liquidity, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp,
    TradeId,
};

fn state(mode: PositionMode, tick_cash: i64) -> State {
    let mut state = State::new(
        InstrumentId::new(1),
        Contract::new(tick_cash),
        TierTable::example_btcusdt(),
        Cash::from_units(100),
    );
    state.mode = mode;
    state.matching = Matching::Venue;
    state.without_liquidation()
}

fn kernel(mode: PositionMode, tick_cash: i64) -> Kernel {
    Kernel::new(state(mode, tick_cash))
}

fn execution(id: u64, side: Side, offset: Offset, qty: i64, price: i64) -> Event {
    Event::VenueFill {
        fill: Fill {
            stamp: Stamp::new(id as i64, id as i64),
            instrument: InstrumentId::new(1),
            order: OrderId(id),
            trade: TradeId(id),
            side,
            offset,
            price: PriceTicks(price),
            qty: QtyLots(qty),
            liquidity: Liquidity::Maker,
        },
        fee: Fee::Reported(Cash::ZERO),
    }
}

fn sides(sign: i64) -> (Side, Side) {
    if sign > 0 {
        (Side::Buy, Side::Sell)
    } else {
        (Side::Sell, Side::Buy)
    }
}

#[test]
fn flattening_realizes_cash_flow_not_rounded_entry() {
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        for sign in [1, -1] {
            let (open, close) = sides(sign);
            let mut k = kernel(mode, 100_000_000);
            for e in [
                execution(1, open, Offset::Open, 1, 100),
                execution(2, open, Offset::Open, 2, 101),
            ] {
                k.apply(&e);
            }
            let h = k.state().holding();
            assert_eq!(h.entry_cost() + h.short_entry_cost(), 302 * 100_000_000);
            k.apply(&execution(3, close, Offset::Close, 3, 102));
            assert!(k.state().is_flat());
            assert_eq!(k.state().realized, Cash(sign * 4 * 100_000_000));
            assert_eq!(k.state().holding().entry_cost(), 0);
            assert_eq!(k.state().holding().short_entry_cost(), 0);
        }
    }
}

#[test]
fn partial_closes_carry_cash_rounding_to_final_flattening() {
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        for sign in [1, -1] {
            let (open, close) = sides(sign);
            for (exit, tick_cash) in [99, 100, 101, 102]
                .into_iter()
                .flat_map(|exit| [1, 100_000_000].map(|scale| (exit, scale)))
            {
                let mut k = kernel(mode, tick_cash);
                k.apply(&execution(1, open, Offset::Open, 1, 100));
                k.apply(&execution(2, open, Offset::Open, 2, 101));
                k.apply(&execution(3, close, Offset::Close, 1, exit));
                // Reference uses the unrounded rational basis, truncating
                // the final profit at the cash boundary, not its terms.
                assert_eq!(
                    k.state().realized,
                    Cash(sign * (exit * 3 - 302) * tick_cash / 3)
                );
                k.apply(&execution(4, close, Offset::Close, 2, exit));
                assert_eq!(
                    k.state().realized,
                    Cash(sign * (exit * 3 - 302) * tick_cash)
                );
                assert!(k.state().is_flat());
            }
        }
    }
}

#[test]
fn adding_after_partial_close_preserves_full_cash_conservation() {
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        for sign in [1, -1] {
            let (open, close) = sides(sign);
            for q1 in 1..=4 {
                for q2 in 1..=4 {
                    for p1 in 100..=103 {
                        for p2 in 100..=103 {
                            let mut k = kernel(mode, 17);
                            let starting = k.state().balance();
                            for e in [
                                execution(1, open, Offset::Open, q1, p1),
                                execution(2, open, Offset::Open, q2, p2),
                                execution(3, close, Offset::Close, 1, 104),
                                execution(4, open, Offset::Open, 2, 102),
                                execution(5, close, Offset::Close, q1 + q2 + 1, 99),
                            ] {
                                k.apply(&e);
                            }
                            let proceeds = 104 + (q1 + q2 + 1) * 99;
                            let costs = q1 * p1 + q2 * p2 + 2 * 102;
                            let expected = Cash(sign * (proceeds - costs) * 17);
                            assert_eq!(
                                k.state().realized,
                                expected,
                                "{mode:?} sign={sign} q={q1}/{q2} p={p1}/{p2}"
                            );
                            assert_eq!(k.state().balance(), starting.add(expected));
                            assert!(k.state().is_flat());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn crossing_flat_starts_a_fresh_opposite_cost() {
    let mut k = kernel(PositionMode::OneWay, 1);
    k.apply(&execution(1, Side::Buy, Offset::Open, 1, 100));
    k.apply(&execution(2, Side::Buy, Offset::Open, 2, 101));
    k.apply(&execution(3, Side::Sell, Offset::Open, 5, 102));
    assert_eq!(k.state().realized, Cash(4));
    assert_eq!(k.state().holding().qty, QtyLots(-2));
    assert_eq!(k.state().holding().entry_cost(), 204);
    k.apply(&execution(4, Side::Buy, Offset::Close, 2, 99));
    assert_eq!(k.state().realized, Cash(10));
    assert!(k.state().is_flat());
}

#[test]
fn mark_equity_and_margin_view_use_precise_cost() {
    let mut k = kernel(PositionMode::OneWay, 1);
    k.apply(&execution(1, Side::Buy, Offset::Open, 1, 100));
    k.apply(&execution(2, Side::Buy, Offset::Open, 2, 101));
    let p = k.state().position();
    assert_eq!(p.entry, PriceTicks(100), "display still uses price ticks");
    assert_eq!(p.entry_cost, Some(302));
    assert_eq!(
        p.unrealized(PriceTicks(101)),
        Cash(1),
        "not the three ticks from rounded entry"
    );
}

#[test]
fn serialized_replay_preserves_remaining_cost_and_future_outputs() {
    use oq_core::{MemorySink, Sequencer};
    let mut recorded = Sequencer::with_sink(state(PositionMode::Hedge, 17), MemorySink::new());
    let mut expected = Vec::new();
    for e in [
        execution(1, Side::Buy, Offset::Open, 1, 100),
        execution(2, Side::Buy, Offset::Open, 2, 101),
        execution(3, Side::Sell, Offset::Close, 1, 102),
    ] {
        expected.extend_from_slice(recorded.submit(&e).unwrap());
    }
    let mut replay = kernel(PositionMode::Hedge, 17);
    let mut actual = Vec::new();
    for (kind, bytes) in recorded.sink().records() {
        let event = Event::decode(*kind, bytes).expect("strict event round trip");
        actual.extend_from_slice(replay.apply(&event));
    }
    assert_eq!(actual, expected);
    assert_eq!(replay.fingerprint(), recorded.kernel().fingerprint());
    for e in [
        execution(4, Side::Buy, Offset::Open, 2, 103),
        execution(5, Side::Sell, Offset::Close, 4, 105),
    ] {
        assert_eq!(replay.apply(&e), recorded.submit(&e).unwrap());
    }
    assert_eq!(replay.fingerprint(), recorded.kernel().fingerprint());
    assert!(replay.state().is_flat());
}

#[test]
fn cost_precision_changes_liquidation_boundary_in_both_directions() {
    use oq_margin::{MarginTier, MarginedPosition};
    use oq_types::Ratio;
    let table = TierTable::new(vec![MarginTier {
        max_notional: Cash(i64::MAX),
        rate: Ratio::from_ppm(8_000),
        amount: Cash::ZERO,
    }])
    .unwrap();
    let long = MarginedPosition::new(Contract::new(1), PriceTicks(100), QtyLots(3), Cash::ZERO)
        .with_entry_cost(302);
    assert_eq!(
        long.liquidation_price_at(&table, PriceTicks(101)),
        Some(PriceTicks(101))
    );
    assert!(long.is_liquidatable(&table, PriceTicks(101)));
    assert!(!long.is_liquidatable(&table, PriceTicks(102)));
    let short = MarginedPosition::new(Contract::new(1), PriceTicks(100), QtyLots(-3), Cash(4))
        .with_entry_cost(302);
    assert_eq!(
        short.liquidation_price_at(&table, PriceTicks(102)),
        Some(PriceTicks(102))
    );
    assert!(short.is_liquidatable(&table, PriceTicks(102)));
    assert!(!short.is_liquidatable(&table, PriceTicks(101)));
}
