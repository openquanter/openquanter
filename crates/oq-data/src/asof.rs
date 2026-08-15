//! As-of joins that cannot leak the future.
//!
//! Attaching a feature to an observation means answering "what was
//! known at this instant?" Get the boundary wrong by one record and a
//! backtest reads a value that did not exist yet. The resulting
//! strategy looks excellent and is unreproducible in production, and
//! the failure is silent: nothing crashes, no test goes red, the equity
//! curve simply bends upward.
//!
//! Two rules make that impossible here rather than unlikely.
//!
//! **Strictly before, by default.** [`AsOf::Strict`] matches the last
//! record with `t < query`. A record stamped at exactly the decision
//! instant is *not* available: at nanosecond resolution an exact tie
//! means the two events were simultaneous, and a decision cannot
//! consume a value published in the same instant it is made. Several
//! widely used dataframe libraries default to `<=` here, which is why
//! this is spelled out rather than assumed — the difference is one
//! record and it is the difference between research and fiction.
//!
//! **Arrival time, not event time.** A record is joinable when it
//! *arrived*, not when the venue says it happened. A funding rate
//! stamped 08:00 that reached the process at 08:00.4 was not knowable
//! at 08:00.2, and joining on the venue's timestamp would hand the
//! strategy four hundred milliseconds of foresight. See
//! [`Timeline::Arrival`].

use oq_types::Nanos;

/// Which boundary an as-of join uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AsOf {
    /// `t < query`. The default, and the only safe choice for anything
    /// a decision consumes.
    #[default]
    Strict,
    /// `t <= query`. Correct only for reconstructing state *after* the
    /// fact — an end-of-day report, an audit — never for a decision.
    Inclusive,
}

/// Which clock a join reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timeline {
    /// When the record reached this process. What was knowable.
    #[default]
    Arrival,
    /// When the venue says the event happened. Later than arrival for
    /// anything that travelled, so joining on it grants foresight.
    Event,
}

/// A value with both timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timed<T> {
    /// When the venue says it happened.
    pub event: Nanos,
    /// When this process observed it.
    pub arrival: Nanos,
    pub value: T,
}

impl<T> Timed<T> {
    #[must_use]
    pub const fn new(event: Nanos, arrival: Nanos, value: T) -> Self {
        Self {
            event,
            arrival,
            value,
        }
    }

    /// A value whose arrival is unknown, treated as arriving when it
    /// happened.
    ///
    /// Only correct for data that never travelled — a locally computed
    /// value, a rule table read from disk. Using it for venue data
    /// throws away the feed latency and makes the join optimistic.
    #[must_use]
    pub const fn local(at: Nanos, value: T) -> Self {
        Self {
            event: at,
            arrival: at,
            value,
        }
    }

    const fn stamp(&self, timeline: Timeline) -> Nanos {
        match timeline {
            Timeline::Arrival => self.arrival,
            Timeline::Event => self.event,
        }
    }
}

/// A time-ordered series that can be asked what was known when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series<T> {
    records: Vec<Timed<T>>,
    timeline: Timeline,
}

impl<T> Default for Series<T> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            timeline: Timeline::Arrival,
        }
    }
}

impl<T: Clone> Series<T> {
    /// Build a series, sorting by the timeline it will be queried on.
    #[must_use]
    pub fn new(mut records: Vec<Timed<T>>, timeline: Timeline) -> Self {
        records.sort_by_key(|r| r.stamp(timeline).0);
        Self { records, timeline }
    }

    /// A series on arrival time, the safe default.
    #[must_use]
    pub fn by_arrival(records: Vec<Timed<T>>) -> Self {
        Self::new(records, Timeline::Arrival)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub fn records(&self) -> &[Timed<T>] {
        &self.records
    }

    /// The value in force at `query`.
    ///
    /// `None` when nothing was known yet — which is a real answer, not
    /// a missing one. Falling back to the first record would fabricate
    /// knowledge the process did not have, and is the same family of
    /// error as extrapolating a rule table backwards.
    #[must_use]
    pub fn as_of(&self, query: Nanos, mode: AsOf) -> Option<&Timed<T>> {
        let idx = match mode {
            AsOf::Strict => self
                .records
                .partition_point(|r| r.stamp(self.timeline) < query),
            AsOf::Inclusive => self
                .records
                .partition_point(|r| r.stamp(self.timeline) <= query),
        };
        if idx == 0 {
            return None;
        }
        self.records.get(idx - 1)
    }

    /// The value in force at `query`, using the safe boundary.
    #[must_use]
    pub fn known_at(&self, query: Nanos) -> Option<&Timed<T>> {
        self.as_of(query, AsOf::Strict)
    }

    /// Attach the value in force to each of `queries`.
    ///
    /// The shape a feature join takes: one row per decision instant,
    /// each carrying what was knowable then.
    #[must_use]
    pub fn join(&self, queries: &[Nanos], mode: AsOf) -> Vec<Option<T>> {
        queries
            .iter()
            .map(|q| self.as_of(*q, mode).map(|r| r.value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> Series<i64> {
        Series::by_arrival(vec![
            Timed::local(Nanos(10), 100),
            Timed::local(Nanos(20), 200),
            Timed::local(Nanos(30), 300),
        ])
    }

    #[test]
    fn strict_refuses_a_record_stamped_at_the_decision_instant() {
        // The whole point of the module, as one assertion.
        let s = series();
        assert_eq!(s.as_of(Nanos(20), AsOf::Strict).map(|r| r.value), Some(100));
        assert_eq!(
            s.as_of(Nanos(20), AsOf::Inclusive).map(|r| r.value),
            Some(200),
            "inclusive is for after-the-fact reconstruction only"
        );
    }

    #[test]
    fn nothing_known_yet_is_none_not_the_first_record() {
        let s = series();
        assert!(s.known_at(Nanos(10)).is_none());
        assert!(s.known_at(Nanos(0)).is_none());
        assert_eq!(s.known_at(Nanos(11)).map(|r| r.value), Some(100));
    }

    #[test]
    fn queries_past_the_end_hold_the_last_value() {
        let s = series();
        assert_eq!(s.known_at(Nanos(1_000)).map(|r| r.value), Some(300));
    }

    #[test]
    fn arrival_and_event_timelines_disagree_where_it_matters() {
        // A funding rate stamped 08:00 that arrived at 08:00.4 was not
        // knowable at 08:00.2. Joining on event time would grant the
        // strategy foresight; the two timelines must give different
        // answers, and the arrival one must be the conservative answer.
        let record = Timed::new(Nanos(800), Nanos(804), 42);
        let by_arrival = Series::new(vec![record], Timeline::Arrival);
        let by_event = Series::new(vec![record], Timeline::Event);

        let decision = Nanos(802);
        assert!(
            by_arrival.known_at(decision).is_none(),
            "it had not arrived yet"
        );
        assert_eq!(
            by_event.known_at(decision).map(|r| r.value),
            Some(42),
            "event time would have handed it over early"
        );
    }

    #[test]
    fn unsorted_input_is_ordered_on_the_queried_timeline() {
        let s = Series::by_arrival(vec![
            Timed::local(Nanos(30), 300),
            Timed::local(Nanos(10), 100),
            Timed::local(Nanos(20), 200),
        ]);
        assert_eq!(s.known_at(Nanos(25)).map(|r| r.value), Some(200));
    }

    #[test]
    fn a_join_produces_one_answer_per_query() {
        let s = series();
        let queries = vec![Nanos(5), Nanos(15), Nanos(25), Nanos(35)];
        assert_eq!(
            s.join(&queries, AsOf::Strict),
            vec![None, Some(100), Some(200), Some(300)]
        );
    }

    #[test]
    fn an_empty_series_knows_nothing() {
        let s: Series<i64> = Series::default();
        assert!(s.is_empty());
        assert!(s.known_at(Nanos(100)).is_none());
    }

    #[test]
    fn ties_on_arrival_resolve_to_the_last_inserted() {
        // Simultaneous arrivals are possible at any clock resolution.
        // The answer must be deterministic; sort stability makes it the
        // later-inserted record, which is the one a stream would have
        // processed second.
        let s = Series::by_arrival(vec![Timed::local(Nanos(10), 1), Timed::local(Nanos(10), 2)]);
        assert_eq!(s.known_at(Nanos(11)).map(|r| r.value), Some(2));
    }
}
