//! Reference data that remembers what was believed, and when.
//!
//! Some inputs are not observations but *statements about the world*
//! that get revised: margin tables, contract specifications, index
//! constituents, corrected funding rates. Two different times matter
//! for each, and conflating them produces two different errors:
//!
//! - **Valid time** — when the fact applied. "The maintenance rate was
//!   0.5% from March onward."
//! - **Known time** — when we learned it. "We were told that in May."
//!
//! A backtest of March must use the March rate. A backtest of March
//! *run in April*, replayed today, must produce the same answer it
//! produced in April — even though we now know more. The first
//! requirement alone is satisfied by valid time. The second needs both,
//! and without it a "reproducible" run silently changes its answer
//! every time a vendor backfills a correction.
//!
//! This is the same failure as using today's index constituents to test
//! a strategy over last decade: the past is quietly rewritten into a
//! shape that flatters the result. Here it is made expressible instead
//! of unavoidable — [`Bitemporal::as_believed_at`] pins both axes, and
//! [`Bitemporal::current`] is the convenience for "everything we know
//! now", named so that using it in a reproducibility-critical path
//! reads as the choice it is.

use oq_types::Nanos;

/// One version of a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version<T> {
    /// When the fact started applying in the world.
    pub valid_from: Nanos,
    /// When we learned it.
    pub known_from: Nanos,
    pub value: T,
}

impl<T> Version<T> {
    #[must_use]
    pub const fn new(valid_from: Nanos, known_from: Nanos, value: T) -> Self {
        Self {
            valid_from,
            known_from,
            value,
        }
    }

    /// A fact learned at the moment it started applying.
    #[must_use]
    pub const fn immediate(at: Nanos, value: T) -> Self {
        Self {
            valid_from: at,
            known_from: at,
            value,
        }
    }
}

/// Versions of one fact over both time axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitemporal<T> {
    versions: Vec<Version<T>>,
}

impl<T> Default for Bitemporal<T> {
    fn default() -> Self {
        Self {
            versions: Vec::new(),
        }
    }
}

impl<T: Clone> Bitemporal<T> {
    #[must_use]
    pub fn new(versions: Vec<Version<T>>) -> Self {
        let mut this = Self { versions };
        this.sort();
        this
    }

    fn sort(&mut self) {
        // Valid time first, then known time: the query walks valid time
        // and picks the latest belief within it.
        self.versions
            .sort_by_key(|v| (v.valid_from.0, v.known_from.0));
    }

    pub fn insert(&mut self, version: Version<T>) {
        self.versions.push(version);
        self.sort();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.versions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    #[must_use]
    pub fn versions(&self) -> &[Version<T>] {
        &self.versions
    }

    /// What applied at `valid`, according to what was known at `known`.
    ///
    /// The query a reproducible backtest makes. `None` when nothing was
    /// known yet — never a fallback to the earliest version, which
    /// would fabricate a belief nobody held.
    #[must_use]
    pub fn as_believed_at(&self, valid: Nanos, known: Nanos) -> Option<&T> {
        self.versions
            .iter()
            .filter(|v| v.valid_from <= valid && v.known_from <= known)
            // Latest applicable version; ties on valid time resolve to
            // the later belief, which is the correction.
            .max_by_key(|v| (v.valid_from.0, v.known_from.0))
            .map(|v| &v.value)
    }

    /// What applied at `valid`, according to everything known now.
    ///
    /// Convenient and *not* reproducible across corrections. Named so
    /// that reaching for it in a path that must be reproducible reads
    /// as a decision rather than a default.
    #[must_use]
    pub fn current(&self, valid: Nanos) -> Option<&T> {
        self.as_believed_at(valid, Nanos(i64::MAX))
    }

    /// Every instant at which the answer changes on either axis.
    ///
    /// For a run that wants to assert it never straddled a revision it
    /// did not intend to.
    #[must_use]
    pub fn revisions(&self) -> Vec<(Nanos, Nanos)> {
        self.versions
            .iter()
            .map(|v| (v.valid_from, v.known_from))
            .collect()
    }

    /// Whether a correction was ever applied retroactively — learned
    /// after the fact it describes had already started applying.
    ///
    /// A dataset with none of these can be treated as single-temporal
    /// without loss, and knowing that is worth a cheap check.
    #[must_use]
    pub fn has_retroactive_corrections(&self) -> bool {
        self.versions.iter().any(|v| v.known_from > v.valid_from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: i64) -> Nanos {
        Nanos::from_secs(n * 86_400)
    }

    /// A rate that applied from March, plus a correction to it that
    /// only became known in May.
    fn corrected() -> Bitemporal<i64> {
        Bitemporal::new(vec![
            Version::immediate(day(0), 40),
            Version::new(day(60), day(60), 50),
            // In May we learned the March-onward figure was wrong.
            Version::new(day(60), day(120), 55),
        ])
    }

    #[test]
    fn a_run_from_april_still_answers_as_april_did() {
        // The property the whole module exists for: replaying an old
        // run today must not pick up a correction that did not exist
        // when it ran.
        let b = corrected();
        assert_eq!(b.as_believed_at(day(90), day(90)), Some(&50));
        assert_eq!(
            b.as_believed_at(day(90), day(200)),
            Some(&55),
            "a run today sees the correction"
        );
    }

    #[test]
    fn valid_time_selects_the_regime_in_force() {
        let b = corrected();
        assert_eq!(b.current(day(30)), Some(&40));
        assert_eq!(b.current(day(90)), Some(&55));
    }

    #[test]
    fn before_anything_was_known_the_answer_is_none() {
        let b = corrected();
        assert_eq!(b.as_believed_at(day(-1), day(1_000)), None);
        assert_eq!(
            b.as_believed_at(day(90), day(-1)),
            None,
            "nothing was known yet, which is an answer"
        );
    }

    #[test]
    fn retroactive_corrections_are_detectable() {
        assert!(corrected().has_retroactive_corrections());

        let clean = Bitemporal::new(vec![
            Version::immediate(day(0), 40),
            Version::immediate(day(60), 50),
        ]);
        assert!(!clean.has_retroactive_corrections());
    }

    #[test]
    fn insertion_order_does_not_change_answers() {
        let forward = corrected();
        let mut backward: Bitemporal<i64> = Bitemporal::default();
        for v in corrected().versions().iter().rev() {
            backward.insert(*v);
        }
        for d in [0, 30, 60, 90, 120, 200] {
            for k in [0, 60, 120, 200] {
                assert_eq!(
                    forward.as_believed_at(day(d), day(k)),
                    backward.as_believed_at(day(d), day(k)),
                    "valid={d} known={k}"
                );
            }
        }
    }

    #[test]
    fn an_empty_store_knows_nothing() {
        let b: Bitemporal<i64> = Bitemporal::default();
        assert!(b.is_empty());
        assert!(b.current(day(1)).is_none());
    }
}
