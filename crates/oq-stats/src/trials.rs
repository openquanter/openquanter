//! Trial registry.
//!
//! The deflated Sharpe ratio needs to know how many configurations were
//! tried and how dispersed their Sharpe ratios were. Both numbers are
//! easy to under-report by accident: abandoned sweeps, discarded
//! variants, and "just one more parameter" all count, and none of them
//! leave a trace unless something records them.
//!
//! This registry is that record. It is deliberately dumb — an honest
//! count kept next to the results — because the failure mode it guards
//! against is social, not computational.
//!
//! # It outlives the sweep
//!
//! A registry that lives only as long as one sweep counts one sweep.
//! The search that matters is every sweep run against the same question,
//! including the ones that were abandoned because they looked bad, so the
//! registry renders to a ledger ([`TrialRegistry::to_ledger`]) and reads
//! back from one ([`TrialRegistry::from_ledger`]). Where the ledger lives
//! is the caller's decision; that it is reloaded before the next sweep is
//! the whole point.
//!
//! # A trial that could not be scored still counts
//!
//! A configuration whose returns were too few to give a Sharpe ratio was
//! still tried. Leaving it out of `N` would make a search look narrower
//! than it was, by exactly the configurations that failed, so it is
//! recorded with [`TrialRegistry::record_unscored`] and counted in `N`.
//! It cannot contribute to the dispersion, which needs a Sharpe ratio.

use crate::dsr::deflated_sharpe_ratio;
use crate::{Result, StatsError};

/// The ledger format this build writes and reads.
pub const LEDGER_VERSION: &str = "1";

/// One evaluated configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    /// Caller-defined identifier, e.g. a parameter hash.
    pub id: String,
    /// Sharpe ratio at the frequency of the underlying returns.
    pub sharpe: f64,
    /// Number of return observations behind that Sharpe ratio.
    pub n_observations: usize,
    /// Skewness of the return series.
    pub skewness: f64,
    /// Non-excess kurtosis of the return series (3.0 = normal).
    pub kurtosis: f64,
}

/// Every configuration evaluated against one research question.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrialRegistry {
    trials: Vec<Trial>,
    unscored: Vec<String>,
    basis: Option<String>,
}

impl TrialRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one evaluated configuration.
    pub fn record(&mut self, trial: Trial) {
        self.trials.push(trial);
    }

    /// Record a configuration that was tried but produced no Sharpe ratio.
    pub fn record_unscored(&mut self, id: impl Into<String>) {
        self.unscored.push(id.into());
    }

    /// Number of trials, scored or not: the `N` the deflation uses.
    #[must_use]
    pub fn len(&self) -> usize {
        self.trials.len() + self.unscored.len()
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The trials that produced a Sharpe ratio.
    #[must_use]
    pub fn trials(&self) -> &[Trial] {
        &self.trials
    }

    /// The trials that did not.
    #[must_use]
    pub fn unscored(&self) -> &[String] {
        &self.unscored
    }

    /// What every Sharpe ratio in this registry was measured on — the
    /// sampling frequency, typically — or `None` before anything set it.
    ///
    /// Sharpe ratios at different frequencies are different numbers, and
    /// their variance is not a dispersion of anything. A caller that
    /// records into a registry checks this first.
    #[must_use]
    pub fn basis(&self) -> Option<&str> {
        self.basis.as_deref()
    }

    /// Name what the Sharpe ratios in this registry are measured on.
    pub fn set_basis(&mut self, basis: impl Into<String>) {
        self.basis = Some(basis.into());
    }

    /// Variance of the Sharpe ratios across trials, the dispersion term
    /// the deflated Sharpe ratio deflates by.
    #[must_use]
    pub fn sharpe_variance(&self) -> f64 {
        let n = self.trials.len();
        if n < 2 {
            return 0.0;
        }
        let n_f = n as f64;
        let mean = self.trials.iter().map(|t| t.sharpe).sum::<f64>() / n_f;
        self.trials
            .iter()
            .map(|t| (t.sharpe - mean).powi(2))
            .sum::<f64>()
            / (n_f - 1.0)
    }

    /// The trial with the highest Sharpe ratio, if any.
    #[must_use]
    pub fn best(&self) -> Option<&Trial> {
        self.trials.iter().max_by(|a, b| {
            a.sharpe
                .partial_cmp(&b.sharpe)
                .unwrap_or(core::cmp::Ordering::Equal)
        })
    }

    /// Deflated Sharpe ratio of the best trial, deflated by the full
    /// trial count and dispersion of this registry.
    ///
    /// This is the number to report: the best result *in the context of
    /// the search that produced it*.
    ///
    /// # Errors
    ///
    /// [`StatsError::TooFewObservations`] if nothing has been scored, or
    /// any error from [`deflated_sharpe_ratio`].
    pub fn deflated_sharpe_of_best(&self) -> Result<f64> {
        let best = self
            .best()
            .ok_or(StatsError::TooFewObservations { got: 0, need: 1 })?;
        self.deflated_sharpe_of(best)
    }

    /// Deflated Sharpe ratio of `trial`, deflated by everything this
    /// registry has seen — every sweep it was reloaded into, and every
    /// configuration that could not be scored.
    ///
    /// # Errors
    ///
    /// Any error from [`deflated_sharpe_ratio`].
    pub fn deflated_sharpe_of(&self, trial: &Trial) -> Result<f64> {
        deflated_sharpe_ratio(
            trial.sharpe,
            trial.n_observations,
            trial.skewness,
            trial.kurtosis,
            self.sharpe_variance(),
            self.len().max(1),
        )
    }

    /// The registry as a ledger: one trial per line, readable by eye and
    /// by [`TrialRegistry::from_ledger`].
    ///
    /// ```text
    /// openquanter-trials 1
    /// basis equity-every=64
    /// trial <sharpe>\t<observations>\t<skewness>\t<kurtosis>\t<id>
    /// unscored <id>
    /// ```
    ///
    /// The id comes last so it may hold anything but a line break; a tab
    /// or line break inside it is written as a space.
    #[must_use]
    pub fn to_ledger(&self) -> String {
        use core::fmt::Write as _;
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let mut out = String::new();
        let _ = writeln!(out, "openquanter-trials {LEDGER_VERSION}");
        if let Some(basis) = &self.basis {
            let _ = writeln!(out, "basis {}", clean(basis));
        }
        for t in &self.trials {
            // `Display` for f64 prints the shortest string that reads back
            // to the same value, so the ledger round-trips exactly.
            let _ = writeln!(
                out,
                "trial {}\t{}\t{}\t{}\t{}",
                t.sharpe,
                t.n_observations,
                t.skewness,
                t.kurtosis,
                clean(&t.id)
            );
        }
        for id in &self.unscored {
            let _ = writeln!(out, "unscored {}", clean(id));
        }
        out
    }

    /// Read a ledger written by [`TrialRegistry::to_ledger`].
    ///
    /// # Errors
    ///
    /// The version line is missing or another, or a line does not parse
    /// — named by its number. A ledger is refused whole rather than read
    /// in part: a partly read ledger is a smaller `N`, which is the error
    /// it exists to prevent.
    pub fn from_ledger(text: &str) -> core::result::Result<Self, String> {
        let mut lines = text.lines().enumerate();
        match lines.next() {
            Some((_, l)) if l.trim() == format!("openquanter-trials {LEDGER_VERSION}") => {}
            Some((_, l)) => return Err(format!("not a trial ledger this build reads: {l:?}")),
            None => return Err("empty".into()),
        }
        let mut registry = Self::new();
        for (i, line) in lines {
            let n = i + 1;
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "basis" => registry.basis = Some(rest.to_string()),
                "unscored" => registry.unscored.push(rest.to_string()),
                "trial" => {
                    let p: Vec<&str> = rest.splitn(5, '\t').collect();
                    if p.len() != 5 {
                        return Err(format!(
                            "line {n}: a trial has 5 fields, this has {}",
                            p.len()
                        ));
                    }
                    let num = |s: &str, what: &str| -> core::result::Result<f64, String> {
                        s.parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite())
                            .ok_or_else(|| format!("line {n}: {what} is not a finite number"))
                    };
                    registry.trials.push(Trial {
                        sharpe: num(p[0], "sharpe")?,
                        n_observations: p[1]
                            .parse()
                            .map_err(|_| format!("line {n}: observations is not a count"))?,
                        skewness: num(p[2], "skewness")?,
                        kurtosis: num(p[3], "kurtosis")?,
                        id: p[4].to_string(),
                    });
                }
                "" => {}
                other => return Err(format!("line {n}: unknown line {other:?}")),
            }
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(id: &str, sharpe: f64) -> Trial {
        Trial {
            id: id.to_string(),
            sharpe,
            n_observations: 500,
            skewness: 0.0,
            kurtosis: 3.0,
        }
    }

    #[test]
    fn tracks_count_and_dispersion() {
        let mut registry = TrialRegistry::new();
        assert!(registry.is_empty());
        for (i, sharpe) in [0.05, 0.10, 0.15, 0.20].iter().enumerate() {
            registry.record(trial(&format!("cfg-{i}"), *sharpe));
        }
        assert_eq!(registry.len(), 4);
        // Sample variance of {0.05, 0.10, 0.15, 0.20}.
        assert!((registry.sharpe_variance() - 0.004_166_666_666_666_667).abs() < 1e-15);
        assert_eq!(registry.best().unwrap().id, "cfg-3");
    }

    #[test]
    fn deflation_uses_the_whole_search_not_just_the_winner() {
        let mut narrow = TrialRegistry::new();
        narrow.record(trial("a", 0.12));
        narrow.record(trial("b", 0.10));
        narrow.record(trial("c", 0.11));

        let mut wide = narrow.clone();
        for i in 0..200 {
            wide.record(trial(&format!("x{i}"), 0.02 + (i % 17) as f64 * 0.004));
        }

        let narrow_dsr = narrow.deflated_sharpe_of_best().unwrap();
        let wide_dsr = wide.deflated_sharpe_of_best().unwrap();
        assert!(
            wide_dsr < narrow_dsr,
            "the same winner found in a wider search must deflate further: {wide_dsr} vs {narrow_dsr}"
        );
    }

    #[test]
    fn empty_registry_reports_an_error_rather_than_a_number() {
        let registry = TrialRegistry::new();
        assert_eq!(
            registry.deflated_sharpe_of_best(),
            Err(StatsError::TooFewObservations { got: 0, need: 1 })
        );
    }

    #[test]
    fn an_unscored_trial_counts_in_n_but_not_in_the_dispersion() {
        let mut scored = TrialRegistry::new();
        scored.record(trial("a", 0.12));
        scored.record(trial("b", 0.10));
        let mut with_failures = scored.clone();
        for i in 0..50 {
            with_failures.record_unscored(format!("dead-{i}"));
        }
        assert_eq!(with_failures.len(), 52);
        assert_eq!(with_failures.trials().len(), 2);
        assert_eq!(
            with_failures.sharpe_variance(),
            scored.sharpe_variance(),
            "a trial without a Sharpe ratio has nothing to add to its spread"
        );
        // The configurations that failed were still tried: dropping them
        // would make the search look narrower by exactly the failures.
        assert!(
            with_failures.deflated_sharpe_of_best().unwrap()
                < scored.deflated_sharpe_of_best().unwrap()
        );
    }

    #[test]
    fn only_unscored_trials_give_no_deflated_ratio() {
        let mut registry = TrialRegistry::new();
        registry.record_unscored("x");
        assert!(!registry.is_empty());
        assert!(registry.deflated_sharpe_of_best().is_err());
    }

    #[test]
    fn a_ledger_reads_back_exactly() {
        let mut registry = TrialRegistry::new();
        registry.set_basis("equity-every=64");
        registry.record(Trial {
            id: "fast=5\tslow=20".into(),
            sharpe: 0.123_456_789_012_345_67,
            n_observations: 812,
            skewness: -0.3,
            kurtosis: 4.25,
        });
        registry.record(trial("b", 1e-17));
        registry.record_unscored("c d");
        let text = registry.to_ledger();
        let back = TrialRegistry::from_ledger(&text).expect("reads");
        assert_eq!(back.basis(), Some("equity-every=64"));
        assert_eq!(
            back.trials()[0].id,
            "fast=5 slow=20",
            "a tab cannot break a line"
        );
        assert_eq!(back.trials()[0].sharpe, registry.trials()[0].sharpe);
        assert_eq!(back.trials()[1].sharpe, 1e-17);
        assert_eq!(back.unscored(), ["c d".to_string()]);
        assert_eq!(back.to_ledger(), text, "and writes the same again");
    }

    #[test]
    fn a_ledger_is_refused_whole_rather_than_read_in_part() {
        // A partly read ledger is a smaller N, the error it exists to stop.
        assert!(TrialRegistry::from_ledger("").is_err());
        assert!(TrialRegistry::from_ledger("openquanter-sweep 1\n").is_err());
        let bad = "openquanter-trials 1\ntrial 0.1\t10\t0\t3\ta\ntrial NaN\t10\t0\t3\tb\n";
        let err = TrialRegistry::from_ledger(bad).expect_err("NaN");
        assert!(err.contains("line 3"), "{err}");
        let err =
            TrialRegistry::from_ledger("openquanter-trials 1\nwinner a\n").expect_err("unknown");
        assert!(err.contains("line 2"), "{err}");
        assert!(
            TrialRegistry::from_ledger("openquanter-trials 1\n")
                .unwrap()
                .is_empty()
        );
    }
}
