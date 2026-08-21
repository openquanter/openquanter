//! L2: the queue and the taker's cost, measured from the venue's book.
//!
//! `FR-MATCH-6`. L1's module documentation states the limit this tier
//! exists to remove:
//!
//! > the tick format carries a price path and a cumulative volume. It
//! > does not carry book depth [...] So L1 cannot *measure* queue-ahead
//! > or latency; it applies a policy, and the policy is the user's claim
//! > about their market rather than the engine's knowledge of it.
//!
//! L2 replaces the claim with a reading, on both sides of the trade.
//!
//! **For a maker**, when an order joins a price level, the quantity
//! already displayed there **is** the queue ahead of it — not a
//! `Fixed(250)` somebody chose, and not a multiple of an observation's
//! volume. That number is most of the difference between the backtest
//! and the account, which is why this tier exists at all.
//!
//! **For a taker**, size is walked instead of estimated. L0 fills any
//! quantity at the observation's reference price, so five lots and five
//! thousand cost the same; L1 corrects that with a square-root penalty
//! scaled by the high-low range, which is a shape borrowed from the
//! literature rather than a property of this venue at this instant.
//! With the depth in hand the walk is simply performed — take the best
//! level, then the next, until the size is filled, and pay what that
//! costs.
//!
//! # It wraps L1, which wraps L0
//!
//! Same reason, one layer up: `FR-MATCH-2` freezes L0, and the cheapest
//! way to keep a promise like that is to make breaking it impossible.
//! [`L2Engine`] owns an [`L1Engine`] and adds measurements through the
//! entries L1 exposes for them, one per policy it can replace. Latency
//! still comes from L1: it is a property of the path between this
//! process and the venue, and no book carries it.
//!
//! Each measurement **displaces** the policy for the fills it covers
//! rather than compounding with it, and a fill the book cannot price
//! keeps the policy. So a tier is never all-or-nothing, and
//! [`L2Engine::swept`] against [`L2Engine::unswept`] is what tells a
//! report which of the two actually priced a run. A backtest that is
//! nine parts policy is an L1 backtest wearing an L2 label.
//!
//! # A measurement is only ever applied against the trader
//!
//! Where the book and the tick disagree, the worse price wins. A
//! reconstructed book is a *different* measurement of the same instant,
//! not a more authoritative one, and the ladder is only worth climbing
//! if climbing it cannot make a backtest look better — otherwise the
//! tiers become something to shop among.
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
//! **The walk does not move the venue's book.** Our own taker consumes
//! a working copy, so several fills in one observation deplete one
//! book; the venue's own depth is the feed's, and what it holds is what
//! the venue displayed. Modelling how the rest of the market reacts to
//! our trade is a different claim entirely, and one nothing here
//! measures.
//!
//! # The book is the venue's, not the strategy's
//!
//! `oq_engine::book::Book` holds *this process's* resting orders.
//! `oq_book::Book` holds *the venue's* displayed depth, rebuilt from
//! incremental updates. Two different questions, and this tier is the
//! only place both are open at once.

use oq_book::{Book as VenueBook, DepthUpdate, SequenceError, Side as BookSide};
use oq_types::{Nanos, PriceTicks, QtyLots, Side, Working};

use crate::Tick;

use crate::l0::L0Fill;
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
    /// Taker fills priced by walking the book.
    swept: u64,
    /// Taker fills the book could not reach, priced by L1's policy.
    ///
    /// The pair is what lets a report say which tier each fill actually
    /// came from. A run that is 90% policy is an L1 run wearing an L2
    /// label, and the only thing separating those two claims is this
    /// count.
    unswept: u64,
}

impl L2Engine {
    /// Wrap an L1 engine and give it a venue book to read.
    #[must_use]
    pub fn new(inner: L1Engine) -> Self {
        Self {
            inner,
            venue: VenueBook::new(),
            refused: 0,
            swept: 0,
            unswept: 0,
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

    /// Advance to an observation, pricing taker fills off the book.
    ///
    /// A taker does not trade at the touch; it walks the levels until
    /// its size is filled, and pays the volume-weighted price of the
    /// walk. L1 estimates that with a square-root penalty scaled by the
    /// observation's high-low range, because a tick carries no depth.
    /// Here the depth is in hand, so the walk is performed rather than
    /// approximated -- and the estimate is not also charged, or the same
    /// climb up the book would be paid for twice.
    ///
    /// # Several fills in one observation deplete one book
    ///
    /// The walk runs against a working copy of the venue's depth that
    /// each fill consumes, so the second taker in an observation
    /// reaches a thinner book than the first. Pricing them all against
    /// the untouched book would make size free in exactly the case that
    /// makes it expensive.
    ///
    /// # When the book cannot answer
    ///
    /// A fill larger than every level held falls back to L1's policy
    /// and is counted in [`unswept`](Self::unswept). Pricing it from the
    /// levels that happen to be present would put the least trustworthy
    /// number on the largest order.
    pub fn on_tick(&mut self, tick: &Tick) -> &[L0Fill] {
        let (mut swept, mut unswept) = (0_u64, 0_u64);
        {
            let venue = &self.venue;
            let mut working = WorkingDepth::default();
            self.inner.on_tick_with_impact(tick, |side, qty| {
                match working.take(venue, side, qty) {
                    Some(price) => {
                        swept += 1;
                        Some(price)
                    }
                    None => {
                        unswept += 1;
                        None
                    }
                }
            });
        }
        self.swept += swept;
        self.unswept += unswept;
        self.inner.released()
    }

    /// Taker fills priced by walking the book.
    #[must_use]
    pub const fn swept(&self) -> u64 {
        self.swept
    }

    /// Taker fills the book could not reach, priced by L1's policy.
    #[must_use]
    pub const fn unswept(&self) -> u64 {
        self.unswept
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

    /// Withdraw every order, in flight and resting alike.
    pub fn cancel_all(&mut self) {
        self.inner.cancel_all();
    }

    /// Withdraw one order, wherever it currently is.
    pub fn cancel(&mut self, id: oq_types::OrderId) -> bool {
        self.inner.cancel(id)
    }

    /// The instrument this engine matches.
    #[must_use]
    pub const fn instrument(&self) -> oq_types::InstrumentId {
        self.inner.instrument()
    }

    /// The last traded price seen.
    #[must_use]
    pub const fn last_price(&self) -> Option<PriceTicks> {
        self.inner.last_price()
    }

    /// Orders resting in the book. As [`L1Engine::book`], not every
    /// order this engine holds.
    #[must_use]
    pub const fn book(&self) -> &crate::book::Book {
        self.inner.book()
    }

    /// Snapshot the identifier watermark, for recovery.
    #[must_use]
    pub const fn id_watermark(&self) -> (u64, u64) {
        self.inner.id_watermark()
    }

    /// Restore the identifier watermark after recovery.
    pub fn restore_ids(&mut self, watermark: (u64, u64)) {
        self.inner.restore_ids(watermark);
    }

    /// Orders that exist but are not yet in the book.
    #[must_use]
    pub fn shadowed(&self) -> usize {
        self.inner.shadowed()
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

/// A per-observation copy of the venue's depth, consumed by fills.
///
/// Cloned when a taker fill first asks for it and not before: an
/// observation that produces no taker fill -- which is most of them --
/// copies nothing. The copy lasts one observation, because the venue's
/// own book is the feed's and the next update rebuilds the truth.
#[derive(Debug, Default)]
struct WorkingDepth {
    bids: Option<BookSide>,
    asks: Option<BookSide>,
}

impl WorkingDepth {
    /// The volume-weighted price of taking `qty`, rounded against the
    /// taker, or `None` if the book does not go that deep.
    fn take(&mut self, venue: &VenueBook, side: Side, qty: QtyLots) -> Option<PriceTicks> {
        // A buy takes from the asks, a sell from the bids.
        let levels = match side {
            Side::Buy => self.asks.get_or_insert_with(|| venue.asks().clone()),
            Side::Sell => self.bids.get_or_insert_with(|| venue.bids().clone()),
        };
        let swept = levels.take(qty.0);
        if swept.exhausted || swept.taken <= 0 {
            return None;
        }
        // Integer division truncates toward zero, which favours the
        // buyer. A buy rounds up so the fraction of a tick is paid, not
        // pocketed; a sell already loses it by truncating.
        let cost = swept.cost;
        let taken = i128::from(swept.taken);
        let vwap = match side {
            Side::Buy => (cost + taken - 1) / taken,
            Side::Sell => cost / taken,
        };
        i64::try_from(vwap).ok().map(PriceTicks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::{Delay, Impact, Latency, Policy, QueueAhead};
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

    // ---- Taker impact, walked rather than estimated ----

    fn market(id: u64, side: Side, qty: i64) -> Working {
        Working::Live(
            Order::new(
                OrderId(id),
                side,
                OrderKind::Market,
                QtyLots(qty),
                TimeInForce::ImmediateOrCancel,
                Stamp::new(0, 0),
            )
            .expect("positive quantity")
            .accept(),
        )
    }

    fn at(price: i64, qty: i64) -> Level {
        Level { price, qty }
    }

    /// A tick whose touch is 99/101 and which trades `volume`.
    fn observation(volume: i64) -> Tick {
        Tick {
            stamp: Stamp::new(1_000, 1_000),
            last: PriceTicks(100),
            high: PriceTicks(101),
            low: PriceTicks(99),
            bid: PriceTicks(99),
            ask: PriceTicks(101),
            volume: QtyLots(volume),
        }
    }

    fn instant(impact: Impact) -> L2Engine {
        L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy {
                queue: QueueAhead::None,
                latency: Latency {
                    entry: Delay::Fixed(Nanos(0)),
                    response: Delay::Fixed(Nanos(0)),
                },
                impact,
            },
        ))
    }

    /// The tier in one assertion, for the taker side.
    ///
    /// L0 fills a market buy at the observation's ask, whatever the
    /// size: 500 lots and 5 lots pay the same. The book says the ask
    /// holds 50, and 450 more sit above it. So the fill is the walk --
    /// and no coefficient was set, which is what proves the number came
    /// from the depth and not from a policy.
    #[test]
    fn a_taker_pays_the_walk_up_the_book_not_the_touch() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(100, &[at(99, 1_000)], &[at(101, 50), at(102, 450)]);
        e.submit(market(1, Side::Buy, 500), Nanos(0));

        let fills = e.on_tick(&observation(10_000)).to_vec();
        assert_eq!(fills.len(), 1);

        // 50 at 101 and 450 at 102 is 50_450 for 500 lots: 100.9,
        // rounded to 101 against the buyer... which is the touch. Use a
        // size that clears the level to make the walk visible.
        assert_eq!(e.swept(), 1, "the fill was priced from the book");
        assert_eq!(e.unswept(), 0);
        let walked = (50 * 101 + 450 * 102 + 499) / 500;
        assert_eq!(fills[0].fill.price, PriceTicks(walked));
        assert!(
            fills[0].fill.price.0 > 101,
            "a 500-lot buy into a 50-lot ask must cost more than the touch, got {}",
            fills[0].fill.price.0
        );
    }

    /// The measurement displaces the policy; it does not stack with it.
    ///
    /// Charging both would make the same climb up the book be paid for
    /// twice, and the second charge would have no measurement behind it.
    #[test]
    fn a_measured_fill_is_not_also_charged_the_policy() {
        let book = &[at(101, 50), at(102, 450)];
        let mut measured_only = instant(Impact { coefficient: 0 });
        measured_only.install_snapshot(100, &[at(99, 1_000)], book);
        measured_only.submit(market(1, Side::Buy, 500), Nanos(0));
        let a = measured_only.on_tick(&observation(10_000))[0].fill.price;

        let mut with_policy = instant(Impact { coefficient: 500 });
        with_policy.install_snapshot(100, &[at(99, 1_000)], book);
        with_policy.submit(market(1, Side::Buy, 500), Nanos(0));
        let b = with_policy.on_tick(&observation(10_000))[0].fill.price;

        assert_eq!(a, b, "the coefficient must not move a measured fill");
    }

    /// Two takers in one observation deplete one book.
    ///
    /// The second reaches what the first left. Pricing both against the
    /// untouched book would say size is free in the one case where it
    /// is not.
    #[test]
    fn the_second_taker_in_an_observation_reaches_a_thinner_book() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(
            100,
            &[at(99, 1_000)],
            &[at(101, 100), at(102, 100), at(103, 800)],
        );
        e.submit(market(1, Side::Buy, 100), Nanos(0));
        e.submit(market(2, Side::Buy, 100), Nanos(0));

        let fills = e.on_tick(&observation(10_000)).to_vec();
        assert_eq!(fills.len(), 2);
        assert_eq!(e.swept(), 2);
        assert_eq!(fills[0].fill.price, PriceTicks(101), "took the whole 101");
        assert_eq!(
            fills[1].fill.price,
            PriceTicks(102),
            "the 101s were gone; pricing this at 101 too would make size free"
        );
    }

    /// An order deeper than the book falls back to L1's policy and says
    /// so, rather than being priced off whatever levels happen to be
    /// present.
    #[test]
    fn an_order_deeper_than_the_book_falls_back_and_is_counted() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(100, &[at(99, 1_000)], &[at(101, 10)]);
        e.submit(market(1, Side::Buy, 10_000), Nanos(0));

        let fills = e.on_tick(&observation(100_000)).to_vec();
        assert_eq!(e.swept(), 0);
        assert_eq!(
            e.unswept(),
            1,
            "the fall back to policy is counted, not silent"
        );
        // Coefficient zero, so the policy charges nothing: the fill is
        // L0's reference price. The point is that it is not 101 dressed
        // up as a measurement.
        assert_eq!(fills[0].fill.price, PriceTicks(101));
    }

    /// A sell walks the bids downward.
    #[test]
    fn a_selling_taker_walks_the_bids() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(100, &[at(99, 50), at(98, 450)], &[at(101, 1_000)]);
        e.submit(market(1, Side::Sell, 500), Nanos(0));

        let fills = e.on_tick(&observation(10_000)).to_vec();
        let walked = (50 * 99 + 450 * 98) / 500;
        assert_eq!(fills[0].fill.price, PriceTicks(walked));
        assert!(
            fills[0].fill.price.0 < 99,
            "a 500-lot sell into a 50-lot bid must receive less than the touch"
        );
    }

    /// The ladder stays monotone: L2 never prices a taker better than
    /// L1 would have. A book that disagrees with the tick in the
    /// taker's favour is a second measurement of the same instant, not
    /// a licence to improve the fill.
    #[test]
    fn a_book_better_than_the_tick_does_not_improve_the_fill() {
        let mut e = instant(Impact { coefficient: 0 });
        // The whole size rests a tick inside the observation's ask.
        e.install_snapshot(100, &[at(99, 1_000)], &[at(100, 1_000)]);
        e.submit(market(1, Side::Buy, 10), Nanos(0));

        let fills = e.on_tick(&observation(10_000)).to_vec();
        assert_eq!(
            fills[0].fill.price,
            PriceTicks(101),
            "the walk says 100 and L0 says 101; the taker pays 101"
        );
    }

    /// A maker fill is not asked about. Its price is the price it
    /// rested at, and no walk of the book has anything to add to it.
    #[test]
    fn a_maker_fill_is_left_alone() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(100, &[at(99, 0)], &[at(99, 1_000)]);
        e.submit(limit(1, Side::Buy, 99, 5), Nanos(0));

        // The ask has to reach the resting buy for it to trade at all.
        let reached = Tick {
            ask: PriceTicks(99),
            ..observation(10_000)
        };
        let fills = e.on_tick(&reached).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].fill.liquidity, oq_types::Liquidity::Maker);
        assert_eq!(fills[0].fill.price, PriceTicks(99));
        assert_eq!(e.swept(), 0, "a maker fill is never swept for");
        assert_eq!(
            e.unswept(),
            0,
            "nor counted as one the book could not price"
        );
    }

    // ---- The seam: a tier that measures nothing answers as L0 ----

    /// L2 with no book and a transparent policy reproduces L0 exactly.
    ///
    /// This is what makes the tier safe to put behind a switch. A run
    /// that has not been given depth must match as the tier below it,
    /// or choosing L2 would change results for reasons having nothing
    /// to do with the depth it was chosen for -- and the frozen anchor
    /// would be frozen in name only.
    ///
    /// An empty book measures a queue of **zero**, which is the reading
    /// and not an absence. That it agrees with a transparent policy's
    /// zero is why this holds.
    #[test]
    fn an_unfed_l2_reproduces_l0_exactly() {
        let ticks = [observation(1_000), observation(2_000), observation(3_000)];

        let mut l0 = crate::L0Engine::new(oq_types::InstrumentId::new(1));
        l0.submit_limit_with(
            OrderId(1),
            Side::Buy,
            PriceTicks(101),
            QtyLots(5),
            Stamp::new(0, 0),
            oq_types::Offset::Open,
        );
        let mut from_l0: Vec<oq_types::Fill> = Vec::new();
        for t in &ticks {
            from_l0.extend(l0.on_tick(t).iter().map(|f| f.fill));
        }

        let mut l2 = L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy::TRANSPARENT,
        ));
        l2.submit(
            crate::limit_order(
                OrderId(1),
                Side::Buy,
                PriceTicks(101),
                QtyLots(5),
                Stamp::new(0, 0),
                oq_types::Offset::Open,
            ),
            Nanos(0),
        );
        let mut from_l2: Vec<oq_types::Fill> = Vec::new();
        for t in &ticks {
            from_l2.extend(l2.on_tick(t).iter().map(|f| f.fill));
        }

        assert!(!from_l0.is_empty(), "the fixture must actually fill");
        assert_eq!(from_l2, from_l0, "an unfed L2 must not change L0's answer");
        assert_eq!(l2.swept(), 0, "and must not claim to have measured one");
    }

    /// An order built by the shared constructor still goes through this
    /// tier's `submit`.
    ///
    /// The constructors are free functions precisely so no tier grows
    /// its own -- but a caller holding one of them could hand the result
    /// to whichever engine is nearest. Given to L0 it rests instantly;
    /// given here it serves entry latency and joins a measured queue.
    /// The run's label is only worth something if the second happens.
    #[test]
    fn an_order_built_outside_still_enters_through_this_tier() {
        let mut e = L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy {
                queue: QueueAhead::None,
                latency: Latency {
                    entry: Delay::Fixed(Nanos(10_000)),
                    response: Delay::Fixed(Nanos(0)),
                },
                impact: Impact { coefficient: 0 },
            },
        ));
        e.install_snapshot(100, &[at(99, 640)], &[]);
        e.submit(
            crate::limit_order(
                OrderId(1),
                Side::Buy,
                PriceTicks(99),
                QtyLots(5),
                Stamp::new(0, 0),
                oq_types::Offset::Open,
            ),
            Nanos(0),
        );

        // Still serving entry latency, so not in the book: had this gone
        // straight to L0 it would already be resting there.
        assert_eq!(e.shadowed(), 1);
        assert_eq!(e.book().iter().count(), 0);

        let tick = Tick {
            stamp: Stamp::new(20_000, 20_000),
            ..observation(0)
        };
        let _ = e.on_tick(&tick);
        assert_eq!(
            e.inner().queued(),
            1,
            "it arrived carrying the 640 the book displayed, not the policy's zero"
        );
    }

    /// Withdrawing everything must reach the orders that are still in
    /// flight, not only the ones already resting.
    #[test]
    fn cancel_all_reaches_orders_that_have_not_landed() {
        let mut e = L2Engine::new(L1Engine::new(
            oq_types::InstrumentId::new(1),
            Policy {
                queue: QueueAhead::None,
                latency: Latency {
                    entry: Delay::Fixed(Nanos(10_000)),
                    response: Delay::Fixed(Nanos(0)),
                },
                impact: Impact { coefficient: 0 },
            },
        ));
        e.submit(limit(1, Side::Buy, 99, 5), Nanos(0));
        assert_eq!(e.shadowed(), 1);

        e.cancel_all();
        assert_eq!(e.shadowed(), 0, "an in-flight order is still an order");

        let tick = Tick {
            stamp: Stamp::new(20_000, 20_000),
            ..observation(0)
        };
        let fills = e.on_tick(&tick).to_vec();
        assert!(
            fills.is_empty(),
            "a cancelled order must not arrive and fill afterwards"
        );
    }

    /// An observation with no taker fill copies no depth. The counters
    /// are the only visible effect, so they are what the test can see.
    #[test]
    fn an_observation_without_a_taker_sweeps_nothing() {
        let mut e = instant(Impact { coefficient: 0 });
        e.install_snapshot(100, &[at(99, 500)], &[at(101, 500)]);
        let _ = e.on_tick(&observation(1_000));
        assert_eq!(e.swept(), 0);
        assert_eq!(e.unswept(), 0);
    }
}
