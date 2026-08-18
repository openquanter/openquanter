//! The live books, driven through every way a venue can misbehave.
//!
//! M3's scope asks for `oq-sim` at full strength: "the entire scenario
//! catalogue plus gateway fuzzing (disconnects, reordering,
//! duplication, partial fills)". The catalogue existed and nothing was
//! driven through it — `distort` could damage a list of events and no
//! component was on the other end of one.
//!
//! # These assert invariants, not numbers
//!
//! A fuzz test that pins an output is a golden test with extra steps: it
//! fails when the fixture changes and passes when the property breaks.
//! What must hold under a misbehaving venue is not a particular
//! position, it is that:
//!
//! - a trade booked twice moves the account once
//! - reordering reports does not change where the account ends up,
//!   because the account is the sum of its trades and addition does not
//!   care about order
//! - a report that never arrives leaves the books *disagreeing* with the
//!   venue, and the reconciler must say so rather than the books
//!   silently being right by luck
//! - nothing the venue can send makes the books invent a fill
//!
//! # Why this found something
//!
//! The first run of the duplication scenario doubled a position. The
//! books had no deduplication — `oq-live`'s order tracker had it, and
//! the kernel-backed books added later did not, which is exactly the
//! kind of gap that opens when one concern is implemented in two places
//! at two times. The fix is in `books.rs`; this is what makes it stay
//! fixed.

use oq_live::books::{Booked, Books};
use oq_margin::{Contract, TierTable};
use oq_sim::{Fault, corpus, distort};
use oq_types::{
    Cash, Fill, InstrumentId, Liquidity, Nanos, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp,
    TradeId,
};

const SEC: i64 = 1_000_000_000;

fn books() -> Books {
    Books::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(1_000_000),
    )
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

/// A session's worth of fills, alternating opens and closes so the
/// position moves both ways rather than only growing.
fn session(n: u64) -> Vec<Fill> {
    (0..n)
        .map(|i| Fill {
            stamp: Stamp::new((i as i64 + 1) * SEC, (i as i64 + 1) * SEC),
            instrument: InstrumentId::new(1),
            order: OrderId(i + 1),
            trade: TradeId(i + 1),
            side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
            offset: if i % 2 == 0 {
                Offset::Open
            } else {
                Offset::Close
            },
            price: PriceTicks(6_000_000 + (i as i64 % 7) * 500),
            qty: QtyLots(2),
            liquidity: Liquidity::Taker,
        })
        .collect()
}

/// Book a stream of reports and return where the account ended up.
fn play(fills: &[Fill]) -> (QtyLots, Cash, usize) {
    let mut b = books();
    b.on_tick(&tick(0, 6_000_000));
    for f in fills {
        b.on_venue_fill(f);
    }
    // A common mark at the end, so two runs are compared at one price
    // rather than at whatever each happened to stop on.
    b.on_tick(&tick(1_000 * SEC, 6_000_000));
    (b.net_position(), b.equity(), b.booked())
}

/// The whole catalogue, against the invariant that survives all of it:
/// nothing a venue can send makes the books invent a trade.
#[test]
fn no_scenario_in_the_catalogue_makes_the_books_invent_a_trade() {
    let clean = session(40);
    for scenario in corpus() {
        let distorted = distort(&scenario, &clean);
        let (_, _, booked) = play(&distorted);
        assert!(
            booked <= clean.len(),
            "{}: booked {booked} distinct trades from a session of {}",
            scenario.name,
            clean.len()
        );
    }
}

/// A reconnecting stream repeats what it already said. This is the
/// scenario that found the defect: the books had no deduplication, and
/// a redelivered fill doubled the position.
#[test]
fn duplication_moves_the_account_once_per_trade() {
    let clean = session(30);
    let (position, equity, booked) = play(&clean);

    let storm = corpus()
        .into_iter()
        .find(|s| s.faults.iter().all(|f| *f == Fault::Duplicate))
        .expect("the catalogue has a redelivery scenario");
    let repeated = distort(&storm, &clean);
    assert!(
        repeated.len() > clean.len(),
        "the fixture must actually repeat something"
    );

    let (p, e, b) = play(&repeated);
    assert_eq!(p, position, "position moved on a redelivery");
    assert_eq!(e, equity, "equity moved on a redelivery");
    assert_eq!(b, booked, "a repeated trade was counted twice");
}

/// The account is the sum of its trades, and addition does not care
/// about order. Reports interleaved by arrival rather than by the venue
/// must land in the same place.
#[test]
fn reordering_reports_does_not_change_where_the_account_ends_up() {
    let clean = session(30);
    let (position, _, booked) = play(&clean);

    let shuffled = corpus()
        .into_iter()
        .find(|s| s.name == "two-sockets-one-account")
        .expect("the catalogue has a reordering scenario");
    let out_of_order = distort(&shuffled, &clean);

    let (p, _, b) = play(&out_of_order);
    assert_eq!(b, booked, "the same trades, in a different order");
    assert_eq!(
        p, position,
        "the position depends on which trades happened, not on the order they were told in"
    );
}

/// A report that never arrives leaves the books wrong, and the point is
/// that they are wrong *visibly*. Books that happened to agree anyway
/// would be right by luck, and the reconciler is what turns the
/// difference into a fact somebody acts on.
#[test]
fn a_dropped_report_leaves_a_disagreement_the_reconciler_names() {
    let clean = session(30);
    let (truth, _, _) = play(&clean);

    let lost = corpus()
        .into_iter()
        .find(|s| s.name == "lost-cancel")
        .expect("the catalogue has a drop scenario");
    let with_holes = distort(&lost, &clean);
    assert!(
        with_holes.len() < clean.len(),
        "the fixture must actually drop something"
    );

    let mut b = books();
    b.on_tick(&tick(0, 6_000_000));
    for f in &with_holes {
        b.on_venue_fill(f);
    }
    b.on_tick(&tick(1_000 * SEC, 6_000_000));

    // The venue knows the truth; these books do not.
    let mismatch = b.reconcile(truth, Nanos(1_000 * SEC));
    if b.net_position() == truth {
        // Possible: the dropped reports may have cancelled out. Then
        // there is nothing to report, and reporting one would be worse.
        assert_eq!(mismatch, None);
    } else {
        let m = mismatch.expect("the books differ and must say so");
        assert_eq!(m.ours, b.net_position());
        assert_eq!(m.theirs, truth);
        assert_ne!(m.drift(), QtyLots(0));
    }
}

/// A fill with no trade id cannot be deduplicated, so accepting it
/// means accepting an unbounded number of copies of one trade. Refused
/// rather than applied — and a position too large because of a
/// redelivery looks exactly like one too large because of a bug.
#[test]
fn a_report_with_no_trade_id_is_refused_however_many_times_it_arrives() {
    let mut b = books();
    b.on_tick(&tick(0, 6_000_000));
    let anonymous = Fill {
        trade: TradeId(0),
        ..session(1)[0]
    };
    for _ in 0..50 {
        assert_eq!(b.on_venue_fill(&anonymous), Booked::Unidentifiable);
    }
    assert_eq!(b.net_position(), QtyLots(0));
    assert_eq!(b.booked(), 0);
}

/// Partial fills of one order are separate trades and must all be
/// booked. Deduplicating by order id rather than trade id would keep
/// the first and discard the rest, leaving a position short of what the
/// account actually holds — the opposite error to the duplication one,
/// and the one that is quieter.
#[test]
fn partial_fills_of_one_order_all_count() {
    let mut b = books();
    b.on_tick(&tick(0, 6_000_000));
    let base = session(1)[0];
    for part in 0..5u64 {
        let f = Fill {
            trade: TradeId(1_000 + part),
            qty: QtyLots(2),
            ..base
        };
        assert!(matches!(b.on_venue_fill(&f), Booked::Applied(_)));
    }
    assert_eq!(b.net_position(), QtyLots(10), "five parts of two lots each");
    assert_eq!(b.booked(), 5);
}

/// Every scenario, run twice, must land in the same place. A fault
/// injector that is not deterministic makes a failure unreproducible,
/// and an unreproducible failure in a fuzz suite is noise.
#[test]
fn the_catalogue_is_deterministic() {
    let clean = session(25);
    for scenario in corpus() {
        let a = play(&distort(&scenario, &clean));
        let b = play(&distort(&scenario, &clean));
        assert_eq!(a, b, "{} is not reproducible", scenario.name);
    }
}
