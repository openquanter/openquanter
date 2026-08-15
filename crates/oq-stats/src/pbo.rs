//! Probability of backtest overfitting, via combinatorially symmetric
//! cross-validation (CSCV).
//!
//! The deflated Sharpe ratio judges a *result*. This judges the
//! *procedure*: if you pick the best configuration in-sample, how often
//! does it land below the median out-of-sample? Under an honest process
//! that number is low; when the sweep is fitting noise it approaches
//! one half, which is the value you would get by choosing at random.
//!
//! Method: split the trial period into `S` disjoint blocks, take every
//! way of assigning half the blocks to training and half to testing,
//! select the in-sample winner, and record its out-of-sample rank. The
//! symmetry of the split is what makes the estimate unbiased — every
//! block serves as training and test material equally often.
//!
//! Reference: Bailey, Borwein, López de Prado & Zhu, "The Probability of
//! Backtest Overfitting" (2014).

use crate::{Result, StatsError};

/// Per-period returns for every configuration in a sweep.
///
/// Row-major: `data[period * n_configs + config]`. All configurations
/// must cover the same periods — that is what makes their ranks
/// comparable.
#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceMatrix {
    n_periods: usize,
    n_configs: usize,
    data: Vec<f64>,
}

impl PerformanceMatrix {
    /// Build a matrix from row-major data.
    ///
    /// # Errors
    ///
    /// [`StatsError::MalformedMatrix`] if the dimensions do not match the
    /// data length or either dimension is zero, and
    /// [`StatsError::NotFinite`] if any entry is NaN or infinite.
    pub fn new(n_periods: usize, n_configs: usize, data: Vec<f64>) -> Result<Self> {
        if n_periods == 0 || n_configs == 0 {
            return Err(StatsError::MalformedMatrix("dimensions must be non-zero"));
        }
        if data.len() != n_periods * n_configs {
            return Err(StatsError::MalformedMatrix(
                "data length does not match n_periods * n_configs",
            ));
        }
        if data.iter().any(|v| !v.is_finite()) {
            return Err(StatsError::NotFinite("performance matrix entry"));
        }
        Ok(Self {
            n_periods,
            n_configs,
            data,
        })
    }

    /// Build a matrix from one return series per configuration.
    ///
    /// # Errors
    ///
    /// As [`PerformanceMatrix::new`]; additionally
    /// [`StatsError::MalformedMatrix`] if the series differ in length.
    pub fn from_columns(columns: &[Vec<f64>]) -> Result<Self> {
        let n_configs = columns.len();
        if n_configs == 0 {
            return Err(StatsError::MalformedMatrix("no configurations"));
        }
        let n_periods = columns[0].len();
        if columns.iter().any(|c| c.len() != n_periods) {
            return Err(StatsError::MalformedMatrix(
                "configurations cover different numbers of periods",
            ));
        }

        let mut data = Vec::with_capacity(n_periods * n_configs);
        for period in 0..n_periods {
            for column in columns {
                data.push(column[period]);
            }
        }
        Self::new(n_periods, n_configs, data)
    }

    /// Number of periods (rows).
    #[must_use]
    pub fn n_periods(&self) -> usize {
        self.n_periods
    }

    /// Number of configurations (columns).
    #[must_use]
    pub fn n_configs(&self) -> usize {
        self.n_configs
    }
}

/// Result of a CSCV run.
#[derive(Debug, Clone)]
pub struct PboReport {
    /// Probability of backtest overfitting: the share of splits where the
    /// in-sample winner ranked at or below the out-of-sample median.
    pub pbo: f64,
    /// Number of train/test splits evaluated.
    pub n_splits: usize,
    /// Logit of the out-of-sample relative rank, one per split. The
    /// distribution is more informative than the headline number.
    pub logits: Vec<f64>,
    /// Share of splits where the selected configuration lost money out of
    /// sample.
    pub probability_of_loss: f64,
    /// Median out-of-sample Sharpe ratio of the selected configuration.
    pub median_oos_sharpe: f64,
    /// Slope of out-of-sample Sharpe regressed on in-sample Sharpe across
    /// splits. At or below zero means in-sample performance carries no
    /// information — the defining symptom of an overfit search.
    pub performance_degradation: f64,
}

impl PboReport {
    /// Whether the sweep passes at the given PBO threshold.
    #[must_use]
    pub fn passes(&self, threshold: f64) -> bool {
        self.pbo <= threshold
    }
}

/// Estimate the probability of backtest overfitting.
///
/// `n_blocks` is the number of disjoint time blocks; it must be even and
/// at least 4. Sixteen is the usual choice, giving 12 870 splits.
///
/// # Errors
///
/// [`StatsError::InvalidSplitCount`] for an odd or too-small block count,
/// [`StatsError::TooFewObservations`] if the matrix cannot supply at
/// least two periods per block or has fewer than two configurations, and
/// [`StatsError::ZeroVariance`] if a block set has no dispersion at all.
pub fn probability_of_backtest_overfitting(
    matrix: &PerformanceMatrix,
    n_blocks: usize,
) -> Result<PboReport> {
    if n_blocks < 4 || n_blocks % 2 != 0 {
        return Err(StatsError::InvalidSplitCount { got: n_blocks });
    }
    if matrix.n_configs < 2 {
        return Err(StatsError::TooFewObservations {
            got: matrix.n_configs,
            need: 2,
        });
    }
    if matrix.n_periods < 2 * n_blocks {
        return Err(StatsError::TooFewObservations {
            got: matrix.n_periods,
            need: 2 * n_blocks,
        });
    }

    let stats = BlockStats::build(matrix, n_blocks);
    let half = n_blocks / 2;

    let mut logits = Vec::new();
    let mut in_sample_sharpes = Vec::new();
    let mut out_of_sample_sharpes = Vec::new();
    let mut losses = 0usize;

    let mut selection = (0..half).collect::<Vec<_>>();
    loop {
        let is_sharpe = stats.sharpes(&selection)?;
        let oos_blocks: Vec<usize> = (0..n_blocks).filter(|b| !selection.contains(b)).collect();
        let oos_sharpe = stats.sharpes(&oos_blocks)?;

        let winner = argmax(&is_sharpe);
        let rank = relative_rank(&oos_sharpe, winner);

        // Guard the logit against the degenerate ranks at the extremes.
        let omega = rank.clamp(f64::EPSILON, 1.0 - f64::EPSILON);
        logits.push((omega / (1.0 - omega)).ln());

        in_sample_sharpes.push(is_sharpe[winner]);
        out_of_sample_sharpes.push(oos_sharpe[winner]);
        if oos_sharpe[winner] < 0.0 {
            losses += 1;
        }

        if !next_combination(&mut selection, n_blocks) {
            break;
        }
    }

    let n_splits = logits.len();
    let n_f = n_splits as f64;
    let pbo = logits.iter().filter(|l| **l <= 0.0).count() as f64 / n_f;

    Ok(PboReport {
        pbo,
        n_splits,
        logits,
        probability_of_loss: losses as f64 / n_f,
        median_oos_sharpe: median(&mut out_of_sample_sharpes.clone()),
        performance_degradation: ols_slope(&in_sample_sharpes, &out_of_sample_sharpes),
    })
}

/// Per-block, per-configuration sufficient statistics.
///
/// Sharpe ratios for any union of blocks are recovered from these sums,
/// so the combinatorial loop never rescans the returns. With 16 blocks
/// this turns a quadratic sweep into a linear one.
struct BlockStats {
    n_configs: usize,
    counts: Vec<usize>,
    sums: Vec<f64>,
    sums_sq: Vec<f64>,
}

impl BlockStats {
    fn build(matrix: &PerformanceMatrix, n_blocks: usize) -> Self {
        let n_configs = matrix.n_configs;
        let mut counts = vec![0usize; n_blocks];
        let mut sums = vec![0.0; n_blocks * n_configs];
        let mut sums_sq = vec![0.0; n_blocks * n_configs];

        for period in 0..matrix.n_periods {
            // Contiguous blocks: CSCV assumes the blocks are time-ordered
            // slices, not a shuffle, so serial structure is preserved.
            let block = (period * n_blocks / matrix.n_periods).min(n_blocks - 1);
            counts[block] += 1;
            for config in 0..n_configs {
                let value = matrix.data[period * n_configs + config];
                sums[block * n_configs + config] += value;
                sums_sq[block * n_configs + config] += value * value;
            }
        }

        Self {
            n_configs,
            counts,
            sums,
            sums_sq,
        }
    }

    fn sharpes(&self, blocks: &[usize]) -> Result<Vec<f64>> {
        let n: usize = blocks.iter().map(|b| self.counts[*b]).sum();
        if n < 2 {
            return Err(StatsError::TooFewObservations { got: n, need: 2 });
        }
        let n_f = n as f64;

        let mut out = Vec::with_capacity(self.n_configs);
        for config in 0..self.n_configs {
            let mut sum = 0.0;
            let mut sum_sq = 0.0;
            for block in blocks {
                sum += self.sums[block * self.n_configs + config];
                sum_sq += self.sums_sq[block * self.n_configs + config];
            }
            let mean = sum / n_f;
            let variance = (sum_sq - n_f * mean * mean) / (n_f - 1.0);
            // A configuration that never moves has no Sharpe ratio; it
            // ranks last rather than aborting the whole run.
            out.push(if variance > 0.0 {
                mean / variance.sqrt()
            } else {
                f64::NEG_INFINITY
            });
        }

        if out.iter().all(|s| !s.is_finite()) {
            return Err(StatsError::ZeroVariance);
        }
        Ok(out)
    }
}

fn argmax(values: &[f64]) -> usize {
    let mut best = 0;
    for (i, v) in values.iter().enumerate() {
        if v > &values[best] {
            best = i;
        }
    }
    best
}

/// Relative rank of `index` within `values`, in `(0, 1)`.
///
/// Zero is worst, one is best; ties share the mid-rank. The `n + 1`
/// denominator keeps the extremes off the open interval so the logit
/// stays finite.
fn relative_rank(values: &[f64], index: usize) -> f64 {
    let target = values[index];
    let mut lower = 0.0;
    let mut equal = 0.0;
    for (i, v) in values.iter().enumerate() {
        if i == index {
            continue;
        }
        if *v < target {
            lower += 1.0;
        } else if *v == target {
            equal += 1.0;
        }
    }
    let rank = lower + equal / 2.0 + 1.0;
    rank / (values.len() as f64 + 1.0)
}

/// Advance `selection` to the next combination in lexicographic order.
/// Returns `false` once the last combination has been produced.
fn next_combination(selection: &mut [usize], n: usize) -> bool {
    let k = selection.len();
    let mut i = k;
    while i > 0 {
        i -= 1;
        if selection[i] != i + n - k {
            selection[i] += 1;
            for j in i + 1..k {
                selection[j] = selection[j - 1] + 1;
            }
            return true;
        }
    }
    false
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn ols_slope(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean_x = x.iter().sum::<f64>() / n;
    let mean_y = y.iter().sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var = 0.0;
    for (xi, yi) in x.iter().zip(y) {
        cov += (xi - mean_x) * (yi - mean_y);
        var += (xi - mean_x) * (xi - mean_x);
    }
    if var == 0.0 { 0.0 } else { cov / var }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic generator: the tests must reproduce exactly, on any
    /// machine, forever. No external RNG, no thread-local state.
    ///
    /// SplitMix64 rather than a bare LCG: an LCG consumed at a fixed
    /// stride (one row of the matrix per iteration) leaks its lattice
    /// structure into the columns, which shows up as persistent
    /// per-configuration bias — synthetic "noise" that is not noise, and
    /// a test that measures the generator instead of the estimator.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn next_uniform(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Standard normal via Box-Muller.
        fn next_normal(&mut self) -> f64 {
            let u1 = self.next_uniform().max(f64::MIN_POSITIVE);
            let u2 = self.next_uniform();
            (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
        }
    }

    fn noise_matrix(n_periods: usize, n_configs: usize, seed: u64) -> PerformanceMatrix {
        let mut rng = SplitMix64::new(seed);
        let data = (0..n_periods * n_configs)
            .map(|_| rng.next_normal())
            .collect();
        PerformanceMatrix::new(n_periods, n_configs, data).unwrap()
    }

    #[test]
    fn pure_noise_gives_pbo_near_one_half() {
        // Every configuration is worthless, so picking the in-sample best
        // is a coin flip out of sample.
        //
        // Averaged over seeds on purpose: for a single sample the PBO
        // estimate ranges roughly 0.25-0.8 even when the null is exactly
        // true, because 12 configurations give the rank statistic very
        // little to work with. Asserting on one seed would be asserting
        // on that seed's luck.
        const SEEDS: usize = 12;
        let mut total = 0.0;
        for seed in 0..SEEDS {
            let matrix = noise_matrix(600, 12, 0x9E37_u64.wrapping_mul(seed as u64 + 1) + 12_345);
            let report = probability_of_backtest_overfitting(&matrix, 8).unwrap();
            assert_eq!(report.n_splits, 70, "C(8,4) = 70 symmetric splits");
            total += report.pbo;
        }
        let mean_pbo = total / SEEDS as f64;
        assert!(
            (mean_pbo - 0.5).abs() < 0.15,
            "mean pbo on pure noise = {mean_pbo}, expected near 0.5"
        );
    }

    #[test]
    fn a_genuinely_dominant_configuration_gives_pbo_near_zero() {
        let (n_periods, n_configs) = (1_000, 10);
        let mut rng = SplitMix64::new(42);
        let mut data = Vec::with_capacity(n_periods * n_configs);
        for _ in 0..n_periods {
            for config in 0..n_configs {
                // Configuration 0 has real edge; the rest are noise.
                let edge = if config == 0 { 0.35 } else { 0.0 };
                data.push(edge + rng.next_normal());
            }
        }
        let matrix = PerformanceMatrix::new(n_periods, n_configs, data).unwrap();
        let report = probability_of_backtest_overfitting(&matrix, 8).unwrap();

        assert!(
            report.pbo < 0.1,
            "a real edge must survive out of sample, pbo = {}",
            report.pbo
        );
        assert!(
            report.median_oos_sharpe > 0.0,
            "the selected configuration should make money out of sample"
        );
        assert!(report.probability_of_loss < 0.2);
        assert!(report.passes(0.5));
    }

    #[test]
    fn combination_enumeration_is_complete_and_ordered() {
        let mut selection = vec![0, 1, 2];
        let mut seen = vec![selection.clone()];
        while next_combination(&mut selection, 5) {
            seen.push(selection.clone());
        }
        assert_eq!(seen.len(), 10, "C(5,3) = 10");
        assert_eq!(seen[0], vec![0, 1, 2]);
        assert_eq!(seen[9], vec![2, 3, 4]);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "combinations must be distinct");
    }

    #[test]
    fn relative_rank_handles_ties_and_extremes() {
        let values = [1.0, 2.0, 3.0];
        assert!(relative_rank(&values, 2) > relative_rank(&values, 0));
        let tied = [1.0, 1.0, 1.0, 1.0];
        for i in 0..4 {
            assert!((relative_rank(&tied, i) - 0.5).abs() < 1e-12);
        }
    }

    #[test]
    fn matrix_construction_validates_input() {
        assert_eq!(
            PerformanceMatrix::new(2, 2, vec![1.0, 2.0]),
            Err(StatsError::MalformedMatrix(
                "data length does not match n_periods * n_configs"
            ))
        );
        assert_eq!(
            PerformanceMatrix::from_columns(&[vec![1.0, 2.0], vec![1.0]]),
            Err(StatsError::MalformedMatrix(
                "configurations cover different numbers of periods"
            ))
        );
        let ok = PerformanceMatrix::from_columns(&[vec![1.0, 2.0], vec![3.0, 4.0]]).unwrap();
        assert_eq!((ok.n_periods(), ok.n_configs()), (2, 2));
    }

    #[test]
    fn rejects_bad_split_counts_and_thin_data() {
        let matrix = noise_matrix(100, 5, 7);
        assert_eq!(
            probability_of_backtest_overfitting(&matrix, 7).unwrap_err(),
            StatsError::InvalidSplitCount { got: 7 }
        );
        assert_eq!(
            probability_of_backtest_overfitting(&matrix, 2).unwrap_err(),
            StatsError::InvalidSplitCount { got: 2 }
        );
        let thin = noise_matrix(10, 5, 7);
        assert_eq!(
            probability_of_backtest_overfitting(&thin, 8).unwrap_err(),
            StatsError::TooFewObservations { got: 10, need: 16 }
        );
    }
}
