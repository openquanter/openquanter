//! The statistical regularities real price series show, measured.
//!
//! Named after Cont's 2001 survey, which collected the properties that
//! hold across essentially every liquid market and asset class. They are
//! not a model and nothing here fits one — each is a number you can
//! compute from a return series and compare.
//!
//! # Why a backtest framework needs them
//!
//! Every example in this workspace, every golden that pins a quoted
//! number, and the whole margin-fidelity argument runs on a **generated**
//! market. If those series are structurally unlike real ones, the
//! numbers are demonstrations on a fiction, and the honest thing is to
//! know in which direction.
//!
//! This is deliberately a measurement and not a gate. A generated market
//! is allowed to fail these — `MarketShape::calm` is supposed to be
//! calm. What is not allowed is failing them without anybody knowing,
//! and then quoting results from it as though the market had been real.
//!
//! # What is not here
//!
//! Facts needing more than a return series — the leverage effect wants a
//! volatility proxy, gain/loss asymmetry wants a drawdown definition
//! with choices in it, order-flow autocorrelation wants the order flow.
//! They are omitted rather than approximated, because a stylized fact
//! computed a slightly different way is a number that cannot be compared
//! to the literature, which is the only reason to compute it.

use crate::{Result, StatsError};

/// Linear autocorrelation of `xs` at `lag`.
///
/// Returns `None` when there are fewer than `lag + 2` observations or
/// the series has no variance — both cases where the coefficient is
/// undefined rather than zero, and reporting zero would read as
/// "measured, and there is no correlation".
#[must_use]
pub fn autocorrelation(xs: &[f64], lag: usize) -> Option<f64> {
    if lag == 0 || xs.len() < lag + 2 {
        return None;
    }
    let n = xs.len();
    #[allow(clippy::cast_precision_loss)]
    let mean = xs.iter().sum::<f64>() / n as f64;
    let var: f64 = xs.iter().map(|x| (x - mean) * (x - mean)).sum();
    if var <= 0.0 {
        return None;
    }
    let cov: f64 = (lag..n)
        .map(|i| (xs[i] - mean) * (xs[i - lag] - mean))
        .sum();
    Some(cov / var)
}

/// Whether one fact holds, with the number that decided it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// The series shows the property.
    Holds(f64),
    /// It does not.
    Absent(f64),
    /// Not enough data, or a degenerate series.
    Unmeasurable,
}

impl Verdict {
    #[must_use]
    pub const fn holds(&self) -> bool {
        matches!(self, Self::Holds(_))
    }

    /// The measured value, when there was one.
    #[must_use]
    pub const fn value(&self) -> Option<f64> {
        match self {
            Self::Holds(v) | Self::Absent(v) => Some(*v),
            Self::Unmeasurable => None,
        }
    }
}

/// The facts this crate can compute from a return series alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StylizedFacts {
    pub n: usize,
    /// **No linear autocorrelation.** Real returns are close to
    /// uncorrelated at lag 1; a series that is not is one an arbitrage
    /// would have removed. Held when |ρ(1)| < 0.1.
    pub uncorrelated_returns: Verdict,
    /// **Heavy tails.** Real returns have excess kurtosis well above
    /// zero — the large moves are far more frequent than a normal
    /// distribution allows, which is the whole reason a margin model
    /// matters. Held when excess kurtosis > 1.
    pub heavy_tails: Verdict,
    /// **Volatility clustering.** Large moves follow large moves.
    /// Measured as the lag-1 autocorrelation of absolute returns, which
    /// is positive and slowly decaying in every liquid market. Held
    /// when it exceeds 0.1.
    pub volatility_clustering: Verdict,
    /// **Aggregational gaussianity.** Tails thin as the horizon
    /// lengthens. Measured as excess kurtosis at horizon 1 minus at
    /// horizon 10; positive means the longer horizon is closer to
    /// normal, which is the direction real series move.
    pub aggregational_gaussianity: Verdict,
}

impl StylizedFacts {
    /// Measure what can be measured from `returns`.
    ///
    /// # Errors
    /// [`StatsError::TooFewObservations`] below 32 observations. The
    /// threshold is not about any single statistic — kurtosis needs
    /// four — but about the horizon-10 aggregation below, which would
    /// otherwise be computed from three points and reported with the
    /// same confidence as everything else.
    pub fn measure(returns: &[f64]) -> Result<Self> {
        if returns.len() < 32 {
            return Err(StatsError::TooFewObservations {
                need: 32,
                got: returns.len(),
            });
        }
        if returns.iter().any(|r| !r.is_finite()) {
            return Err(StatsError::NotFinite("returns"));
        }

        let uncorrelated_returns = match autocorrelation(returns, 1) {
            None => Verdict::Unmeasurable,
            Some(r) if r.abs() < 0.1 => Verdict::Holds(r),
            Some(r) => Verdict::Absent(r),
        };

        let excess = excess_kurtosis(returns);
        let heavy_tails = match excess {
            None => Verdict::Unmeasurable,
            Some(k) if k > 1.0 => Verdict::Holds(k),
            Some(k) => Verdict::Absent(k),
        };

        let abs: Vec<f64> = returns.iter().map(|r| r.abs()).collect();
        let volatility_clustering = match autocorrelation(&abs, 1) {
            None => Verdict::Unmeasurable,
            Some(r) if r > 0.1 => Verdict::Holds(r),
            Some(r) => Verdict::Absent(r),
        };

        let aggregated = aggregate(returns, 10);
        let aggregational_gaussianity = match (excess, excess_kurtosis(&aggregated)) {
            (Some(short), Some(long)) => {
                let thinning = short - long;
                if thinning > 0.0 {
                    Verdict::Holds(thinning)
                } else {
                    Verdict::Absent(thinning)
                }
            }
            _ => Verdict::Unmeasurable,
        };

        Ok(Self {
            n: returns.len(),
            uncorrelated_returns,
            heavy_tails,
            volatility_clustering,
            aggregational_gaussianity,
        })
    }

    /// How many of the four hold.
    #[must_use]
    pub const fn held(&self) -> usize {
        self.uncorrelated_returns.holds() as usize
            + self.heavy_tails.holds() as usize
            + self.volatility_clustering.holds() as usize
            + self.aggregational_gaussianity.holds() as usize
    }

    /// One line per fact, for a report.
    #[must_use]
    pub fn render(&self) -> String {
        let line = |name: &str, v: Verdict| match v {
            Verdict::Holds(x) => format!("  {name:<26} holds      {x:>8.3}\n"),
            Verdict::Absent(x) => format!("  {name:<26} absent     {x:>8.3}\n"),
            Verdict::Unmeasurable => format!("  {name:<26} unmeasurable\n"),
        };
        let mut s = format!("stylized facts over {} returns\n", self.n);
        s.push_str(&line("uncorrelated returns", self.uncorrelated_returns));
        s.push_str(&line("heavy tails", self.heavy_tails));
        s.push_str(&line("volatility clustering", self.volatility_clustering));
        s.push_str(&line(
            "aggregational gaussianity",
            self.aggregational_gaussianity,
        ));
        s
    }
}

/// Excess kurtosis: zero for a normal sample rather than three.
///
/// [`crate::Moments::kurtosis`] is the raw fourth moment and documents
/// itself as not-excess. The literature quotes excess, so this converts
/// rather than leaving every caller to remember which convention it is
/// reading.
fn excess_kurtosis(xs: &[f64]) -> Option<f64> {
    if xs.len() < 4 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let m2 = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    if m2 <= 0.0 {
        return None;
    }
    let m4 = xs.iter().map(|x| (x - mean).powi(4)).sum::<f64>() / n;
    Some(m4 / (m2 * m2) - 3.0)
}

/// Non-overlapping sums of `k` consecutive returns.
///
/// Non-overlapping on purpose: overlapping windows share observations,
/// so their kurtosis is biased toward the short-horizon value and the
/// aggregation would appear to do less than it does.
fn aggregate(returns: &[f64], k: usize) -> Vec<f64> {
    returns.chunks_exact(k).map(|c| c.iter().sum()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random sequence, so the tests below
    /// describe this series and not whatever the machine felt like.
    fn lcg(seed: u64, n: usize) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                #[allow(clippy::cast_precision_loss)]
                let u = (s >> 11) as f64 / (1u64 << 53) as f64;
                u - 0.5
            })
            .collect()
    }

    #[test]
    fn autocorrelation_of_a_repeated_pattern_is_one_at_its_period() {
        let xs: Vec<f64> = (0..100)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let rho2 = autocorrelation(&xs, 2).expect("measurable");
        assert!(
            rho2 > 0.9,
            "lag 2 of a period-2 series should be ~1: {rho2}"
        );
        let rho1 = autocorrelation(&xs, 1).expect("measurable");
        assert!(rho1 < -0.9, "lag 1 should be ~-1: {rho1}");
    }

    /// A constant series has no variance, so the coefficient is
    /// undefined. Reporting zero would read as "measured, uncorrelated".
    #[test]
    fn a_flat_series_is_unmeasurable_rather_than_uncorrelated() {
        let xs = vec![1.0; 100];
        assert_eq!(autocorrelation(&xs, 1), None);
    }

    /// Independent noise shows the first fact and not the other two.
    ///
    /// This is the control: if a series with no structure in it were
    /// reported as having heavy tails or clustering, the measurement
    /// would be finding something that is not there.
    #[test]
    fn independent_uniform_noise_is_uncorrelated_and_thin_tailed() {
        let f = StylizedFacts::measure(&lcg(42, 4_000)).expect("enough data");
        assert!(
            f.uncorrelated_returns.holds(),
            "independent draws showed autocorrelation: {:?}",
            f.uncorrelated_returns
        );
        assert!(
            !f.heavy_tails.holds(),
            "uniform noise is thin-tailed, not heavy: {:?}",
            f.heavy_tails
        );
        assert!(
            !f.volatility_clustering.holds(),
            "independent draws cannot cluster: {:?}",
            f.volatility_clustering
        );
    }

    /// And a series built to cluster is found to cluster, with the
    /// heavy tails that come with it.
    ///
    /// The other half of the control. A measurement that never says yes
    /// is as useless as one that never says no, and only running it on
    /// series where the answer is known separates the two.
    ///
    /// The quiet regime is nine times as long as the loud one, and that
    /// ratio is doing real work. An even mixture of two variances
    /// reaches an excess kurtosis of about 0.6 and no further — measured
    /// while writing this, at 0.679 — because a uniform draw starts at
    /// −1.2 and an even mix only lifts it that far. Heavy tails come
    /// from large moves being *rare*, not from there being two sizes of
    /// move. Getting that wrong is what a control is for.
    #[test]
    fn rare_volatile_bursts_cluster_and_produce_heavy_tails() {
        let base = lcg(7, 4_000);
        let clustered: Vec<f64> = base
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if (i / 200) % 10 == 0 {
                    r * 5.0
                } else {
                    r * 0.1
                }
            })
            .collect();
        let f = StylizedFacts::measure(&clustered).expect("enough data");
        assert!(
            f.volatility_clustering.holds(),
            "contiguous bursts did not register as clustering: {:?}",
            f.volatility_clustering
        );
        assert!(
            f.heavy_tails.holds(),
            "rare large moves should produce excess kurtosis: {:?}",
            f.heavy_tails
        );
    }

    #[test]
    fn too_few_observations_is_refused_rather_than_guessed() {
        assert!(StylizedFacts::measure(&lcg(1, 31)).is_err());
        assert!(StylizedFacts::measure(&lcg(1, 32)).is_ok());
    }
}
