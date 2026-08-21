//! One thing that happened, in the order it happened.
//!
//! A matcher above L0 needs more than a price path. L2 reads the venue's
//! book, and a book cannot go *in* a [`Tick`]: a tick is a fixed record
//! of seven numbers, and depth is a variable-length list of levels. So
//! the two arrive interleaved on one stream.
//!
//! # Why one stream and not two
//!
//! Two streams means whoever holds them decides which to advance, which
//! is a merge — written once here or once per caller. A merge that gets
//! the order wrong matches an order against a book from the future, and
//! produces a backtest that is wrong in the direction of looking good.
//!
//! # Why the snapshot is its own arrival
//!
//! Because it is one. The venue serves it over REST while the updates
//! come over a socket, and the first update that can be placed against
//! it is where reconstruction begins. A book with no snapshot refuses
//! every update it is given — an incremental stream says what *changed*,
//! and there is nothing to change. Bootstrapping from the first update
//! instead would silently make every level that existed beforehand
//! invisible, so a queue measured early would read shorter than it was,
//! which is the direction that flatters a backtest.
//!
//! This type lives here rather than beside the backtest loop because
//! both ends need it: the loop consumes it, and whatever reads an
//! archive produces it. One definition, so the two cannot drift.

use crate::Tick;
use oq_book::{DepthUpdate, Level};

/// One arrival on a run's input stream.
#[derive(Debug, Clone)]
pub enum Observation {
    /// A market observation. The strategy sees this.
    Tick(Tick),
    /// A depth update. The matcher reads it; the strategy is not called,
    /// because a book is a matcher's input and not a signal.
    Depth(Box<DepthUpdate>),
    /// The snapshot an incremental stream is sequenced against.
    Snapshot {
        /// The update id the snapshot was taken at. The first update
        /// applied must straddle it.
        update_id: u64,
        bids: Vec<Level>,
        asks: Vec<Level>,
    },
}

impl From<Tick> for Observation {
    fn from(t: Tick) -> Self {
        Self::Tick(t)
    }
}

impl From<DepthUpdate> for Observation {
    fn from(u: DepthUpdate) -> Self {
        Self::Depth(Box::new(u))
    }
}

impl Observation {
    /// The exchange time this arrival carries, for ordering a merge.
    ///
    /// A snapshot has none: it is a starting state rather than an event,
    /// and it belongs at the front of the stream regardless of when it
    /// was fetched.
    #[must_use]
    pub fn at_ms(&self) -> Option<i64> {
        match self {
            Self::Tick(t) => Some(t.stamp.exch.0 / 1_000_000),
            Self::Depth(u) => Some(u.event_ms),
            Self::Snapshot { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{Nanos, PriceTicks, QtyLots, Stamp};

    fn tick(ns: i64) -> Tick {
        Tick {
            stamp: Stamp {
                exch: Nanos(ns),
                local: Nanos(ns),
            },
            last: PriceTicks(100),
            high: PriceTicks(100),
            low: PriceTicks(100),
            bid: PriceTicks(99),
            ask: PriceTicks(101),
            volume: QtyLots(0),
        }
    }

    fn update(ms: i64) -> DepthUpdate {
        DepthUpdate {
            event_ms: ms,
            first_id: 1,
            final_id: 1,
            prev_final_id: None,
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }

    /// Both kinds report the same unit, or a merge sorting on it
    /// interleaves them wrongly — and the failure is silent, because a
    /// stream in the wrong order still runs.
    #[test]
    fn a_tick_and_an_update_report_time_in_the_same_unit() {
        assert_eq!(Observation::Tick(tick(5_000_000_000)).at_ms(), Some(5_000));
        assert_eq!(Observation::from(update(5_000)).at_ms(), Some(5_000));
    }

    /// A snapshot is a starting state, not an event. Giving it a time
    /// would let a merge sort it into the middle, where it wipes the
    /// book a hundred updates had built.
    #[test]
    fn a_snapshot_carries_no_time() {
        assert_eq!(
            Observation::Snapshot {
                update_id: 7,
                bids: Vec::new(),
                asks: Vec::new(),
            }
            .at_ms(),
            None
        );
    }
}
