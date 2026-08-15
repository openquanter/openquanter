//! Margin rules, resolved by when the event happened.
//!
//! Venues revise their maintenance-margin tables. A backtest over 2024
//! must use the table that was in force in 2024, and — separately — a
//! backtest run today must produce the same answer as the same backtest
//! run last year. Those are two different requirements and both are
//! satisfied by keying tables on their effective date and resolving
//! against the *event's* timestamp rather than the run's.
//!
//! Applying today's rules to old data is the same family of error as
//! survivorship bias: it rewrites the past into a shape that flatters
//! the strategy, and it does so silently. A leveraged position that
//! would have been liquidated under the rules of its own time survives
//! under looser modern ones, and the equity curve reports a recovery
//! that never happened.
//!
//! The schedule is *bitemporal* in the sense that matters here: a table
//! has an effective time (when the venue applied it) and the schedule
//! is queried by event time. When the correction itself needs to be
//! audited — "what did we believe the 2024 table was, when we ran this
//! in 2025?" — the answer is the schedule as it existed in that run's
//! configuration hash, which the parity manifest already records.

use crate::tier::TierTable;
use oq_types::Nanos;

/// Margin tables and the instants they took effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSchedule {
    /// Ordered by `effective_from`, ascending.
    entries: Vec<(Nanos, TierTable)>,
}

impl TierSchedule {
    /// A schedule with one table that has always applied.
    ///
    /// For instruments whose rules have not changed inside the window
    /// being tested, and for tests. Named `constant` rather than
    /// `new` so that the single-table case reads as an assertion the
    /// author made rather than a default they accepted.
    #[must_use]
    pub fn constant(table: TierTable) -> Self {
        Self {
            entries: vec![(Nanos(i64::MIN), table)],
        }
    }

    /// A schedule from dated tables.
    ///
    /// Returns `None` if empty, or if two tables claim the same
    /// effective instant — an ambiguity that would make the resolved
    /// table depend on input order, and therefore make the run
    /// irreproducible.
    #[must_use]
    pub fn new(mut entries: Vec<(Nanos, TierTable)>) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        entries.sort_by_key(|(at, _)| at.0);
        if entries.windows(2).any(|w| w[0].0 == w[1].0) {
            return None;
        }
        Some(Self { entries })
    }

    /// Add a revision.
    ///
    /// Returns `false` if a table already takes effect at that instant.
    pub fn insert(&mut self, effective_from: Nanos, table: TierTable) -> bool {
        if self.entries.iter().any(|(at, _)| *at == effective_from) {
            return false;
        }
        let at = self
            .entries
            .partition_point(|(existing, _)| *existing < effective_from);
        self.entries.insert(at, (effective_from, table));
        true
    }

    /// The table in force at `when`.
    ///
    /// `None` when `when` precedes every table in the schedule. That is
    /// deliberately not "fall back to the earliest table": a backtest
    /// reaching outside the rules it was given should stop, not
    /// silently extrapolate a margin regime backwards.
    #[must_use]
    pub fn at(&self, when: Nanos) -> Option<&TierTable> {
        let idx = self.entries.partition_point(|(at, _)| *at <= when);
        if idx == 0 {
            return None;
        }
        Some(&self.entries[idx - 1].1)
    }

    /// The instants at which the rules change.
    #[must_use]
    pub fn revisions(&self) -> Vec<Nanos> {
        self.entries.iter().map(|(at, _)| *at).collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::{MarginTier, TierTable};
    use oq_types::{Cash, Ratio};

    fn table(rate_ppm: i64) -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_ppm(rate_ppm),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    fn day(n: i64) -> Nanos {
        Nanos::from_secs(n * 86_400)
    }

    #[test]
    fn a_constant_schedule_answers_for_any_time() {
        let s = TierSchedule::constant(table(4_000));
        assert!(s.at(day(-10_000)).is_some());
        assert!(s.at(day(10_000)).is_some());
    }

    #[test]
    fn the_table_in_force_is_the_latest_one_at_or_before_the_event() {
        let s = TierSchedule::new(vec![
            (day(100), table(4_000)),
            (day(200), table(5_000)),
            (day(300), table(6_000)),
        ])
        .expect("distinct dates");

        assert_eq!(
            s.at(day(150)).expect("in force").tiers()[0].rate,
            Ratio::from_ppm(4_000)
        );
        assert_eq!(
            s.at(day(200)).expect("in force").tiers()[0].rate,
            Ratio::from_ppm(5_000),
            "a revision applies from its effective instant, inclusive"
        );
        assert_eq!(
            s.at(day(999)).expect("in force").tiers()[0].rate,
            Ratio::from_ppm(6_000)
        );
    }

    #[test]
    fn before_the_first_table_there_is_no_answer_rather_than_a_guess() {
        let s = TierSchedule::new(vec![(day(100), table(4_000))]).expect("one entry");
        assert!(
            s.at(day(99)).is_none(),
            "extrapolating a margin regime backwards is how old backtests get flattered"
        );
    }

    #[test]
    fn duplicate_effective_instants_are_refused() {
        let dup = vec![(day(100), table(4_000)), (day(100), table(5_000))];
        assert!(TierSchedule::new(dup).is_none());

        let mut s = TierSchedule::new(vec![(day(100), table(4_000))]).expect("one entry");
        assert!(!s.insert(day(100), table(9_000)));
        assert!(s.insert(day(101), table(9_000)));
    }

    #[test]
    fn unsorted_input_is_ordered() {
        let s = TierSchedule::new(vec![
            (day(300), table(6_000)),
            (day(100), table(4_000)),
            (day(200), table(5_000)),
        ])
        .expect("distinct dates");
        assert_eq!(s.revisions(), vec![day(100), day(200), day(300)]);
    }

    #[test]
    fn the_same_query_is_stable_across_runs() {
        // The reproducibility requirement, stated as a test: resolving
        // by event time means the answer does not depend on when the
        // run happens.
        let s = TierSchedule::new(vec![(day(100), table(4_000)), (day(200), table(5_000))])
            .expect("distinct dates");
        let answer_now = s.at(day(150)).expect("in force").clone();
        let answer_later = s.at(day(150)).expect("in force").clone();
        assert_eq!(answer_now, answer_later);
    }
}
