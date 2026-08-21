//! One kernel per instrument, sharing nothing.
//!
//! `FR-CORE-6`: the core is sharded by instrument, each shard is
//! single-threaded, and cross-shard interaction happens through the
//! sequenced stream rather than shared mutable state.
//!
//! # What sharding actually means here
//!
//! The requirement is usually read as a performance decision, and it is
//! also an accounting one. "Shares no mutable state" and "trades several
//! instruments on one balance" are contradictory: a balance two
//! instruments draw on *is* shared mutable state, and no arrangement of
//! threads changes that.
//!
//! Both arrangements exist at venues, and this workspace has one type
//! for each:
//!
//! - **Cross margin** — one balance behind every position. That is
//!   [`State`] with several [`Holding`](crate::kernel::Holding)s, in one
//!   kernel. Margin nets across instruments and a loss on one is paid
//!   for by a gain on another. It cannot be sharded, by construction.
//! - **Isolated margin** — one balance per instrument, and a position
//!   that exhausts its own balance is liquidated while the others
//!   continue. That is [`Shards`]: one kernel each, nothing shared, and
//!   the arrangement `FR-CORE-6` describes.
//!
//! Choosing between them is an account decision at the venue, so it is
//! one here too rather than a deployment flag.
//!
//! # Why this does not spawn a thread
//!
//! `FR-CORE-1` forbids the core from spawning anything, and the reason
//! outlives the rule: a run whose result depended on a scheduler would
//! not be reproducible from `(journal, seed, commit)`, which is
//! `FR-CORE-4`.
//!
//! So shards are stepped in order here, and running them on separate
//! threads is the host's decision — safe precisely because they share
//! nothing. What this type provides is the isolation that makes that
//! safe, and the routing that makes it addressable. A `Shards` stepped
//! on one thread and the same shards stepped on several produce the same
//! outputs, which is the property that has to hold before any of it is
//! worth doing.

use oq_types::{InstrumentId, OrderId};

use crate::event::Event;
use crate::kernel::{Kernel, Output, RejectReason, State};

/// Several independent kernels, addressed by instrument.
#[derive(Debug)]
pub struct Shards {
    shards: Vec<Kernel>,
    outputs: Vec<Output>,
}

impl Shards {
    /// Build one shard per state.
    ///
    /// # Panics
    /// If two states name the same instrument. Two shards for one
    /// instrument would each hold a position in it and neither would be
    /// the account's — a wrong number rather than a duplicate row.
    #[must_use]
    pub fn new(states: Vec<State>) -> Self {
        let mut seen: Vec<InstrumentId> = states.iter().map(|s| s.holding().instrument).collect();
        let before = seen.len();
        seen.sort_unstable_by_key(|i| i.0);
        seen.dedup();
        assert_eq!(
            seen.len(),
            before,
            "each instrument gets one shard, or neither holds the position"
        );

        Self {
            shards: states.into_iter().map(Kernel::new).collect(),
            outputs: Vec::with_capacity(16),
        }
    }

    /// How many shards there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// The shard holding an instrument.
    #[must_use]
    pub fn shard(&self, instrument: InstrumentId) -> Option<&Kernel> {
        self.shards
            .iter()
            .find(|k| k.state().holding().instrument == instrument)
    }

    /// The shard holding an instrument, mutably.
    pub fn shard_mut(&mut self, instrument: InstrumentId) -> Option<&mut Kernel> {
        self.shards
            .iter_mut()
            .find(|k| k.state().holding().instrument == instrument)
    }

    /// Every shard, in the order they were given.
    pub fn iter(&self) -> impl Iterator<Item = &Kernel> + '_ {
        self.shards.iter()
    }

    /// Apply an event to the shard it addresses.
    ///
    /// An event naming no instrument reaches the only shard, and is
    /// refused when there is more than one — the same rule a single
    /// kernel applies to its holdings, for the same reason: guessing
    /// would mark one instrument's position at another's price.
    ///
    /// An event for an instrument no shard holds is refused rather than
    /// dropped. A shard set is a closed list of what this process
    /// trades, and an event outside it is a routing mistake upstream,
    /// not an observation to be discarded quietly.
    pub fn apply(&mut self, event: &Event) -> &[Output] {
        self.outputs.clear();
        let Some(index) = self.route(event) else {
            self.outputs.push(Output::Rejected {
                id: order_id(event).unwrap_or(OrderId::new(0)),
                reason: RejectReason::UnroutableObservation,
            });
            return &self.outputs;
        };
        // Copied out rather than returned by reference: the borrow of
        // one shard would otherwise outlive this call and stop the next
        // event reaching a different shard.
        self.outputs
            .extend_from_slice(self.shards[index].apply(event));
        &self.outputs
    }

    /// Which shard an event belongs to.
    fn route(&self, event: &Event) -> Option<usize> {
        match instrument_of(event) {
            Some(id) => self
                .shards
                .iter()
                .position(|k| k.state().holding().instrument == id),
            None => (self.shards.len() == 1).then_some(0),
        }
    }
}

/// The instrument an event addresses, when it names one.
///
/// Orders name theirs; a cancel does not, because an order id is unique
/// within a shard and the shard holding it is the one that answers.
/// That makes a cancel unroutable across shards, which is a real limit
/// and is stated as one: a host holding several shards has to know which
/// it placed an order through, and it does, because it placed it.
const fn instrument_of(event: &Event) -> Option<InstrumentId> {
    match event {
        Event::Tick { instrument, .. } | Event::Submit { instrument, .. } => *instrument,
        _ => None,
    }
}

const fn order_id(event: &Event) -> Option<OrderId> {
    match event {
        Event::Submit { id, .. } | Event::Cancel { id, .. } => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::PositionMode;
    use oq_engine::Tick;
    use oq_margin::{Contract, MarginTier, TierTable};
    use oq_types::{Cash, Offset, PriceTicks, QtyLots, Ratio, Side, Stamp};

    const BTC: Contract = Contract::new(1_000);

    fn table() -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    fn state_for(id: u32, balance: i64) -> State {
        State::new(
            InstrumentId::new(id),
            BTC,
            table(),
            Cash::from_units(balance),
        )
        .with_mode(PositionMode::OneWay)
    }

    fn tick_on(id: u32, at: i64, price: i64) -> Event {
        Event::Tick {
            instrument: Some(InstrumentId::new(id)),
            tick: Tick::trades_only(Stamp::synthetic(at), price, price, price),
        }
    }

    /// **The property sharding exists for.** What happens in one shard
    /// does not reach another's balance.
    ///
    /// This is isolated margin: a position that exhausts its own balance
    /// is liquidated while the others continue. Under one shared balance
    /// — which is `State` with several holdings — the loss would be paid
    /// for out of the same money, which is cross margin and a different
    /// account.
    #[test]
    fn a_loss_in_one_shard_does_not_reach_another() {
        let mut shards = Shards::new(vec![state_for(1, 10_000), state_for(2, 10_000)]);

        shards.apply(&tick_on(1, 1_000, 100_000));
        shards.apply(&Event::Submit {
            instrument: Some(InstrumentId::new(1)),
            id: OrderId::new(1),
            side: Side::Buy,
            price: None,
            qty: QtyLots(10),
            offset: Offset::Open,
            stamp: Stamp::synthetic(1_000),
        });
        // A market order fills on the *next* observation, so this one is
        // where it is bought — at the price it was submitted against.
        shards.apply(&tick_on(1, 2_000, 100_000));
        // And this is the fall that costs the first shard.
        shards.apply(&tick_on(1, 3_000, 50_000));

        let first = shards
            .shard(InstrumentId::new(1))
            .expect("held")
            .summary()
            .equity;
        let second = shards
            .shard(InstrumentId::new(2))
            .expect("held")
            .summary()
            .equity;

        assert!(
            first < Cash::from_units(10_000),
            "the first shard lost; equity {first:?}, position {:?}",
            shards
                .shard(InstrumentId::new(1))
                .expect("held")
                .summary()
                .qty
        );
        assert_eq!(
            second,
            Cash::from_units(10_000),
            "and the second is untouched"
        );
    }

    /// An event reaches the shard it names.
    #[test]
    fn an_event_routes_by_instrument() {
        let mut shards = Shards::new(vec![state_for(1, 10_000), state_for(2, 10_000)]);
        shards.apply(&tick_on(2, 1_000, 77_000));

        assert_eq!(
            shards
                .shard(InstrumentId::new(2))
                .expect("held")
                .summary()
                .mark,
            PriceTicks(77_000)
        );
        assert_eq!(
            shards
                .shard(InstrumentId::new(1))
                .expect("held")
                .summary()
                .mark,
            PriceTicks::ZERO,
            "the other shard saw nothing"
        );
    }

    /// An instrument no shard holds is refused, not dropped.
    ///
    /// A shard set is a closed list of what this process trades, so an
    /// event outside it is a routing mistake upstream — and one that
    /// vanishes silently is a feed nobody notices is misconfigured.
    #[test]
    fn an_event_for_an_unheld_instrument_is_refused() {
        let mut shards = Shards::new(vec![state_for(1, 10_000)]);
        let out = shards.apply(&tick_on(99, 1_000, 100_000));
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Rejected {
                reason: RejectReason::UnroutableObservation,
                ..
            }
        )));
    }

    /// An unnamed event reaches a lone shard and is refused once there
    /// are two — the same rule a single kernel applies to its holdings.
    #[test]
    fn an_unnamed_event_needs_exactly_one_shard() {
        let unnamed = Event::Tick {
            instrument: None,
            tick: Tick::trades_only(Stamp::synthetic(1_000), 100_000, 100_000, 100_000),
        };

        let mut one = Shards::new(vec![state_for(1, 10_000)]);
        assert!(
            !one.apply(&unnamed)
                .iter()
                .any(|o| matches!(o, Output::Rejected { .. })),
            "one shard is unambiguous"
        );

        let mut two = Shards::new(vec![state_for(1, 10_000), state_for(2, 10_000)]);
        assert!(
            two.apply(&unnamed)
                .iter()
                .any(|o| matches!(o, Output::Rejected { .. })),
            "two are not"
        );
    }

    /// Stepping shards in order gives the same answer as stepping each
    /// separately.
    ///
    /// The property a host needs before putting them on threads: they
    /// share nothing, so the interleaving cannot matter. Asserted rather
    /// than assumed, because it is the whole basis for parallelising
    /// them later.
    #[test]
    fn interleaving_shards_changes_nothing() {
        let events = |id: u32| {
            vec![
                tick_on(id, 1_000, 100_000),
                tick_on(id, 2_000, 101_000),
                tick_on(id, 3_000, 99_000),
            ]
        };

        // Interleaved.
        let mut together = Shards::new(vec![state_for(1, 10_000), state_for(2, 10_000)]);
        for (a, b) in events(1).iter().zip(events(2).iter()) {
            together.apply(a);
            together.apply(b);
        }

        // One shard's events, then the other's.
        let mut apart = Shards::new(vec![state_for(1, 10_000), state_for(2, 10_000)]);
        for e in events(1) {
            apart.apply(&e);
        }
        for e in events(2) {
            apart.apply(&e);
        }

        for id in [1, 2] {
            let a = together.shard(InstrumentId::new(id)).expect("held");
            let b = apart.shard(InstrumentId::new(id)).expect("held");
            assert_eq!(
                a.fingerprint(),
                b.fingerprint(),
                "shard {id} depends on the order events reached other shards"
            );
        }
    }

    /// Two shards for one instrument is refused.
    #[test]
    #[should_panic(expected = "each instrument gets one shard")]
    fn one_instrument_gets_one_shard() {
        let _ = Shards::new(vec![state_for(1, 10_000), state_for(1, 10_000)]);
    }
}
