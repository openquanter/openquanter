//! The engine, running beside the live session, disagreeing out loud.
//!
//! # The claim this exists to make true
//!
//! `IMPLEMENTATION.md` §1 says backtest and live differ only in the
//! event producer, and that there is no separate backtest engine to keep
//! in sync with a live one. Until this module, that was an intention:
//! `oq-live` journalled its own decisions in its own frames and never
//! touched `oq-core`, so the live process and the backtest were two
//! programs that happened to agree about a strategy.
//!
//! A shadow is the same [`Kernel`] a backtest runs, fed the same events,
//! standing next to the session. It places nothing and cancels nothing.
//! Its only product is the list of places where it and the venue
//! disagree.
//!
//! # Why the disagreement is the point
//!
//! M3's second entry trigger asks that a strategy be run in shadow
//! against a live venue "long enough to compare, and every divergence
//! between the shadow run and the venue attributed rather than
//! tolerated". Nothing produced that comparison, because nothing ran the
//! two side by side. This does, and it names the four ways they can
//! part:
//!
//! - the model filled and the venue did not — the backtest is optimistic
//!   about this order, which is the direction that flatters a strategy
//! - the venue filled and the model did not — the backtest is *pessimistic*,
//!   which is the direction nobody investigates and which hides just as
//!   much
//! - both filled, at different prices — slippage, and the number is the
//!   size of it
//! - the positions disagree — the most serious, because every subsequent
//!   decision on both sides is now computed from a different base
//!
//! # What a divergence is not
//!
//! It is not a defect on its own. A limit order that filled live and not
//! in the model may simply mean the model's queue position was
//! pessimistic — L0 does not model queue position and does not claim to.
//! The value is that the difference is *counted and attributed* rather
//! than absorbed into a P&L discrepancy nobody can decompose six weeks
//! later.

use oq_core::{Event, Kernel, Output, State};
use oq_engine::Tick;
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, Fill, InstrumentId, Nanos, OrderId, PriceTicks, QtyLots, Side};

/// How the model and the venue parted company.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// The model filled an order the venue has not.
    ///
    /// The direction that flatters a backtest.
    ModelOnly {
        /// The order.
        id: OrderId,
        /// Where the model thinks it filled.
        price: PriceTicks,
        /// How much.
        qty: QtyLots,
        /// When the model decided.
        at: Nanos,
    },
    /// The venue filled an order the model has not.
    ///
    /// The direction nobody investigates, and it hides exactly as much.
    VenueOnly {
        /// The order.
        id: OrderId,
        /// Where the venue filled it.
        price: PriceTicks,
        /// How much.
        qty: QtyLots,
        /// When the venue reported it.
        at: Nanos,
    },
    /// Both filled, at different prices.
    Price {
        /// The order.
        id: OrderId,
        /// What the model expected.
        model: PriceTicks,
        /// What the venue did.
        venue: PriceTicks,
    },
    /// Both filled, for different quantities.
    Quantity {
        /// The order.
        id: OrderId,
        /// What the model expected.
        model: QtyLots,
        /// What the venue did.
        venue: QtyLots,
    },
    /// The net positions disagree.
    ///
    /// The most serious, because from here every decision on both sides
    /// is computed from a different base — and unlike a fill difference,
    /// this one compounds.
    Position {
        /// The model's net position.
        model: QtyLots,
        /// The venue's.
        venue: QtyLots,
        /// When the comparison was made.
        at: Nanos,
    },
}

impl Divergence {
    /// Whether this one makes the backtest look better than reality.
    ///
    /// Reported because the two directions deserve different attention:
    /// an optimistic divergence inflates a result somebody may act on,
    /// and a pessimistic one is a strategy being under-credited. Both
    /// are wrong; only one of them is dangerous.
    #[must_use]
    pub const fn flatters_the_model(&self) -> bool {
        matches!(self, Self::ModelOnly { .. })
    }

    /// One line, for a log.
    #[must_use]
    pub fn summary_line(&self) -> String {
        match self {
            Self::ModelOnly { id, price, qty, .. } => format!(
                "model filled {} x{} on order {} and the venue did not",
                price.0, qty.0, id.0
            ),
            Self::VenueOnly { id, price, qty, .. } => format!(
                "venue filled {} x{} on order {} and the model did not",
                price.0, qty.0, id.0
            ),
            Self::Price { id, model, venue } => format!(
                "order {} filled at {} live and {} in the model ({:+} ticks)",
                id.0,
                venue.0,
                model.0,
                venue.0 - model.0
            ),
            Self::Quantity { id, model, venue } => format!(
                "order {} filled x{} live and x{} in the model",
                id.0, venue.0, model.0
            ),
            Self::Position { model, venue, .. } => format!(
                "positions disagree: model {} venue {} ({:+})",
                model.0,
                venue.0,
                venue.0 - model.0
            ),
        }
    }
}

/// A fill the model produced and has not yet seen matched by the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    id: OrderId,
    side: Side,
    price: PriceTicks,
    qty: QtyLots,
    at: Nanos,
    /// The price prevailing when this fill happened.
    ///
    /// Recorded here rather than looked up later because "prevailing"
    /// means *at that moment*, and a shadow that reconstructed it
    /// afterwards would be attributing slippage against a price the
    /// order never saw. `None` before the first observation, which is
    /// what makes slippage and latency unavailable rather than wrong.
    reference: Option<PriceTicks>,
}

/// The engine, run beside a live session.
///
/// It is fed the same events the session sees and the fills the venue
/// reports, and it accumulates the differences. It never sends anything:
/// a shadow that could place an order is a second trading system, and
/// the cutover playbook exists because two of those is the worst state
/// available.
pub struct Shadow {
    kernel: Kernel,
    /// The last observation, for the prevailing price at a fill.
    last: Option<PriceTicks>,
    /// Fills both sides made, kept so the gap can be attributed rather
    /// than only counted.
    matched: Vec<oq_parity::attribution::Matched>,
    /// Fills only one side made, likewise.
    unpaired: Vec<oq_parity::attribution::Unmatched>,
    /// Model fills waiting for the venue to confirm them.
    unmatched_model: Vec<Pending>,
    /// Venue fills waiting for the model to produce them.
    unmatched_venue: Vec<Pending>,
    divergences: Vec<Divergence>,
    /// Events applied, so a run can say how much comparing it did.
    applied: usize,
    /// How long a fill may go unmatched before it is called a
    /// divergence.
    ///
    /// Not zero. The venue's report and the model's decision are not
    /// simultaneous — the model fills on the tick that crossed the
    /// price, the venue reports over a socket some milliseconds later —
    /// and calling that a divergence would report a divergence for every
    /// fill, which is the same as reporting none.
    grace: Nanos,
}

impl Shadow {
    /// Build a shadow of one instrument.
    #[must_use]
    pub fn new(
        instrument: InstrumentId,
        contract: Contract,
        table: TierTable,
        starting_balance: Cash,
    ) -> Self {
        Self {
            kernel: Kernel::new(State::new(instrument, contract, table, starting_balance)),
            last: None,
            matched: Vec::new(),
            unpaired: Vec::new(),
            unmatched_model: Vec::new(),
            unmatched_venue: Vec::new(),
            divergences: Vec::new(),
            applied: 0,
            grace: Nanos(2_000_000_000),
        }
    }

    /// How long a fill may go unmatched before it counts as a
    /// divergence.
    #[must_use]
    pub const fn with_grace(mut self, grace: Nanos) -> Self {
        self.grace = grace;
        self
    }

    /// Feed the shadow an event the session also saw.
    ///
    /// Returns nothing: the shadow's product is its divergences, and a
    /// caller that acted on its outputs would be running a second
    /// trading system.
    pub fn apply(&mut self, event: &Event) {
        self.applied += 1;
        let at = event.at();
        // `apply` borrows the kernel, so the outputs are copied out
        // before anything else touches it.
        let produced: Vec<Output> = self.kernel.apply(event).to_vec();
        for output in produced {
            if let Output::Filled(fill) = output {
                self.record_model_fill(&fill, at);
            }
        }
        self.expire(at);
    }

    /// Convenience for the commonest event.
    pub fn on_tick(&mut self, tick: Tick) {
        // The prevailing price, for the fills that happen next. Recorded
        // before the kernel sees the observation, because a fill the
        // kernel makes from *this* tick happened at this price.
        self.last = Some(tick.last);
        self.apply(&Event::Tick {
            instrument: None,
            tick,
        });
    }

    /// Tell the shadow what the venue actually did.
    pub fn on_venue_fill(
        &mut self,
        id: OrderId,
        side: Side,
        price: PriceTicks,
        qty: QtyLots,
        at: Nanos,
    ) {
        // Match against a model fill for the same order, if there is
        // one. Matching by order id rather than by price: the whole
        // point is to notice when the prices differ.
        if let Some(i) = self.unmatched_model.iter().position(|p| p.id == id) {
            let model = self.unmatched_model.remove(i);
            // Kept whether or not it diverged: a matched pair with the
            // same price contributes zero slippage, and a decomposition
            // that only saw the divergent pairs would be attributing a
            // gap against a subset of the trades that caused it.
            self.matched.push(oq_parity::attribution::Matched {
                side: model.side,
                qty: model.qty,
                model_price: model.price,
                venue_price: price,
                reference_price: model.reference,
            });
            if model.price != price {
                self.divergences.push(Divergence::Price {
                    id,
                    model: model.price,
                    venue: price,
                });
            }
            if model.qty != qty {
                self.divergences.push(Divergence::Quantity {
                    id,
                    model: model.qty,
                    venue: qty,
                });
            }
            return;
        }
        self.unmatched_venue.push(Pending {
            id,
            side,
            price,
            qty,
            at,
            reference: self.last,
        });
        self.expire(at);
    }

    /// Compare the model's net position against the venue's.
    ///
    /// Called by a reconciler on whatever schedule it already runs. A
    /// position divergence is the one worth interrupting for, because
    /// unlike a fill difference it compounds.
    pub fn compare_position(&mut self, venue: QtyLots, at: Nanos) {
        let model = self.net_position();
        if model != venue {
            self.divergences
                .push(Divergence::Position { model, venue, at });
        }
    }

    /// The model's net position, long minus short.
    #[must_use]
    pub fn net_position(&self) -> QtyLots {
        let s = self.kernel.state();
        QtyLots(s.holding().qty.0 - s.holding().short_qty.0)
    }

    /// The model's equity at the last mark it saw.
    #[must_use]
    pub fn equity(&self) -> Cash {
        self.kernel.state().equity()
    }

    /// Everything the model and the venue disagreed about.
    #[must_use]
    pub fn divergences(&self) -> &[Divergence] {
        &self.divergences
    }

    /// Events compared.
    ///
    /// A run with no divergences and four events has established
    /// nothing, and this is how a reader tells that apart from a run
    /// that agreed a million times.
    #[must_use]
    pub const fn applied(&self) -> usize {
        self.applied
    }

    /// Divergences that make the backtest look better than reality.
    #[must_use]
    pub fn flattering(&self) -> usize {
        self.divergences
            .iter()
            .filter(|d| d.flatters_the_model())
            .count()
    }

    /// Everything the attribution decomposition needs from this run.
    ///
    /// The bridge between what a shadow observes and what
    /// `oq_parity::attribution` decomposes — the two halves of the
    /// project's headline claim, which until this existed did not
    /// connect: the shadow produced divergences and the decomposition
    /// wanted evidence, and nothing turned one into the other.
    ///
    /// `funding` and `fees` are the caller's, as `(venue, model)` pairs,
    /// and they are **arguments rather than fields** because a shadow
    /// does not see them. The venue's statement is the only source for
    /// what was charged, and a shadow that defaulted them to zero would
    /// report a gap fully explained by causes nobody looked at — which
    /// `FR-ATTRIB-6` exists to prevent. Passing `None` says so, and the
    /// report then declines to produce a residual at all.
    ///
    /// Call [`Shadow::finish`] first. Fills still inside the grace
    /// period are neither matched nor reported as unmatched, so
    /// evidence taken before it is evidence with a hole in it.
    #[must_use]
    pub fn evidence(
        &self,
        funding: Option<(Cash, Cash)>,
        fees: Option<(Cash, Cash)>,
    ) -> oq_parity::attribution::Evidence {
        oq_parity::attribution::Evidence {
            matched: self.matched.clone(),
            unmatched: self.unpaired.clone(),
            funding,
            fees,
        }
    }

    /// Fees the model charged itself.
    ///
    /// The other half of the fee component: the venue's number comes
    /// from its own trade records, this one from the kernel's fee
    /// schedule, and the difference is what a wrong fee tier costs.
    /// Deriving either from the other would make that difference zero.
    #[must_use]
    pub fn model_fees(&self) -> Cash {
        self.kernel.state().fees
    }

    /// The model's realized result, for the decomposition's other side.
    ///
    /// The gap `attribute` decomposes is the venue's P&L minus this one,
    /// and the two must come from independent sources — deriving either
    /// from the components would make the residual zero by construction.
    #[must_use]
    pub fn model_pnl(&self) -> Cash {
        let s = self.kernel.state();
        Cash(s.realized.0 - s.fees.0 + s.funding.0)
    }

    /// Flush anything still unmatched, at the end of a run.
    ///
    /// Without this, a fill in the last second of a session is neither
    /// matched nor reported, and a shadow that silently drops its tail
    /// under-reports exactly when a session ended badly.
    pub fn finish(&mut self, at: Nanos) {
        self.grace = Nanos(0);
        self.expire(at);
    }

    fn record_model_fill(&mut self, fill: &Fill, at: Nanos) {
        if let Some(i) = self.unmatched_venue.iter().position(|p| p.id == fill.order) {
            let venue = self.unmatched_venue.remove(i);
            if venue.price != fill.price {
                self.divergences.push(Divergence::Price {
                    id: fill.order,
                    model: fill.price,
                    venue: venue.price,
                });
            }
            if venue.qty != fill.qty {
                self.divergences.push(Divergence::Quantity {
                    id: fill.order,
                    model: fill.qty,
                    venue: venue.qty,
                });
            }
            return;
        }
        self.unmatched_model.push(Pending {
            id: fill.order,
            side: fill.side,
            reference: self.last,
            price: fill.price,
            qty: fill.qty,
            at,
        });
    }

    /// Promote anything that has waited longer than the grace period.
    fn expire(&mut self, now: Nanos) {
        let cutoff = now.0.saturating_sub(self.grace.0);
        let mut still_waiting = Vec::new();
        for p in core::mem::take(&mut self.unmatched_model) {
            if p.at.0 <= cutoff {
                self.divergences.push(Divergence::ModelOnly {
                    id: p.id,
                    price: p.price,
                    qty: p.qty,
                    at: p.at,
                });
                self.unpaired.push(oq_parity::attribution::Unmatched {
                    side: p.side,
                    qty: p.qty,
                    price: p.price,
                    reference_price: p.reference,
                    at_venue: false,
                });
            } else {
                still_waiting.push(p);
            }
        }
        self.unmatched_model = still_waiting;

        let mut still_waiting = Vec::new();
        for p in core::mem::take(&mut self.unmatched_venue) {
            if p.at.0 <= cutoff {
                self.divergences.push(Divergence::VenueOnly {
                    id: p.id,
                    price: p.price,
                    qty: p.qty,
                    at: p.at,
                });
                self.unpaired.push(oq_parity::attribution::Unmatched {
                    side: p.side,
                    qty: p.qty,
                    price: p.price,
                    reference_price: p.reference,
                    at_venue: true,
                });
            } else {
                still_waiting.push(p);
            }
        }
        self.unmatched_venue = still_waiting;
    }
}

/// Build the submit event for an order the session is sending.
///
/// Exists so a caller cannot forget a field: the shadow is only worth
/// anything if it sees exactly what the session sent, and an event
/// assembled by hand at each call site is an event that eventually
/// differs from the order.
#[must_use]
pub fn submitted(
    id: OrderId,
    side: Side,
    price: Option<PriceTicks>,
    qty: QtyLots,
    offset: oq_types::Offset,
    stamp: oq_types::Stamp,
) -> Event {
    Event::Submit {
        id,
        side,
        price,
        qty,
        offset,
        stamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{Offset, Stamp};

    const SEC: i64 = 1_000_000_000;

    fn shadow() -> Shadow {
        Shadow::new(
            InstrumentId::new(1),
            Contract::new(10_000),
            TierTable::example_btcusdt(),
            Cash::from_units(100_000),
        )
    }

    fn stamp(ns: i64) -> Stamp {
        Stamp {
            exch: Nanos(ns),
            local: Nanos(ns),
        }
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

    /// Submit a market order and let it match.
    ///
    /// L0 matches against the *next* observation, not the one that was
    /// on screen when the order was sent — which is the honest ordering
    /// (an order cannot trade against a print that already happened)
    /// and is why this helper feeds a second tick.
    fn buy(s: &mut Shadow, id: u64, ns: i64, price: i64) {
        s.on_tick(tick(ns, price));
        s.apply(&submitted(
            OrderId(id),
            Side::Buy,
            None,
            QtyLots(1),
            Offset::Open,
            stamp(ns),
        ));
        s.on_tick(tick(ns, price));
    }

    /// The baseline. When the venue confirms what the model did, at the
    /// same price, there is nothing to report — and a shadow that
    /// reported something here would report something for every fill,
    /// which is the same as reporting nothing.
    #[test]
    fn agreement_produces_no_divergence() {
        let mut s = shadow();
        buy(&mut s, 1, SEC, 6_000_000);
        let filled = s.net_position();
        assert_eq!(
            filled,
            QtyLots(1),
            "the model must have filled, or this test is vacuous"
        );

        s.on_venue_fill(
            OrderId(1),
            Side::Buy,
            PriceTicks(6_000_001),
            QtyLots(1),
            Nanos(SEC),
        );
        s.finish(Nanos(10 * SEC));
        assert_eq!(s.divergences(), &[], "identical fills must not diverge");
    }

    /// The direction that flatters a backtest: the model booked a fill
    /// the venue never made.
    #[test]
    fn a_fill_the_venue_never_made_is_reported_and_flagged_as_flattering() {
        let mut s = shadow();
        buy(&mut s, 1, SEC, 6_000_000);
        // The venue says nothing, ever.
        s.finish(Nanos(60 * SEC));

        assert_eq!(s.divergences().len(), 1, "{:?}", s.divergences());
        assert!(matches!(
            s.divergences()[0],
            Divergence::ModelOnly { id: OrderId(1), .. }
        ));
        assert_eq!(s.flattering(), 1);
    }

    /// The direction nobody investigates. A backtest that misses a fill
    /// the venue made is understating the strategy, which is wrong in a
    /// way that looks like conservatism.
    #[test]
    fn a_fill_the_model_never_made_is_reported_and_is_not_flattering() {
        let mut s = shadow();
        s.on_tick(tick(SEC, 6_000_000));
        s.on_venue_fill(
            OrderId(9),
            Side::Buy,
            PriceTicks(6_000_000),
            QtyLots(1),
            Nanos(SEC),
        );
        s.finish(Nanos(60 * SEC));

        assert_eq!(s.divergences().len(), 1, "{:?}", s.divergences());
        assert!(matches!(
            s.divergences()[0],
            Divergence::VenueOnly { id: OrderId(9), .. }
        ));
        assert_eq!(
            s.flattering(),
            0,
            "a missed fill does not flatter the model"
        );
    }

    /// Slippage, with the number attached. This is the divergence that
    /// happens on most fills and the one whose *size* is the useful
    /// part.
    #[test]
    fn a_different_price_is_reported_with_the_difference() {
        let mut s = shadow();
        buy(&mut s, 1, SEC, 6_000_000);
        s.on_venue_fill(
            OrderId(1),
            Side::Buy,
            PriceTicks(6_000_050),
            QtyLots(1),
            Nanos(SEC),
        );
        s.finish(Nanos(10 * SEC));

        match s.divergences() {
            [Divergence::Price { id, model, venue }] => {
                assert_eq!(*id, OrderId(1));
                assert_eq!(venue.0 - model.0, 49, "the slippage in ticks");
            }
            other => panic!("expected one price divergence, got {other:?}"),
        }
        assert!(
            s.divergences()[0].summary_line().contains("+49"),
            "the line must carry the number: {}",
            s.divergences()[0].summary_line()
        );
    }

    /// A partial fill live against a full fill in the model is a
    /// quantity divergence, not a price one, and conflating them would
    /// hide a real liquidity finding behind a slippage number.
    #[test]
    fn a_different_quantity_is_its_own_finding() {
        let mut s = shadow();
        s.on_tick(tick(SEC, 6_000_000));
        s.apply(&submitted(
            OrderId(1),
            Side::Buy,
            None,
            QtyLots(10),
            Offset::Open,
            stamp(SEC),
        ));
        // The model has not matched yet; the next observation is what
        // lets it. The venue speaks after, which is the realistic order.
        s.on_tick(tick(2 * SEC, 6_000_000));
        s.on_venue_fill(
            OrderId(1),
            Side::Buy,
            PriceTicks(6_000_001),
            QtyLots(3),
            Nanos(2 * SEC),
        );
        s.finish(Nanos(10 * SEC));

        assert!(
            s.divergences()
                .iter()
                .any(|d| matches!(d, Divergence::Quantity { model, venue, .. }
                                  if model.0 == 10 && venue.0 == 3)),
            "{:?}",
            s.divergences()
        );
    }

    /// The one worth interrupting for. Unlike a fill difference, a
    /// position difference compounds: every later decision on both sides
    /// is computed from a different base.
    #[test]
    fn a_position_disagreement_is_reported() {
        let mut s = shadow();
        buy(&mut s, 1, SEC, 6_000_000);
        s.compare_position(QtyLots(1), Nanos(2 * SEC));
        assert_eq!(s.divergences(), &[], "agreement is silent");

        s.compare_position(QtyLots(4), Nanos(3 * SEC));
        match s.divergences() {
            [Divergence::Position { model, venue, .. }] => {
                assert_eq!((model.0, venue.0), (1, 4));
            }
            other => panic!("expected a position divergence, got {other:?}"),
        }
    }

    /// The grace period is the whole reason this is usable. A model
    /// fills on the tick that crossed; the venue reports over a socket
    /// milliseconds later. Calling that a divergence would report one
    /// for every fill.
    #[test]
    fn a_late_confirmation_within_the_grace_period_is_not_a_divergence() {
        let mut s = shadow().with_grace(Nanos(5 * SEC));
        buy(&mut s, 1, SEC, 6_000_000);
        // Three seconds of ticks pass before the venue speaks.
        for i in 2..=4 {
            s.on_tick(tick(i * SEC, 6_000_000));
        }
        assert_eq!(s.divergences(), &[], "still inside the grace period");

        s.on_venue_fill(
            OrderId(1),
            Side::Buy,
            PriceTicks(6_000_001),
            QtyLots(1),
            Nanos(4 * SEC),
        );
        s.finish(Nanos(20 * SEC));
        assert_eq!(s.divergences(), &[], "a late but matching fill agrees");
    }

    /// And past the grace period it is a divergence, or the grace period
    /// would be an excuse rather than a tolerance.
    #[test]
    fn a_confirmation_that_never_comes_becomes_a_divergence() {
        let mut s = shadow().with_grace(Nanos(5 * SEC));
        buy(&mut s, 1, SEC, 6_000_000);
        for i in 2..=20 {
            s.on_tick(tick(i * SEC, 6_000_000));
        }
        assert_eq!(s.divergences().len(), 1, "{:?}", s.divergences());
        assert!(matches!(s.divergences()[0], Divergence::ModelOnly { .. }));
    }

    /// A fill in the last second of a session must not be dropped. A
    /// shadow that silently loses its tail under-reports exactly when a
    /// session ended badly.
    #[test]
    fn the_tail_is_flushed_rather_than_dropped() {
        let mut s = shadow().with_grace(Nanos(60 * SEC));
        buy(&mut s, 1, SEC, 6_000_000);
        assert_eq!(s.divergences(), &[], "inside the grace period");
        s.finish(Nanos(2 * SEC));
        assert_eq!(
            s.divergences().len(),
            1,
            "the tail must be reported at the end"
        );
    }

    /// "No divergences" and "no comparison" look identical in a report
    /// unless the count is there.
    #[test]
    fn a_run_says_how_much_it_compared() {
        let mut s = shadow();
        assert_eq!(s.applied(), 0);
        for i in 1..=100 {
            s.on_tick(tick(i * SEC, 6_000_000));
        }
        assert_eq!(s.applied(), 100);
        assert_eq!(s.divergences(), &[], "quiet ticks produce nothing");
    }

    /// The shadow is the same kernel a backtest runs, so a position it
    /// takes has to move its equity the way a backtest's would. If this
    /// failed, the shadow would be a third implementation rather than
    /// the second use of one.
    #[test]
    fn the_shadow_accounts_the_way_the_backtest_does() {
        let mut s = shadow();
        let opening = s.equity();
        buy(&mut s, 1, SEC, 6_000_000);
        // Price moves up; a long position is worth more.
        s.on_tick(tick(2 * SEC, 6_100_000));
        assert!(
            s.equity().0 > opening.0,
            "a long into a rising market must gain: {} -> {}",
            opening.0,
            s.equity().0
        );
    }
}
