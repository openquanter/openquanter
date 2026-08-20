//! L2: the queue measured from the venue's own book.
//!
//! `FR-MATCH-6`. L1's module documentation states the limit this tier
//! exists to remove:
//!
//! > the tick format carries a price path and a cumulative volume. It
//! > does not carry book depth [...] So L1 cannot *measure* queue-ahead
//! > or latency; it applies a policy, and the policy is the user's claim
//! > about their market rather than the engine's knowledge of it.
//!
//! L2 replaces the claim with a reading. When an order joins a price
//! level, the quantity already displayed there **is** the queue ahead of
//! it — not a `Fixed(250)` somebody chose, and not a multiple of an
//! observation's volume. For a maker strategy that number is most of the
//! difference between the backtest and the account, which is why this
//! tier exists at all.
//!
//! # It wraps L1, which wraps L0
//!
//! Same reason, one layer up: `FR-MATCH-2` freezes L0, and the cheapest
//! way to keep a promise like that is to make breaking it impossible.
//! [`L2Engine`] owns an [`L1Engine`] and adds one thing — the queue
//! measurement — through the entry L1 already exposes for it. Latency
//! and impact policy still come from L1, because neither is in a book.
//!
//! # What L2 still cannot do, and no MBP feed can
//!
//! **It cannot tell a cancellation ahead of you from one behind you.**
//! An incremental depth feed reports that a level shrank; it does not
//! say which resting order left. So the queue here depletes on *trades*
//! and not on cancellations, which makes it **conservative**: a real
//! queue also shortens when someone ahead gives up, and this one does
//! not, so an order fills here no earlier than it would have in life.
//!
//! Distinguishing them needs order-by-order data — MBO, an L3 feed —
//! and Binance does not publish one. MBP is the ceiling, and this is the
//! honest shape of that ceiling rather than a gap to apologise for.
//!
//! # The book is the venue's, not the strategy's
//!
//! `oq_engine::book::Book` holds *this process's* resting orders.
//! `oq_book::Book` holds *the venue's* displayed depth, rebuilt from
//! incremental updates. Two different questions, and this tier is the
//! only place both are open at once.

use oq_book::{Book as VenueBook, DepthUpdate, SequenceError};
use oq_types::{Nanos, PriceTicks, Side, Working};

use crate::l1::L1Engine;

/// L1 with the queue read from the venue's book.
#[derive(Debug)]
pub struct L2Engine {
    inner: L1Engine,
    venue: VenueBook,
    /// Depth updates the book refused, in sequence order.
    ///
    /// Counted rather than absorbed. A book that quietly accepted an
    /// out-of-order update would produce plausible prices that are
    /// wrong, and a queue measured against them would be wrong in a way
    /// nothing downstream could see.
    refused: u64,
}

impl L2Engine {
    /// Wrap an L1 engine and give it a venue book to read.
    #[must_use]
    pub fn new(inner: L1Engine) -> Self {
        Self {
            inner,
            venue: VenueBook::new(),
            refused: 0,
        }
    }

    /// Seed the book from a REST snapshot.
    ///
    /// Without one the book bootstraps from the first update it can
    /// place, and every level that existed beforehand is invisible — so
    /// a queue measured early reads shorter than it was, which is the
    /// one direction this tier must not be wrong in. [`Self::ready`]
    /// says whether that has happened.
    pub fn install_snapshot(
        &mut self,
        update_id: u64,
        bids: &[oq_book::Level],
        asks: &[oq_book::Level],
    ) {
        self.venue.install_snapshot(update_id, bids, asks);
    }

    /// Apply one incremental depth update.
    ///
    /// # Errors
    /// The sequencing rule it broke. The update is not applied and the
    /// book is left as it was.
    pub fn on_depth(&mut self, update: &DepthUpdate) -> Result<(), SequenceError> {
        match self.venue.apply(update) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.refused += 1;
                Err(e)
            }
        }
    }

    /// Submit an order, measuring what is queued ahead of it.
    ///
    /// A market order measures nothing: it is the thing the queue is
    /// waiting for. A limit order at a price the book does not show
    /// measures **zero**, because nothing is displayed there — which is
    /// a reading and not an absence, and is why it is not `None`.
    pub fn submit(&mut self, order: Working, now: Nanos) {
        let ahead = order.price().map(|p| self.displayed_at(order.side(), p));
        self.inner.submit_with_queue(order, now, ahead);
    }

    /// Quantity displayed at `price` on `side`.
    ///
    /// Zero when the level is absent. An order joining a level that is
    /// not there is first in the queue, which is what an empty level
    /// means.
    #[must_use]
    pub fn displayed_at(&self, side: Side, price: PriceTicks) -> i64 {
        let levels = match side {
            Side::Buy => self.venue.bids(),
            Side::Sell => self.venue.asks(),
        };
        levels
            .levels()
            .iter()
            .find(|l| l.price == price.0)
            .map_or(0, |l| l.qty)
    }

    /// Whether the book has been placed in sequence.
    ///
    /// `false` means every queue measurement so far is a lower bound:
    /// levels that existed before the first applied update are not in
    /// it. A caller reporting fills from a book in this state is
    /// reporting an optimism it can name.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.venue.is_ready()
    }

    /// Depth updates the book refused.
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// The venue's book, for a caller that wants to look at it.
    #[must_use]
    pub const fn venue_book(&self) -> &VenueBook {
        &self.venue
    }

    /// The L1 engine underneath, for everything this tier does not change.
    #[must_use]
    pub const fn inner(&self) -> &L1Engine {
        &self.inner
    }

    /// Mutable access, for ticks, cancels and fill collection.
    pub const fn inner_mut(&mut self) -> &mut L1Engine {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::{Impact, Latency, Policy, QueueAhead};
    use oq_book::Level;
    use oq_types::{Order, OrderId, OrderKind, QtyLots, Stamp, TimeInForce};

    fn engine(queue: QueueAhead) -> L2Engine {
        L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy {
                queue,
                latency: Latency::default(),
                impact: Impact { coefficient: 0 },
            },
        ))
    }

    fn limit(id: u64, side: Side, price: i64, qty: i64) -> Working {
        Working::Live(
            Order::new(
                OrderId(id),
                side,
                OrderKind::Limit {
                    price: PriceTicks(price),
                },
                QtyLots(qty),
                TimeInForce::GoodTilCancel,
                Stamp::new(0, 0),
            )
            .expect("positive quantity")
            .accept(),
        )
    }

    fn seeded() -> L2Engine {
        let mut e = engine(QueueAhead::None);
        e.install_snapshot(
            100,
            &[
                Level {
                    price: 99,
                    qty: 250,
                },
                Level { price: 98, qty: 40 },
            ],
            &[Level {
                price: 101,
                qty: 70,
            }],
        );
        e
    }

    /// The queue ahead is what the venue displays, not what a policy
    /// claims.
    ///
    /// This is the entire tier in one assertion. L1 makes the caller
    /// name a number; here the number is read off the level the order
    /// joins.
    #[test]
    fn the_queue_is_read_from_the_book() {
        let e = seeded();
        assert_eq!(e.displayed_at(Side::Buy, PriceTicks(99)), 250);
        assert_eq!(e.displayed_at(Side::Buy, PriceTicks(98)), 40);
        assert_eq!(e.displayed_at(Side::Sell, PriceTicks(101)), 70);
    }

    /// A level the book does not show measures zero, and that is a
    /// reading rather than an absence.
    ///
    /// An order joining an empty level is first in the queue, which is
    /// what an empty level means. Returning `None` here would fall back
    /// to the policy and quietly re-import the assumption this tier
    /// exists to remove.
    #[test]
    fn an_empty_level_measures_zero_and_not_unknown() {
        let e = seeded();
        assert_eq!(e.displayed_at(Side::Buy, PriceTicks(97)), 0);
        assert_eq!(e.displayed_at(Side::Sell, PriceTicks(105)), 0);
    }

    /// The measurement overrides a policy that says otherwise.
    ///
    /// A tier that second-guessed its own data would be reporting L1's
    /// answer under L2's name, which is the confusion the ladder exists
    /// to prevent.
    #[test]
    fn a_measurement_beats_the_policy_it_replaces() {
        let mut e = engine(QueueAhead::Fixed(QtyLots(9_999)));
        e.install_snapshot(100, &[Level { price: 99, qty: 12 }], &[]);
        e.submit(limit(1, Side::Buy, 99, 5), Nanos(0));
        // Nothing has traded, so an order with 12 ahead of it is still
        // queued and not resting in L0's book.
        assert_eq!(
            e.inner().queued(),
            1,
            "the order should be waiting behind 12"
        );
    }

    /// A book that has not been placed in sequence says so.
    ///
    /// Every measurement taken before that is a lower bound — levels
    /// that existed beforehand are invisible — and an engine reporting
    /// fills from one is reporting an optimism it can at least name.
    #[test]
    fn a_book_that_has_not_bootstrapped_says_so() {
        let e = engine(QueueAhead::None);
        assert!(!e.ready(), "an empty book cannot be ready");
        assert!(seeded().ready(), "a snapshot makes it ready");
    }

    /// A refused depth update is counted and not absorbed.
    ///
    /// `oq-book`'s own documentation: a book that quietly accepts an
    /// out-of-order message is worse than one that stops, because it
    /// produces plausible prices that are wrong. A queue measured
    /// against those is wrong in a way nothing downstream can see.
    #[test]
    fn an_out_of_sequence_update_is_refused_and_counted() {
        let mut e = seeded();
        let stale = DepthUpdate {
            event_ms: 1,
            first_id: 500,
            final_id: 600,
            prev_final_id: Some(499),
            bids: vec![Level { price: 99, qty: 1 }],
            asks: Vec::new(),
        };
        assert!(
            e.on_depth(&stale).is_err(),
            "a jump in sequence must refuse"
        );
        assert_eq!(e.refused(), 1);
        assert_eq!(
            e.displayed_at(Side::Buy, PriceTicks(99)),
            250,
            "a refused update must not have moved the book"
        );
    }

    /// The measurement survives entry latency.
    ///
    /// An order delayed on the way to the venue is held outside L0
    /// until it arrives, and the queue it was measured against travels
    /// with it. Dropping it there would make latency silently downgrade
    /// an L2 order to an L1 one — the tier would change without the
    /// report changing, which is exactly the confusion the ladder
    /// exists to prevent.
    ///
    /// The measurement is deliberately *not* recomputed on arrival. It
    /// belongs to the moment the order was sent; by the time it lands
    /// the book has moved, and re-reading it would answer a different
    /// question with the same name.
    #[test]
    fn a_measured_queue_is_not_lost_to_entry_latency() {
        use crate::l1::Delay;
        let mut e = L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy {
                queue: QueueAhead::Fixed(QtyLots(0)),
                latency: Latency {
                    entry: Delay::Fixed(Nanos(1_000)),
                    response: Delay::Fixed(Nanos(0)),
                },
                impact: Impact { coefficient: 0 },
            },
        ));
        e.install_snapshot(100, &[Level { price: 99, qty: 77 }], &[]);
        e.submit(limit(1, Side::Buy, 99, 5), Nanos(0));
        assert_eq!(e.inner().queued(), 0, "still in flight, not yet queued");

        // The order arrives. Its 77 came with it; had it been dropped,
        // the policy's zero would have put it straight into L0's book.
        let tick = crate::Tick {
            stamp: oq_types::Stamp::new(2_000, 2_000),
            last: PriceTicks(99),
            high: PriceTicks(99),
            low: PriceTicks(99),
            bid: PriceTicks(99),
            ask: PriceTicks(100),
            volume: QtyLots(0),
        };
        let _ = e.inner_mut().on_tick(&tick);
        assert_eq!(
            e.inner().queued(),
            1,
            "the order arrived with a measured queue of 77 and should be waiting behind it"
        );
    }

    /// A market order measures nothing, because it is the thing the
    /// queue is waiting for.
    #[test]
    fn a_market_order_queues_for_nothing() {
        let mut e = engine(QueueAhead::Fixed(QtyLots(500)));
        e.install_snapshot(
            100,
            &[Level {
                price: 99,
                qty: 250,
            }],
            &[],
        );
        let market = Working::Live(
            Order::new(
                OrderId(7),
                Side::Buy,
                OrderKind::Market,
                QtyLots(1),
                TimeInForce::ImmediateOrCancel,
                Stamp::new(0, 0),
            )
            .expect("positive quantity")
            .accept(),
        );
        e.submit(market, Nanos(0));
        assert_eq!(e.inner().queued(), 0, "a market order does not queue");
    }
}
