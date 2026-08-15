//! Probabilistic and deflated Sharpe ratios.
//!
//! The observed Sharpe ratio of the best strategy in a sweep is a biased
//! estimate of its true Sharpe ratio: with enough trials, a good-looking
//! number appears even when every strategy is worthless. The deflated
//! Sharpe ratio corrects for that by asking a sharper question — what is
//! the probability that the true Sharpe ratio exceeds the *highest value
//! you would expect from chance alone*, given how many strategies you
//! tried and how dispersed their results were.
//!
//! Two conventions matter and are easy to get wrong:
//!
//! - Every Sharpe ratio here is at the **frequency of the observations**.
//!   Feed daily returns and you get a daily Sharpe ratio; annualizing
//!   before deflation inflates the result.
//! - Kurtosis is **non-excess** (3.0 for a normal sample), matching
//!   [`crate::moments::Moments`].
//!
//! References: Bailey & López de Prado, "The Sharpe Ratio Efficient
//! Frontier" (2012) and "The Deflated Sharpe Ratio" (2014).

use crate::normal::{cdf, inverse_cdf};
use crate::{Result, StatsError};

/// Euler–Mascheroni constant, used in the expected maximum of `n` draws.
const EULER_MASCHERONI: f64 = 0.577_215_664_901_532_9;

/// Probabilistic Sharpe ratio: `P(true SR > benchmark SR)`.
///
/// `sharpe` and `benchmark` are at the frequency of the observations.
/// `skewness` and `kurtosis` describe the return distribution; pass
/// `0.0` and `3.0` for the normal case.
///
/// # Errors
///
/// [`StatsError::TooFewObservations`] below two observations,
/// [`StatsError::NotFinite`] for non-finite inputs, and
/// [`StatsError::ZeroVariance`] if the adjustment term collapses, which
/// happens only for degenerate moment combinations.
pub fn probabilistic_sharpe_ratio(
    sharpe: f64,
    benchmark: f64,
    n_observations: usize,
    skewness: f64,
    kurtosis: f64,
) -> Result<f64> {
    if n_observations < 2 {
        return Err(StatsError::TooFewObservations {
            got: n_observations,
            need: 2,
        });
    }
    for (value, name) in [
        (sharpe, "sharpe"),
        (benchmark, "benchmark"),
        (skewness, "skewness"),
        (kurtosis, "kurtosis"),
    ] {
        if !value.is_finite() {
            return Err(StatsError::NotFinite(name));
        }
    }

    // Variance of the Sharpe ratio estimator under non-normal returns.
    let variance = 1.0 - skewness * sharpe + (kurtosis - 1.0) / 4.0 * sharpe * sharpe;
    if variance <= 0.0 {
        return Err(StatsError::ZeroVariance);
    }

    let z = (sharpe - benchmark) * ((n_observations - 1) as f64).sqrt() / variance.sqrt();
    Ok(cdf(z))
}

/// Expected maximum Sharpe ratio across `n_trials` independent trials
/// whose true Sharpe ratios are all zero.
///
/// This is the bar an observed Sharpe ratio has to clear before it means
/// anything: the best of many coin flips still looks impressive.
///
/// `sharpe_variance` is the variance of the Sharpe ratios *across the
/// trials* — the dispersion of the sweep itself. With a single trial
/// there is nothing to select from and the expected maximum is zero.
#[must_use]
pub fn expected_max_sharpe(sharpe_variance: f64, n_trials: usize) -> f64 {
    if n_trials <= 1 || sharpe_variance <= 0.0 {
        return 0.0;
    }

    let n = n_trials as f64;
    let q1 = inverse_cdf(1.0 - 1.0 / n);
    let q2 = inverse_cdf(1.0 - 1.0 / (n * core::f64::consts::E));

    sharpe_variance.sqrt() * ((1.0 - EULER_MASCHERONI) * q1 + EULER_MASCHERONI * q2)
}

/// Deflated Sharpe ratio: the probabilistic Sharpe ratio measured
/// against the expected maximum of the trials that produced it.
///
/// Read it as a probability that the strategy is real. Values below
/// roughly 0.95 mean the observed performance is not distinguishable
/// from the best of a lucky search.
///
/// # Errors
///
/// As [`probabilistic_sharpe_ratio`].
pub fn deflated_sharpe_ratio(
    sharpe: f64,
    n_observations: usize,
    skewness: f64,
    kurtosis: f64,
    sharpe_variance_across_trials: f64,
    n_trials: usize,
) -> Result<f64> {
    let benchmark = expected_max_sharpe(sharpe_variance_across_trials, n_trials);
    probabilistic_sharpe_ratio(sharpe, benchmark, n_observations, skewness, kurtosis)
}

/// Minimum track record length: the number of observations needed for
/// the probabilistic Sharpe ratio to reach `confidence`.
///
/// Answers "how long must I run this before the result means anything?"
///
/// # Errors
///
/// [`StatsError::NotFinite`] for non-finite inputs. Returns
/// [`StatsError::ZeroVariance`] when the observed Sharpe ratio does not
/// exceed the benchmark, in which case no track record length suffices.
pub fn minimum_track_record_length(
    sharpe: f64,
    benchmark: f64,
    skewness: f64,
    kurtosis: f64,
    confidence: f64,
) -> Result<f64> {
    for (value, name) in [
        (sharpe, "sharpe"),
        (benchmark, "benchmark"),
        (skewness, "skewness"),
        (kurtosis, "kurtosis"),
        (confidence, "confidence"),
    ] {
        if !value.is_finite() {
            return Err(StatsError::NotFinite(name));
        }
    }
    if sharpe <= benchmark {
        return Err(StatsError::ZeroVariance);
    }

    let variance = 1.0 - skewness * sharpe + (kurtosis - 1.0) / 4.0 * sharpe * sharpe;
    if variance <= 0.0 {
        return Err(StatsError::ZeroVariance);
    }

    let z = inverse_cdf(confidence);
    Ok(1.0 + variance * (z / (sharpe - benchmark)).powi(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psr_is_one_half_when_sharpe_equals_benchmark() {
        let p = probabilistic_sharpe_ratio(1.0, 1.0, 100, 0.0, 3.0).unwrap();
        assert!((p - 0.5).abs() < 1e-12);
    }

    #[test]
    fn psr_matches_hand_computed_normal_case() {
        // sharpe 0.1, benchmark 0, 101 observations, normal returns.
        // variance = 1 + 0.5 * 0.01 = 1.005
        // z = 0.1 * sqrt(100) / sqrt(1.005) = 0.997509...
        // Phi(0.9975093) = 0.8407413...
        let p = probabilistic_sharpe_ratio(0.1, 0.0, 101, 0.0, 3.0).unwrap();
        let z = 0.1 * 100.0_f64.sqrt() / 1.005_f64.sqrt();
        assert!((p - cdf(z)).abs() < 1e-15);
        assert!((p - 0.840_741_3).abs() < 1e-6, "psr = {p}");
    }

    #[test]
    fn psr_rises_with_track_record_length() {
        let short = probabilistic_sharpe_ratio(0.1, 0.0, 50, 0.0, 3.0).unwrap();
        let long = probabilistic_sharpe_ratio(0.1, 0.0, 500, 0.0, 3.0).unwrap();
        assert!(long > short, "more observations must raise confidence");
    }

    #[test]
    fn negative_skew_and_fat_tails_lower_confidence() {
        let normal = probabilistic_sharpe_ratio(0.2, 0.0, 250, 0.0, 3.0).unwrap();
        let skewed = probabilistic_sharpe_ratio(0.2, 0.0, 250, -1.5, 3.0).unwrap();
        let fat = probabilistic_sharpe_ratio(0.2, 0.0, 250, 0.0, 9.0).unwrap();
        assert!(skewed < normal, "negative skew must reduce the PSR");
        assert!(fat < normal, "excess kurtosis must reduce the PSR");
    }

    #[test]
    fn expected_max_sharpe_grows_with_trials() {
        let v = 0.25;
        let one = expected_max_sharpe(v, 1);
        let ten = expected_max_sharpe(v, 10);
        let thousand = expected_max_sharpe(v, 1_000);
        assert_eq!(one, 0.0, "a single trial selects nothing");
        assert!(ten > 0.0);
        assert!(thousand > ten, "more trials raise the bar");
        // Growth is on the order of sqrt(2 ln N): slow, but never zero.
        assert!(thousand < 5.0 * v.sqrt() * (2.0 * 1000.0_f64.ln()).sqrt());
    }

    #[test]
    fn deflation_punishes_a_wide_search() {
        // The same observed Sharpe ratio, found after 5 trials and after
        // 5000, is not the same evidence.
        let few = deflated_sharpe_ratio(0.15, 500, 0.0, 3.0, 0.01, 5).unwrap();
        let many = deflated_sharpe_ratio(0.15, 500, 0.0, 3.0, 0.01, 5_000).unwrap();
        assert!(few > many, "a wider search must deflate harder");
        assert!(
            many < few - 0.05,
            "the penalty must be material: {few} vs {many}"
        );
    }

    #[test]
    fn minimum_track_record_length_is_consistent_with_psr() {
        let (sharpe, benchmark, confidence) = (0.12, 0.0, 0.95);
        let n = minimum_track_record_length(sharpe, benchmark, 0.0, 3.0, confidence).unwrap();
        // At the returned length the PSR should sit right at the target.
        let at =
            probabilistic_sharpe_ratio(sharpe, benchmark, n.ceil() as usize, 0.0, 3.0).unwrap();
        assert!((at - confidence).abs() < 1e-3, "psr at MinTRL = {at}");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            probabilistic_sharpe_ratio(0.1, 0.0, 1, 0.0, 3.0),
            Err(StatsError::TooFewObservations { got: 1, need: 2 })
        );
        assert_eq!(
            probabilistic_sharpe_ratio(f64::NAN, 0.0, 10, 0.0, 3.0),
            Err(StatsError::NotFinite("sharpe"))
        );
        assert_eq!(
            minimum_track_record_length(0.05, 0.10, 0.0, 3.0, 0.95),
            Err(StatsError::ZeroVariance)
        );
    }
}
