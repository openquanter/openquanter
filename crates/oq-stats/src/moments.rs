//! Sample moments of a return series.
//!
//! Skewness and kurtosis use the population (biased) estimators, which is
//! what the deflated Sharpe ratio literature assumes. Kurtosis is
//! reported **non-excess**: a normal sample has kurtosis 3, not 0. Getting
//! that convention wrong silently shifts every downstream probability, so
//! it is stated in the type rather than left to the caller's memory.

use crate::{Result, StatsError};

/// Sample moments of a return series, at the frequency of the input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moments {
    /// Number of observations.
    pub n: usize,
    /// Arithmetic mean.
    pub mean: f64,
    /// Sample standard deviation (divides by `n - 1`).
    pub std_dev: f64,
    /// Population skewness.
    pub skewness: f64,
    /// Population kurtosis, **not** excess: 3.0 for a normal sample.
    pub kurtosis: f64,
}

impl Moments {
    /// Compute the moments of `returns`.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError::TooFewObservations`] with fewer than four
    /// observations (kurtosis is meaningless below that),
    /// [`StatsError::NotFinite`] if any observation is NaN or infinite,
    /// and [`StatsError::ZeroVariance`] for a constant series.
    pub fn from_returns(returns: &[f64]) -> Result<Self> {
        let n = returns.len();
        if n < 4 {
            return Err(StatsError::TooFewObservations { got: n, need: 4 });
        }
        if returns.iter().any(|r| !r.is_finite()) {
            return Err(StatsError::NotFinite("return"));
        }

        let n_f = n as f64;
        let mean = returns.iter().sum::<f64>() / n_f;

        let mut m2 = 0.0;
        let mut m3 = 0.0;
        let mut m4 = 0.0;
        for r in returns {
            let d = r - mean;
            let d2 = d * d;
            m2 += d2;
            m3 += d2 * d;
            m4 += d2 * d2;
        }
        m2 /= n_f;
        m3 /= n_f;
        m4 /= n_f;

        if m2 <= 0.0 {
            return Err(StatsError::ZeroVariance);
        }

        let std_dev = (m2 * n_f / (n_f - 1.0)).sqrt();

        Ok(Self {
            n,
            mean,
            std_dev,
            skewness: m3 / m2.powf(1.5),
            kurtosis: m4 / (m2 * m2),
        })
    }

    /// Sharpe ratio at the frequency of the input series, against a zero
    /// benchmark. Not annualized: annualizing is a reporting decision and
    /// doing it here would make the deflated Sharpe ratio wrong.
    #[must_use]
    pub fn sharpe_ratio(&self) -> f64 {
        self.mean / self.std_dev
    }
}

/// Sharpe ratio of a return series, at the frequency of the input.
///
/// # Errors
///
/// As [`Moments::from_returns`].
pub fn sharpe_ratio(returns: &[f64]) -> Result<f64> {
    Ok(Moments::from_returns(returns)?.sharpe_ratio())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_of_a_known_series() {
        // Mean 3, population variance 2, symmetric.
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let m = Moments::from_returns(&x).unwrap();
        assert!((m.mean - 3.0).abs() < 1e-12);
        assert!((m.std_dev - 2.0_f64.sqrt() * (5.0_f64 / 4.0).sqrt()).abs() < 1e-12);
        assert!(m.skewness.abs() < 1e-12, "symmetric sample has zero skew");
        // Population kurtosis of an arithmetic sequence of five points.
        assert!((m.kurtosis - 1.7).abs() < 1e-12);
    }

    #[test]
    fn skewness_sign_follows_the_tail() {
        let right_tailed = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 10.0];
        let left_tailed: Vec<f64> = right_tailed.iter().map(|v| -v).collect();
        assert!(Moments::from_returns(&right_tailed).unwrap().skewness > 0.0);
        assert!(Moments::from_returns(&left_tailed).unwrap().skewness < 0.0);
    }

    #[test]
    fn sharpe_ratio_is_mean_over_sample_std_dev() {
        let x = [0.01, -0.005, 0.02, 0.0, 0.015, -0.01];
        let m = Moments::from_returns(&x).unwrap();
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let var = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (x.len() as f64 - 1.0);
        assert!((m.sharpe_ratio() - mean / var.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn rejects_degenerate_input() {
        assert_eq!(
            Moments::from_returns(&[1.0, 2.0]),
            Err(StatsError::TooFewObservations { got: 2, need: 4 })
        );
        assert_eq!(
            Moments::from_returns(&[1.0; 8]),
            Err(StatsError::ZeroVariance)
        );
        assert_eq!(
            Moments::from_returns(&[1.0, 2.0, f64::NAN, 4.0]),
            Err(StatsError::NotFinite("return"))
        );
    }
}
