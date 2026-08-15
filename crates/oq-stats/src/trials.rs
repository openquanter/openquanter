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

use crate::dsr::deflated_sharpe_ratio;
use crate::{Result, StatsError};

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

/// Every configuration evaluated in a sweep.
#[derive(Debug, Clone, Default)]
pub struct TrialRegistry {
    trials: Vec<Trial>,
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

    /// Number of trials recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.trials.len()
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.trials.is_empty()
    }

    /// All recorded trials.
    #[must_use]
    pub fn trials(&self) -> &[Trial] {
        &self.trials
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
    /// [`StatsError::TooFewObservations`] if the registry is empty, or
    /// any error from [`deflated_sharpe_ratio`].
    pub fn deflated_sharpe_of_best(&self) -> Result<f64> {
        let best = self
            .best()
            .ok_or(StatsError::TooFewObservations { got: 0, need: 1 })?;
        deflated_sharpe_ratio(
            best.sharpe,
            best.n_observations,
            best.skewness,
            best.kurtosis,
            self.sharpe_variance(),
            self.trials.len(),
        )
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
}
