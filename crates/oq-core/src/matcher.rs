//! Which matcher the kernel is holding.
//!
//! The kernel used to hold an [`L0Engine`] by name, so the fidelity tier
//! a run used was decided by the type of one field and could not be
//! decided by anything else. `L2Engine` existed and no backtest could
//! reach it: the tier was written, the place it plugs in was not.
//!
//! # Why an enum rather than a trait object
//!
//! Three reasons, and the first is the one that decides it.
//!
//! **The tiers are a closed set.** L0, L1 and L2 are named in
//! `FR-MATCH-*` and in the fidelity ladder; a fourth is a documented
//! change, not an extension point. A trait would advertise an openness
//! that does not exist and invite a matcher nobody can compare against
//! the frozen anchor.
//!
//! **Dispatch is on the hot path.** Every observation of every run goes
//! through `on_tick`, and the throughput floor is a gate in CI.
//!
//! **A snapshot has to name what it restored into.** Recovery compares a
//! fingerprint against a rebuilt state, and `Box<dyn Matcher>` has no
//! answer to "which one were you" that survives a round trip.
//!
//! # What it must not do
//!
//! Selecting a tier must not change results unless that tier was given
//! something the one below it did not have. [`Matcher::L1`] with a
//! transparent policy and [`Matcher::L2`] with no book both reproduce L0
//! exactly — asserted in `oq-engine`, and the reason a switch here is
//! safe. A ladder whose rungs differ for reasons other than the data
//! they were given is a menu, not a claim about fidelity.

use oq_engine::{L0Engine, L0Fill, L1Engine, L2Engine, Tick};
use oq_types::{InstrumentId, OrderId, PriceTicks, Working};

/// The matcher a run is using.
///
/// # Why only the upper tiers are boxed
///
/// `L0Engine` is 216 bytes and the boxed variants are 8, which clippy
/// reads as a lopsided enum and suggests boxing all three. The suggestion
/// optimises the wrong axis here: this enum lives in `State`, of which a
/// run has **one**, so the 200 bytes are paid once per run. Boxing L0
/// would instead add an indirection to `on_tick` — the default tier, on
/// the path every observation of every run takes, with a throughput floor
/// gating it in CI. One pointer hop per tick against 200 bytes per run is
/// not a close call.
///
/// The upper tiers are boxed for the same reason read the other way: an
/// `L2Engine` carries a venue book, and a run that never asks for one
/// should not carry its footprint in every `State` it moves.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Matcher {
    /// The frozen anchor. Fills at the observation's prices, with no
    /// queue, no latency and no impact.
    L0(L0Engine),
    /// Queue, latency and impact as **policy**: numbers the caller
    /// supplies about their own market, because a tick carries no depth
    /// to measure them from.
    L1(Box<L1Engine>),
    /// The same, with queue and taker cost **measured** from the venue's
    /// reconstructed book. Falls back to L1's policy for anything the
    /// book cannot answer, and says how often it did.
    L2(Box<L2Engine>),
}

impl Matcher {
    /// The frozen anchor, which is what a run gets unless it asks for
    /// more.
    #[must_use]
    #[inline]
    pub fn l0(instrument: InstrumentId) -> Self {
        Self::L0(L0Engine::new(instrument))
    }

    /// Which tier this is, for a report that has to say so.
    ///
    /// A run reporting fills without naming the matcher that produced
    /// them is a number with no provenance, and the tiers disagree by
    /// design.
    #[must_use]
    #[inline]
    pub const fn tier(&self) -> &'static str {
        match self {
            Self::L0(_) => "L0",
            Self::L1(_) => "L1",
            Self::L2(_) => "L2",
        }
    }

    /// Rest an order.
    ///
    /// `now` is when the order was sent. L0 has no use for it — it has
    /// no concept of an order being in flight — and the upper tiers
    /// cannot do without it, so it is passed always rather than
    /// reconstructed by whoever happens to need it.
    #[inline]
    pub fn submit(&mut self, order: Working, now: oq_types::Nanos) {
        match self {
            Self::L0(e) => e.submit(order),
            Self::L1(e) => e.submit(order, now),
            Self::L2(e) => e.submit(order, now),
        }
    }

    /// Withdraw one order, wherever it currently is.
    #[inline]
    pub fn cancel(&mut self, id: OrderId) -> bool {
        match self {
            Self::L0(e) => e.cancel(id),
            Self::L1(e) => e.cancel(id),
            Self::L2(e) => e.cancel(id),
        }
    }

    /// Withdraw every order, in flight and resting alike.
    #[inline]
    pub fn cancel_all(&mut self) -> usize {
        match self {
            Self::L0(e) => e.cancel_all(),
            Self::L1(e) => e.cancel_all(),
            Self::L2(e) => e.cancel_all(),
        }
    }

    /// Advance to an observation and return the fills it produced.
    #[inline]
    pub fn on_tick(&mut self, tick: &Tick) -> &[L0Fill] {
        match self {
            Self::L0(e) => e.on_tick(tick),
            Self::L1(e) => e.on_tick(tick),
            Self::L2(e) => e.on_tick(tick),
        }
    }

    /// Orders resting in the book.
    ///
    /// **Not every order the matcher holds** above L0: one in flight or
    /// behind a queue is in neither this nor the book. [`shadowed`] is
    /// the other half, and a caller fingerprinting state needs both.
    ///
    /// [`shadowed`]: Self::shadowed
    #[must_use]
    #[inline]
    pub const fn book(&self) -> &oq_engine::book::Book {
        match self {
            Self::L0(e) => e.book(),
            Self::L1(e) => e.book(),
            Self::L2(e) => e.book(),
        }
    }

    /// Orders that exist but are not yet in the book.
    ///
    /// Always zero at L0, where an order rests the moment it is
    /// submitted.
    #[must_use]
    #[inline]
    pub fn shadowed(&self) -> usize {
        match self {
            Self::L0(_) => 0,
            Self::L1(e) => e.shadowed(),
            Self::L2(e) => e.shadowed(),
        }
    }

    /// The instrument being matched.
    #[must_use]
    #[inline]
    pub const fn instrument(&self) -> InstrumentId {
        match self {
            Self::L0(e) => e.instrument(),
            Self::L1(e) => e.instrument(),
            Self::L2(e) => e.instrument(),
        }
    }

    /// The last traded price seen.
    #[must_use]
    #[inline]
    pub const fn last_price(&self) -> Option<PriceTicks> {
        match self {
            Self::L0(e) => e.last_price(),
            Self::L1(e) => e.last_price(),
            Self::L2(e) => e.last_price(),
        }
    }

    /// Snapshot the identifier watermark, for recovery.
    #[must_use]
    #[inline]
    pub const fn id_watermark(&self) -> (u64, u64) {
        match self {
            Self::L0(e) => e.id_watermark(),
            Self::L1(e) => e.id_watermark(),
            Self::L2(e) => e.id_watermark(),
        }
    }

    /// Restore the identifier watermark after recovery.
    #[inline]
    pub fn restore_ids(&mut self, watermark: (u64, u64)) {
        match self {
            Self::L0(e) => e.restore_ids(watermark),
            Self::L1(e) => e.restore_ids(watermark),
            Self::L2(e) => e.restore_ids(watermark),
        }
    }

    /// Apply a depth update, if this tier reads one.
    #[inline]
    pub fn on_depth(&mut self, update: &oq_engine::DepthUpdate) -> DepthOutcome {
        match self {
            Self::L0(_) | Self::L1(_) => DepthOutcome::NotRead,
            Self::L2(e) => match e.on_depth(update) {
                Ok(()) => DepthOutcome::Applied,
                Err(e) => DepthOutcome::Refused(e),
            },
        }
    }

    /// Seed the venue book from a snapshot, if this tier keeps one.
    ///
    /// An incremental depth stream is meaningless without one: the
    /// updates say what *changed*, and a book with nothing to change
    /// refuses them. Bootstrapping from the first update instead would
    /// silently make every level that existed beforehand invisible, so
    /// a queue measured early reads shorter than it was — which is the
    /// direction that flatters a backtest.
    #[inline]
    pub fn install_snapshot(
        &mut self,
        update_id: u64,
        bids: &[oq_engine::Level],
        asks: &[oq_engine::Level],
    ) -> bool {
        match self {
            Self::L0(_) | Self::L1(_) => false,
            Self::L2(e) => {
                e.install_snapshot(update_id, bids, asks);
                true
            }
        }
    }
}

/// What a matcher did with a depth update.
///
/// Three outcomes rather than a boolean, because they call for three
/// different reactions and merging any two hides one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepthOutcome {
    /// The book advanced.
    Applied,
    /// The book refused it, naming the sequencing rule that broke.
    /// Messages were lost, and the correct response is a fresh
    /// snapshot rather than guessing the missing state.
    Refused(oq_engine::SequenceError),
    /// This tier does not read depth.
    ///
    /// Not an error — and not something to pass over either. A run
    /// handed depth and matching without it is producing a lower tier's
    /// answer under whatever name the report carries.
    NotRead,
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_engine::l1::{Impact, Latency, Policy, QueueAhead};
    use oq_types::{Nanos, Offset, QtyLots, Side, Stamp};

    fn tick(n: i64, price: i64) -> Tick {
        Tick {
            stamp: Stamp::new(n, n),
            last: PriceTicks(price),
            high: PriceTicks(price),
            low: PriceTicks(price),
            bid: PriceTicks(price - 1),
            ask: PriceTicks(price + 1),
            volume: QtyLots(1_000),
        }
    }

    /// Priced at the ask the fixture quotes, so it actually trades: a
    /// buy is triggered by the ask, and one a tick below it never fills
    /// at any tier.
    fn order(id: u64) -> Working {
        oq_engine::limit_order(
            OrderId(id),
            Side::Buy,
            PriceTicks(101),
            QtyLots(5),
            Stamp::new(0, 0),
            Offset::Open,
        )
    }

    fn fills_from(mut m: Matcher) -> Vec<oq_types::Fill> {
        m.submit(order(1), Nanos(0));
        let mut out = Vec::new();
        for n in 1..=3 {
            out.extend(m.on_tick(&tick(n * 1_000, 100)).iter().map(|f| f.fill));
        }
        out
    }

    /// Choosing a tier that was given nothing extra must not change the
    /// answer. Otherwise the tiers are a menu to shop among rather than
    /// a claim about fidelity, and the frozen anchor is frozen in name.
    #[test]
    fn the_upper_tiers_answer_as_l0_when_given_nothing_extra() {
        let l0 = fills_from(Matcher::l0(InstrumentId::new(1)));
        assert!(!l0.is_empty(), "the fixture must actually fill");

        let l1 = fills_from(Matcher::L1(Box::new(L1Engine::new(
            InstrumentId::new(1),
            Policy::TRANSPARENT,
        ))));
        assert_eq!(l1, l0, "a transparent L1 must reproduce L0");

        let l2 = fills_from(Matcher::L2(Box::new(L2Engine::new(L1Engine::new(
            InstrumentId::new(1),
            Policy::TRANSPARENT,
        )))));
        assert_eq!(l2, l0, "an L2 with no book must reproduce L0");
    }

    /// A tier that cannot read depth says so rather than accepting it.
    ///
    /// Silently ignoring it is how a run reads an L2 archive, reports
    /// L1's answer, and gives nobody a reason to look.
    #[test]
    fn depth_offered_to_a_tier_that_cannot_use_it_is_refused() {
        let update = oq_engine::DepthUpdate {
            event_ms: 0,
            first_id: 1,
            final_id: 1,
            prev_final_id: None,
            bids: vec![oq_engine::Level {
                price: 99,
                qty: 100,
            }],
            asks: Vec::new(),
        };

        assert_eq!(
            Matcher::l0(InstrumentId::new(1)).on_depth(&update),
            DepthOutcome::NotRead
        );
        assert_eq!(
            Matcher::L1(Box::new(L1Engine::new(
                InstrumentId::new(1),
                Policy::TRANSPARENT
            )))
            .on_depth(&update),
            DepthOutcome::NotRead
        );

        // L2 reads it -- and refuses this one, because a book with no
        // snapshot has nothing for an incremental update to change.
        // "Read it and rejected it" is a different fact from "does not
        // read depth", which is why they are different variants.
        let mut l2 = Matcher::L2(Box::new(L2Engine::new(L1Engine::new(
            InstrumentId::new(1),
            Policy::TRANSPARENT,
        ))));
        assert_eq!(
            l2.on_depth(&update),
            DepthOutcome::Refused(oq_engine::SequenceError::NoSnapshot)
        );

        // Seeded, it applies.
        assert!(l2.install_snapshot(0, &[], &[]));
        assert_eq!(l2.on_depth(&update), DepthOutcome::Applied);
        assert!(
            !Matcher::l0(InstrumentId::new(1)).install_snapshot(0, &[], &[]),
            "a tier with no book has nowhere to put a snapshot"
        );
    }

    /// L0 has no in-flight state, so the count is zero rather than
    /// unavailable — a caller adding book and shadowed gets the right
    /// total at every tier.
    #[test]
    fn an_order_in_flight_is_counted_at_the_tiers_that_have_one() {
        let mut l0 = Matcher::l0(InstrumentId::new(1));
        l0.submit(order(1), Nanos(0));
        assert_eq!(l0.shadowed(), 0);
        assert_eq!(l0.book().iter().count(), 1, "L0 rests it immediately");

        let mut l1 = Matcher::L1(Box::new(L1Engine::new(
            InstrumentId::new(1),
            Policy {
                queue: QueueAhead::None,
                latency: Latency {
                    entry: oq_engine::l1::Delay::Fixed(Nanos(10_000)),
                    response: oq_engine::l1::Delay::Fixed(Nanos(0)),
                },
                impact: Impact { coefficient: 0 },
            },
        )));
        l1.submit(order(1), Nanos(0));
        assert_eq!(l1.shadowed(), 1, "still in flight");
        assert_eq!(l1.book().iter().count(), 0, "so not in the book");
    }

    /// The tier is reportable, because fills without a named matcher are
    /// numbers with no provenance.
    #[test]
    fn a_matcher_names_its_tier() {
        assert_eq!(Matcher::l0(InstrumentId::new(1)).tier(), "L0");
        assert_eq!(
            Matcher::L1(Box::new(L1Engine::new(
                InstrumentId::new(1),
                Policy::TRANSPARENT
            )))
            .tier(),
            "L1"
        );
    }
}
