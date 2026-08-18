//! The part worth having without migrating.
//!
//! D16 settles that Python is a binding rather than a compromise, and
//! that the first surface should be small — because whatever is exposed
//! first becomes the API and freezes hardest. This is that surface: the
//! statistics that tell a Python user something about the backtest they
//! already have, in the framework they already use.
//!
//! That choice is not caution. It is the shortest path to an outside
//! user, and an outside user is one of M3's four entry conditions and the
//! only one engineering time cannot buy. A binding that required
//! migrating a strategy first would be a binding nobody reaches.
//!
//! # What is deliberately absent
//!
//! The engine, the venue clients, the order path. Not because they could
//! not be bound, but because binding them would make this package the way
//! the framework is used, and the Rust-only path has to stay usable — the
//! third cost D16 names. This crate is excluded from the workspace's
//! default members for the same reason: building the engine must not
//! require a Python interpreter.
//!
//! # Errors become exceptions with their reasons intact
//!
//! Every refusal in `oq-stats` says why — too few observations, zero
//! variance, a matrix that does not split. Those reasons are the useful
//! part, and collapsing them into `ValueError("bad input")` would throw
//! away the half a user acts on.

pub mod tier;

pub mod pure {
    //! The substance, with no FFI in it.
    //!
    //! Every function here returns `Result<_, String>` and knows nothing
    //! about Python. The bindings below are thin enough to read at a
    //! glance, which is the point: a wrapper that contained logic would be
    //! logic testable only with an interpreter loaded, and the interesting
    //! cases here are refusals rather than plumbing.

    /// Sharpe ratio at the frequency of the returns, not annualised.
    ///
    /// Annualising needs the period, and a function that guessed would
    /// return a number whose scale rests on an assumption the caller never
    /// made.
    pub fn sharpe_ratio(returns: &[f64]) -> Result<f64, String> {
        oq_stats::moments::sharpe_ratio(returns).map_err(|e| e.to_string())
    }

    /// Sharpe ratio, skewness and kurtosis from one pass.
    ///
    /// Together because the deflated Sharpe ratio needs all three, and
    /// three separate calls are three chances to pass a different series.
    pub fn moments(returns: &[f64]) -> Result<(f64, f64, f64), String> {
        let m = oq_stats::Moments::from_returns(returns).map_err(|e| e.to_string())?;
        Ok((m.sharpe_ratio(), m.skewness, m.kurtosis))
    }

    pub fn probabilistic_sharpe_ratio(
        observed: f64,
        benchmark: f64,
        n_observations: usize,
        skewness: f64,
        kurtosis: f64,
    ) -> Result<f64, String> {
        oq_stats::probabilistic_sharpe_ratio(
            observed,
            benchmark,
            n_observations,
            skewness,
            kurtosis,
        )
        .map_err(|e| e.to_string())
    }

    /// The best Sharpe ratio in a search, minus what the searching bought.
    ///
    /// The whole set is needed, not just the winner: the deflation depends
    /// on how much the candidates varied, so a single value would deflate
    /// by nothing and report the number the search already flattered.
    pub fn deflated_sharpe_ratio(
        all_sharpes: &[f64],
        n_observations: usize,
        skewness: f64,
        kurtosis: f64,
    ) -> Result<f64, String> {
        if all_sharpes.len() < 2 {
            return Err(
                "deflation needs more than one configuration: with one candidate \
                        there was no search to deflate, and the answer is the plain \
                        Sharpe ratio"
                    .to_string(),
            );
        }
        let best = all_sharpes.iter().copied().fold(f64::MIN, f64::max);
        let mean = all_sharpes.iter().sum::<f64>() / all_sharpes.len() as f64;
        let variance = all_sharpes.iter().map(|s| (s - mean).powi(2)).sum::<f64>()
            / (all_sharpes.len() - 1) as f64;
        oq_stats::deflated_sharpe_ratio(
            best,
            n_observations,
            skewness,
            kurtosis,
            variance,
            all_sharpes.len(),
        )
        .map_err(|e| e.to_string())
    }

    /// How often the best in-sample configuration underperforms out of
    /// sample.
    ///
    /// Unequal columns are refused rather than truncated: truncation is a
    /// decision about which data to discard, and it belongs to whoever
    /// knows what the rows mean.
    pub fn probability_of_backtest_overfitting(
        columns: &[Vec<f64>],
        n_blocks: usize,
    ) -> Result<f64, String> {
        if columns.len() < 2 {
            return Err(
                "overfitting is a statement about a choice among alternatives; \
                        with one configuration there was no choice"
                    .to_string(),
            );
        }
        let first = columns[0].len();
        if let Some(bad) = columns.iter().position(|c| c.len() != first) {
            return Err(format!(
                "column {bad} has {} returns and column 0 has {first}; equalise them \
                 deliberately rather than having this truncate for you",
                columns[bad].len()
            ));
        }
        let matrix =
            oq_stats::pbo::PerformanceMatrix::from_columns(columns).map_err(|e| e.to_string())?;
        oq_stats::probability_of_backtest_overfitting(&matrix, n_blocks)
            .map(|r| r.pbo)
            .map_err(|e| e.to_string())
    }

    pub fn minimum_track_record_length(
        observed: f64,
        benchmark: f64,
        skewness: f64,
        kurtosis: f64,
        confidence: f64,
    ) -> Result<f64, String> {
        oq_stats::dsr::minimum_track_record_length(
            observed, benchmark, skewness, kurtosis, confidence,
        )
        .map_err(|e| e.to_string())
    }
}

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn to_py(e: String) -> PyErr {
    PyValueError::new_err(e)
}

#[pyfunction]
fn sharpe_ratio(returns: Vec<f64>) -> PyResult<f64> {
    pure::sharpe_ratio(&returns).map_err(to_py)
}

#[pyfunction]
fn moments(returns: Vec<f64>) -> PyResult<(f64, f64, f64)> {
    pure::moments(&returns).map_err(to_py)
}

#[pyfunction]
#[pyo3(signature = (observed, benchmark, n_observations, skewness, kurtosis))]
fn probabilistic_sharpe_ratio(
    observed: f64,
    benchmark: f64,
    n_observations: usize,
    skewness: f64,
    kurtosis: f64,
) -> PyResult<f64> {
    pure::probabilistic_sharpe_ratio(observed, benchmark, n_observations, skewness, kurtosis)
        .map_err(to_py)
}

#[pyfunction]
#[pyo3(signature = (all_sharpes, n_observations, skewness, kurtosis))]
fn deflated_sharpe_ratio(
    all_sharpes: Vec<f64>,
    n_observations: usize,
    skewness: f64,
    kurtosis: f64,
) -> PyResult<f64> {
    pure::deflated_sharpe_ratio(&all_sharpes, n_observations, skewness, kurtosis).map_err(to_py)
}

#[pyfunction]
#[pyo3(signature = (columns, n_blocks = 16))]
fn probability_of_backtest_overfitting(columns: Vec<Vec<f64>>, n_blocks: usize) -> PyResult<f64> {
    pure::probability_of_backtest_overfitting(&columns, n_blocks).map_err(to_py)
}

#[pyfunction]
#[pyo3(signature = (observed, benchmark, skewness, kurtosis, confidence = 0.95))]
fn minimum_track_record_length(
    observed: f64,
    benchmark: f64,
    skewness: f64,
    kurtosis: f64,
    confidence: f64,
) -> PyResult<f64> {
    pure::minimum_track_record_length(observed, benchmark, skewness, kurtosis, confidence)
        .map_err(to_py)
}

#[pymodule]
fn openquanter(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__doc__", "Evaluate the backtest you already have.")?;
    m.add_function(wrap_pyfunction!(sharpe_ratio, m)?)?;
    m.add_function(wrap_pyfunction!(moments, m)?)?;
    m.add_function(wrap_pyfunction!(probabilistic_sharpe_ratio, m)?)?;
    m.add_function(wrap_pyfunction!(deflated_sharpe_ratio, m)?)?;
    m.add_function(wrap_pyfunction!(probability_of_backtest_overfitting, m)?)?;
    m.add_function(wrap_pyfunction!(minimum_track_record_length, m)?)?;
    crate::tier::register(m)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pure::*;

    /// A return series with dispersion, since a constant one has no
    /// Sharpe ratio and every statistic here would refuse it.
    fn series(n: usize) -> Vec<f64> {
        (0..n).map(|i| 0.01 * ((i % 7) as f64) - 0.02).collect()
    }

    #[test]
    fn a_sharpe_ratio_comes_back_unannualised() {
        // Annualising needs the period, and a function that guessed would
        // return a number whose scale depends on an assumption the caller
        // never made.
        let s = sharpe_ratio(&series(100)).expect("computed");
        assert!(s.is_finite(), "{s}");
        assert!(s.abs() < 10.0, "an unannualised per-period figure: {s}");
    }

    #[test]
    fn the_three_moments_come_from_one_pass() {
        // Returned together because the deflated Sharpe ratio needs all
        // three, and three separate calls are three chances to pass a
        // different series.
        let (sharpe, skew, kurt) = moments(&series(100)).expect("computed");
        assert!(sharpe.is_finite() && skew.is_finite() && kurt.is_finite());
        let alone = sharpe_ratio(&series(100)).expect("computed");
        assert!((sharpe - alone).abs() < 1e-12, "the same number either way");
    }

    #[test]
    fn deflation_refuses_a_search_of_one() {
        // With one candidate there was no search to deflate, and returning
        // the plain Sharpe ratio under the deflated name would be the one
        // number this package exists to stop people quoting.
        let err = deflated_sharpe_ratio(&[1.5], 100, 0.0, 3.0).expect_err("one config");
        assert!(err.contains("no search to deflate"), "{err}");
    }

    #[test]
    fn deflation_lowers_the_best_result_of_a_wide_search() {
        // The whole point. Twenty candidates, the best of them flattered
        // by having been chosen; the deflated figure has to be below the
        // probability the plain one would claim.
        // The numbers matter: a strong Sharpe over many observations puts
        // both probabilities at 1.0 and the comparison cannot discriminate,
        // which is how the first version of this test passed vacuously.
        // These leave headroom on both sides.
        let sharpes: Vec<f64> = (0..20).map(|i| 0.05 + 0.013 * f64::from(i)).collect();
        let best = *sharpes.last().expect("non-empty");
        let deflated = deflated_sharpe_ratio(&sharpes, 100, 0.0, 3.0).expect("computed");
        let undeflated = probabilistic_sharpe_ratio(best, 0.0, 100, 0.0, 3.0).expect("computed");
        assert!(
            undeflated < 0.9999,
            "the fixture must not saturate: {undeflated}"
        );
        assert!(
            deflated < undeflated,
            "deflated {deflated} should be below undeflated {undeflated}"
        );
        assert!((0.0..=1.0).contains(&deflated), "a probability: {deflated}");
    }

    #[test]
    fn pbo_refuses_one_configuration_with_the_reason() {
        let err = probability_of_backtest_overfitting(&[series(100)], 16).expect_err("one config");
        assert!(err.contains("no choice"), "{err}");
    }

    #[test]
    fn pbo_refuses_unequal_columns_rather_than_truncating_them() {
        // Truncation is a decision about which data to discard, and it
        // belongs to whoever knows what the rows mean.
        let err = probability_of_backtest_overfitting(&[series(100), series(60)], 16)
            .expect_err("unequal");
        let text = err.clone();
        assert!(text.contains("column 1 has 60"), "{text}");
        assert!(text.contains("deliberately"), "{text}");
    }

    #[test]
    fn pbo_computes_for_a_well_formed_set() {
        let columns: Vec<Vec<f64>> = (0..4)
            .map(|k| {
                (0..120)
                    .map(|i| 0.01 * ((i + k) % (5 + k)) as f64 - 0.02)
                    .collect()
            })
            .collect();
        let pbo = probability_of_backtest_overfitting(&columns, 16).expect("computed");
        assert!((0.0..=1.0).contains(&pbo), "a probability: {pbo}");
    }

    #[test]
    fn a_refusal_keeps_the_reason_it_was_given() {
        // Every refusal in oq-stats says why, and the reason is the half a
        // user acts on. Collapsing them into one message would throw it
        // away.
        let err = sharpe_ratio(&[0.01, 0.01, 0.01, 0.01]).expect_err("no variance");
        let text = err.clone();
        assert!(text.contains("variance"), "{text}");
        let short = sharpe_ratio(&[0.01]).expect_err("too few");
        assert!(short.contains("observations"), "{short}");
    }
}
