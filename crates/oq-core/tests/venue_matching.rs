//! One kernel, two sources of fills.
//!
//! `IMPLEMENTATION` §1 says backtest and live differ only in the event
//! producer. `Matching` is what makes that true rather than
//! aspirational: the accounting, the margin, the funding and the state
//! are one implementation, and only the decision about which orders
//! trade moves.
//!
//! The important test is the first one — the same trade, booked by the
//! matcher and booked by the venue, must leave the account in the same
//! place. If it did not, "the same engine" would be a figure of speech.

use oq_core::kernel::Matching;
use oq_core::{Event, Kernel, Output, RejectReason, State};
use oq_engine::Tick;
use oq_margin::{Contract, TierTable};
use oq_types::{
    Cash, Fill, InstrumentId, Liquidity, Nanos, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp,
    TradeId,
};

const SEC: i64 = 1_000_000_000;

fn stamp(ns: i64) -> Stamp {
    Stamp::new(ns, ns)
}

fn tick(ns: i64, price: i64) -> Tick {
    Tick {
        stamp: stamp(ns),
        last: PriceTicks(price),
        high: PriceTicks(price),
        low: PriceTicks(price),
        bid: PriceTicks(price - 1),
        ask: PriceTicks(price + 1),
        volume: QtyLots(0),
    }
}

fn state(matching: Matching) -> State {
    let mut s = State::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(100_000),
    );
    s.matching = matching;
    s
}

fn venue_fill(ns: i64, order: u64, price: i64, qty: i64) -> Fill {
    Fill {
        stamp: stamp(ns),
        instrument: InstrumentId::new(1),
        order: OrderId(order),
        trade: TradeId(1),
        side: Side::Buy,
        offset: Offset::Open,
        price: PriceTicks(price),
        qty: QtyLots(qty),
        liquidity: Liquidity::Taker,
    }
}

fn submit(id: u64, ns: i64) -> Event {
    Event::Submit {
        instrument: None,
        id: OrderId(id),
        side: Side::Buy,
        price: None,
        qty: QtyLots(3),
        offset: Offset::Open,
        stamp: stamp(ns),
    }
}

/// **The claim.** The same trade, booked by the matcher and booked by
/// the venue, leaves the account in the same place.
#[test]
fn a_matched_fill_and_a_venue_fill_leave_the_same_account() {
    let mut sim = Kernel::new(state(Matching::Simulated));
    sim.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });
    sim.apply(&submit(1, SEC));
    sim.apply(&Event::Tick {
        instrument: None,
        tick: tick(2 * SEC, 6_000_000),
    });
    let after_sim = sim.summary();
    assert!(!after_sim.qty.is_zero(), "the fixture must actually fill");

    let mut live = Kernel::new(state(Matching::Venue));
    live.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });
    live.apply(&submit(1, SEC));
    live.apply(&Event::VenueFill(venue_fill(
        2 * SEC,
        1,
        after_sim.entry.0,
        after_sim.qty.0,
    )));
    live.apply(&Event::Tick {
        instrument: None,
        tick: tick(2 * SEC, 6_000_000),
    });
    let after_live = live.summary();

    assert_eq!(after_live.qty, after_sim.qty, "position");
    assert_eq!(after_live.entry, after_sim.entry, "average entry");
    assert_eq!(after_live.fees, after_sim.fees, "fees");
    assert_eq!(after_live.balance, after_sim.balance, "balance");
    assert_eq!(after_live.equity, after_sim.equity, "equity");
}

/// Under venue matching the matcher must never fill. A kernel that both
/// matched and accepted venue fills would book every trade twice, and
/// the second copy would look exactly like the first.
#[test]
fn the_matcher_does_not_fill_when_the_venue_is_matching() {
    let mut k = Kernel::new(state(Matching::Venue));
    k.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });
    k.apply(&submit(1, SEC));
    for i in 2..10 {
        let outputs = k.apply(&Event::Tick {
            instrument: None,
            tick: tick(i * SEC, 6_000_000 + i * 100),
        });
        assert!(
            !outputs.iter().any(|o| matches!(o, Output::Filled(_))),
            "the matcher filled at tick {i}, and the venue had not said so"
        );
    }
    assert!(k.summary().qty.is_zero(), "and nothing was booked");
}

/// And the reverse. A simulated run produces its own fills; one
/// arriving from outside is a second matcher.
#[test]
fn a_venue_fill_is_refused_by_a_kernel_that_is_matching_for_itself() {
    let mut k = Kernel::new(state(Matching::Simulated));
    k.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });
    let outputs = k.apply(&Event::VenueFill(venue_fill(SEC, 1, 6_000_000, 3)));
    assert!(
        outputs.iter().any(|o| matches!(
            o,
            Output::Rejected {
                reason: RejectReason::NotVenueMatched,
                ..
            }
        )),
        "expected a refusal, got {outputs:?}"
    );
    assert!(k.summary().qty.is_zero(), "and nothing was booked");
}

/// The failure this guards against is a *replay*, not a live run. A
/// journal carrying both the submit and the venue's fill, replayed by a
/// build whose mode was not set, would rest the order and match it too.
#[test]
fn a_filled_order_leaves_the_book_so_a_replay_cannot_match_it_again() {
    let mut k = Kernel::new(state(Matching::Venue));
    k.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });
    k.apply(&Event::Submit {
        instrument: None,
        id: OrderId(7),
        side: Side::Buy,
        price: Some(PriceTicks(5_900_000)),
        qty: QtyLots(3),
        offset: Offset::Open,
        stamp: stamp(SEC),
    });
    k.apply(&Event::VenueFill(venue_fill(2 * SEC, 7, 5_900_000, 3)));
    let after = k.summary().qty;

    for i in 3..8 {
        k.apply(&Event::Tick {
            instrument: None,
            tick: tick(i * SEC, 5_800_000),
        });
    }
    assert_eq!(k.summary().qty, after, "the position must not have moved");
}

/// A venue fill can be what makes the account liquidatable, and waiting
/// for the next tick would report it at a price that never caused it.
#[test]
fn a_venue_fill_that_breaches_maintenance_is_noticed_at_once() {
    let mut s = state(Matching::Venue);
    // Overwrite by crediting the difference: the balance is what the
    // account holds in its settlement currency, not a field to poke.
    s.credit(Cash::from_units(50).sub(s.balance()));
    let mut k = Kernel::new(s);
    k.apply(&Event::Tick {
        instrument: None,
        tick: tick(SEC, 6_000_000),
    });

    let outputs = k
        .apply(&Event::VenueFill(venue_fill(2 * SEC, 1, 6_000_000, 10_000)))
        .to_vec();
    assert!(
        outputs
            .iter()
            .any(|o| matches!(o, Output::Liquidated { .. })),
        "expected a liquidation in {outputs:?}"
    );
}

/// The journal is what a replay reads, so the record has to survive it
/// exactly. Anything less and a live run's replay is about a different
/// trade.
#[test]
fn a_venue_fill_survives_the_journal() {
    let original = Event::VenueFill(Fill {
        liquidity: Liquidity::Maker,
        offset: Offset::Close,
        side: Side::Sell,
        ..venue_fill(1_700_000_000_000_000_000, 42, 6_123_456, 17)
    });
    let bytes = original.encode();
    let back = Event::decode(original.kind(), &bytes).expect("decodable");
    assert_eq!(back, original);

    assert_eq!(
        Event::decode(original.kind(), &bytes[..bytes.len() - 1]),
        None,
        "a truncated record must not read as a valid shorter one"
    );
    assert_eq!(Event::decode(original.kind(), &[]), None);
}

/// Ordered by when the trade happened, not by when this process heard
/// about it. The local receive time is a property of the link.
#[test]
fn a_venue_fill_is_ordered_by_the_venues_clock() {
    let e = Event::VenueFill(Fill {
        stamp: Stamp::new(1_000, 9_999),
        ..venue_fill(0, 1, 1, 1)
    });
    assert_eq!(e.at(), Nanos(1_000));
}
