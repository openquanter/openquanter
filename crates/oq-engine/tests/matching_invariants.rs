//! The matching invariants, over generated inputs.
//!
//! `FR-MATCH-7` names four: quantity conservation, price-time priority,
//! no crossed book, no negative fills. It also says matching is
//! "expressed as pure functions covered by property tests" — the purity
//! is what makes this possible, since a matcher that read a clock could
//! not be replayed against a generated price path at all.
//!
//! # Why these matter more here than elsewhere
//!
//! L0 is frozen as the migration and regression anchor. Everything
//! measured against it — parity baselines, the L1 comparison, the
//! attribution report's model side — is measured in units L0 defines. A
//! defect here does not produce a wrong number in one place; it moves
//! the origin.
//!
//! # No output is pinned
//!
//! What is asserted is a relationship that holds for every input. A
//! failure arrives as a counterexample proptest has shrunk to its
//! smallest form, which is the bug report rather than a hint toward one.

use oq_engine::l0::L0Engine;
use oq_engine::tick::Tick;
use oq_types::{InstrumentId, PriceTicks, QtyLots, Side, Stamp};
use proptest::prelude::*;

/// One instruction to the engine.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Rest a limit order.
    Limit {
        id: u64,
        side: Side,
        price: i64,
        qty: i64,
    },
    /// Send a market order.
    Market { id: u64, side: Side, qty: i64 },
    /// Cancel by id.
    Cancel { id: u64 },
    /// Advance the market.
    Observe { last: i64, spread: i64 },
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

/// Prices in a band the engine can cross in either direction, so orders
/// actually fill rather than the whole run resting untouched.
fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (1u64..40, side(), 5_900_000i64..6_100_000, 1i64..50).prop_map(|(id, side, price, qty)| {
            Step::Limit {
                id,
                side,
                price,
                qty,
            }
        }),
        (1u64..40, side(), 1i64..50).prop_map(|(id, side, qty)| Step::Market { id, side, qty }),
        (1u64..40).prop_map(|id| Step::Cancel { id }),
        (5_900_000i64..6_100_000, 1i64..200)
            .prop_map(|(last, spread)| Step::Observe { last, spread }),
    ]
}

/// What a run produced, with everything the invariants are about.
struct Run {
    /// Every fill, in order.
    fills: Vec<oq_types::Fill>,
    /// Quantity submitted per order id.
    submitted: std::collections::HashMap<u64, i64>,
    /// Book extremes observed after every observation.
    crossings: Vec<(PriceTicks, PriceTicks)>,
}

/// Drive the engine through a script.
fn play(steps: &[Step]) -> Run {
    let mut e = L0Engine::new(InstrumentId::new(1));
    let mut fills = Vec::new();
    let mut submitted: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
    let mut crossings = Vec::new();
    let mut now = 0i64;

    for step in steps {
        now += 1_000_000_000;
        let stamp = Stamp::new(now, now);
        match *step {
            Step::Limit {
                id,
                side,
                price,
                qty,
            } => {
                // An id already working is refused by the book, so only
                // count what the engine accepted.
                if !e.book().contains(oq_types::OrderId(id)) {
                    e.submit_limit(
                        oq_types::OrderId(id),
                        side,
                        PriceTicks(price),
                        QtyLots(qty),
                        stamp,
                    );
                    *submitted.entry(id).or_default() += qty;
                }
            }
            Step::Market { id, side, qty } => {
                if !e.book().contains(oq_types::OrderId(id)) {
                    e.submit_market(oq_types::OrderId(id), side, QtyLots(qty), stamp);
                    *submitted.entry(id).or_default() += qty;
                }
            }
            Step::Cancel { id } => {
                e.cancel(oq_types::OrderId(id));
            }
            Step::Observe { last, spread } => {
                let tick = Tick {
                    stamp,
                    last: PriceTicks(last),
                    high: PriceTicks(last + spread),
                    low: PriceTicks(last - spread),
                    bid: PriceTicks(last - spread),
                    ask: PriceTicks(last + spread),
                    volume: QtyLots(0),
                };
                fills.extend(e.on_tick(&tick).iter().map(|f| f.fill));
                if let (Some(b), Some(a)) = (e.book().best_bid(), e.book().best_ask()) {
                    crossings.push((b, a));
                }
            }
        }
    }
    Run {
        fills,
        submitted,
        crossings,
    }
}

proptest! {
    /// **Quantity conservation.** No order fills for more than it asked
    /// for, whatever the price path does.
    ///
    /// Over-filling is the defect that manufactures a position out of
    /// nothing, and it is invisible in a P&L that happens to be
    /// profitable.
    #[test]
    fn no_order_fills_for_more_than_it_asked(steps in prop::collection::vec(step(), 1..60)) {
        let run = play(&steps);
        let mut filled: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
        for f in &run.fills {
            *filled.entry(f.order.0).or_default() += f.qty.0;
        }
        for (id, got) in filled {
            let asked = run.submitted.get(&id).copied().unwrap_or(0);
            prop_assert!(
                got <= asked,
                "order {id} asked for {asked} and filled {got}"
            );
        }
    }

    /// **No negative fills.** Every fill has a positive quantity and a
    /// positive price.
    ///
    /// A zero-priced fill is not hypothetical: the predecessor produced
    /// them, an entire position ladder computed against nonsense, and
    /// nothing crashed. That is the first wall `WHY.md` describes.
    #[test]
    fn every_fill_has_a_positive_price_and_quantity(
        steps in prop::collection::vec(step(), 1..60),
    ) {
        let run = play(&steps);
        for f in &run.fills {
            prop_assert!(f.qty.0 > 0, "a fill of {} lots", f.qty.0);
            prop_assert!(f.price.0 > 0, "a fill at {}", f.price.0);
        }
    }

    /// **No crossed book.** Among resting orders the best bid never
    /// reaches the best ask.
    ///
    /// A crossed book is two orders that should have traded with each
    /// other and did not, so every fill after it is against a market
    /// that could not have existed.
    #[test]
    fn the_resting_book_is_never_crossed(steps in prop::collection::vec(step(), 1..60)) {
        let run = play(&steps);
        for (bid, ask) in run.crossings {
            prop_assert!(
                bid.0 < ask.0,
                "the book is crossed: bid {} at or above ask {}",
                bid.0,
                ask.0
            );
        }
    }

    /// **Price-time priority.** Two orders resting at the same price on
    /// the same side fill in the order they arrived.
    ///
    /// Without it a backtest silently rewards whichever order the data
    /// structure happened to reach first, which is a property of the
    /// implementation rather than of the market.
    #[test]
    fn same_price_orders_fill_in_arrival_order(
        price in 5_950_000i64..6_050_000,
        n in 2usize..6,
        qty in 1i64..20,
    ) {
        let mut e = L0Engine::new(InstrumentId::new(1));
        for i in 0..n {
            let now = (i as i64 + 1) * 1_000_000_000;
            e.submit_limit(
                oq_types::OrderId(i as u64 + 1),
                Side::Buy,
                PriceTicks(price),
                QtyLots(qty),
                Stamp::new(now, now),
            );
        }
        // A price path that reaches every one of them.
        let now = 100_000_000_000i64;
        let tick = Tick {
            stamp: Stamp::new(now, now),
            last: PriceTicks(price - 1_000),
            high: PriceTicks(price + 1_000),
            low: PriceTicks(price - 1_000),
            bid: PriceTicks(price - 1_001),
            ask: PriceTicks(price - 999),
            volume: QtyLots(0),
        };
        let order: Vec<u64> = e.on_tick(&tick).iter().map(|f| f.fill.order.0).collect();
        let mut expected: Vec<u64> = order.clone();
        expected.sort_unstable();
        prop_assert_eq!(
            order,
            expected,
            "orders at one price filled out of arrival order"
        );
    }

    /// A cancelled order never fills afterwards.
    ///
    /// Not in FR-MATCH-7's list and it belongs with them: a fill on a
    /// withdrawn order is a position the strategy believes it closed.
    #[test]
    fn a_cancelled_order_never_fills(
        price in 5_950_000i64..6_050_000,
        qty in 1i64..20,
        ticks in 1usize..10,
    ) {
        let mut e = L0Engine::new(InstrumentId::new(1));
        e.submit_limit(
            oq_types::OrderId(1),
            Side::Buy,
            PriceTicks(price),
            QtyLots(qty),
            Stamp::new(0, 0),
        );
        prop_assert!(e.cancel(oq_types::OrderId(1)));

        for i in 0..ticks {
            let now = (i as i64 + 1) * 1_000_000_000;
            let tick = Tick {
                stamp: Stamp::new(now, now),
                last: PriceTicks(price - 5_000),
                high: PriceTicks(price + 5_000),
                low: PriceTicks(price - 5_000),
                bid: PriceTicks(price - 5_001),
                ask: PriceTicks(price - 4_999),
                volume: QtyLots(0),
            };
            prop_assert!(
                e.on_tick(&tick).is_empty(),
                "a cancelled order filled at observation {i}"
            );
        }
    }

    /// The same script twice produces the same fills.
    ///
    /// Determinism is what makes replay and parity mean anything, and it
    /// is asserted over generated scripts rather than the handful
    /// somebody wrote.
    #[test]
    fn the_same_script_produces_the_same_fills(
        steps in prop::collection::vec(step(), 1..60),
    ) {
        let a = play(&steps);
        let b = play(&steps);
        prop_assert_eq!(a.fills.len(), b.fills.len());
        for (x, y) in a.fills.iter().zip(&b.fills) {
            prop_assert_eq!(x, y);
        }
    }
}
