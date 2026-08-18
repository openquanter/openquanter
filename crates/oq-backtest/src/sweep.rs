//! Running many configurations, and reporting what that costs.
//!
//! # A sweep is where a backtest starts lying
//!
//! One backtest is an experiment. A hundred is a search, and the best
//! result of a search is biased upward by the searching — the more
//! configurations tried, the better the best one looks for reasons that
//! have nothing to do with the strategy. That is not a subtlety to
//! mention in a footnote; it is the dominant effect in published
//! backtest results.
//!
//! So this does not report the best Sharpe ratio. It reports the
//! **deflated** Sharpe ratio, which subtracts what the search itself
//! bought, and the **probability of backtest overfitting**, which asks
//! how often the best in-sample configuration underperforms out of
//! sample. Both were already implemented in `oq-stats` and neither could
//! be computed, because a run produced no return series to feed them.
//! That gap is what this module closes.
//!
//! # The return frequency is part of the answer
//!
//! A Sharpe ratio means nothing without knowing what a period is, and a
//! sweep over tick data can produce any number you like by choosing the
//! sampling interval. So the interval is a field on the configuration,
//! every run in a sweep shares it, and it is reported alongside the
//! result rather than assumed.

use oq_stats::pbo::PerformanceMatrix;
use oq_stats::trials::Trial;
use oq_stats::{Moments, TrialRegistry, probability_of_backtest_overfitting};
use oq_strategy::Strategy;
use oq_types::Cash;

use crate::run::{RunConfig, RunResult, run_stream};
use oq_engine::Tick;

/// One configuration in a sweep, and how to build the strategy it names.
pub struct Candidate<'a, S> {
    /// Identifies the configuration in the report. A parameter hash or a
    /// human label; the registry keys trials by it.
    pub id: String,
    /// Builds a fresh strategy. A sweep must not share one between
    /// configurations — a strategy carries state, and reusing it would
    /// make every result depend on the order the sweep happened to run.
    pub build: &'a dyn Fn() -> S,
}

/// What a sweep found.
#[derive(Debug, Clone)]
pub struct SweepReport {
    /// Every configuration's outcome, in the order they were run.
    pub results: Vec<(String, RunResult)>,
    /// Ticks per sampled return, carried because a Sharpe ratio without
    /// it is not a number anyone can compare.
    pub equity_every: usize,
    /// Deflated Sharpe ratio of the best configuration, or the reason it
    /// could not be computed.
    pub deflated_sharpe: Result<f64, String>,
    /// Probability that the best in-sample configuration is not the best
    /// out of sample, or the reason it could not be computed.
    pub pbo: Result<f64, String>,
    /// Configurations that produced too few returns to score.
    pub unscorable: Vec<String>,
}

/// Simple returns from a sampled equity curve.
///
/// Zero-equity samples end the series rather than producing an infinite
/// return: an account at zero has been liquidated, and a return computed
/// through that point describes a position nobody could have held.
#[must_use]
pub fn returns(curve: &[Cash]) -> Vec<f64> {
    let mut out = Vec::with_capacity(curve.len().saturating_sub(1));
    for pair in curve.windows(2) {
        let (prev, next) = (pair[0].0, pair[1].0);
        if prev <= 0 {
            break;
        }
        out.push((next - prev) as f64 / prev as f64);
    }
    out
}

/// Run every candidate over the same ticks and score the set.
///
/// `config.equity_every` must be non-zero or nothing can be scored, and
/// that is reported rather than silently returning a sweep with no
/// statistics.
pub fn sweep<S: Strategy>(
    config: &RunConfig,
    candidates: &[Candidate<'_, S>],
    ticks: &[Tick],
) -> SweepReport {
    let mut results = Vec::new();
    let mut registry = TrialRegistry::new();
    let mut columns: Vec<Vec<f64>> = Vec::new();
    let mut unscorable = Vec::new();

    for candidate in candidates {
        let mut strategy = (candidate.build)();
        let result = run_stream(config, &mut strategy, ticks.iter().copied());
        let series = returns(&result.equity_curve);

        // Two returns is the floor for a variance, and a Sharpe ratio
        // from fewer is a number with no dispersion behind it.
        // Four returns is `Moments`' own floor: skewness and kurtosis
        // are not defined below it, and a Sharpe ratio reported without
        // them cannot be deflated.
        match Moments::from_returns(&series) {
            Ok(moments) => {
                registry.record(Trial {
                    id: candidate.id.clone(),
                    sharpe: moments.sharpe_ratio(),
                    n_observations: series.len(),
                    skewness: moments.skewness,
                    kurtosis: moments.kurtosis,
                });
                columns.push(series);
            }
            Err(_) => unscorable.push(candidate.id.clone()),
        }
        results.push((candidate.id.clone(), result));
    }

    let deflated_sharpe = registry
        .deflated_sharpe_of_best()
        .map_err(|e| format!("{e}"));

    // PBO needs a rectangular matrix, so the columns are cut to the
    // shortest. Cutting rather than padding: a padded column would
    // invent returns, and inventing them in the input to an overfitting
    // measure is the one place it is least acceptable.
    let pbo = pbo_of(&columns);

    SweepReport {
        results,
        equity_every: config.equity_every,
        deflated_sharpe,
        pbo,
        unscorable,
    }
}

fn pbo_of(columns: &[Vec<f64>]) -> Result<f64, String> {
    if columns.len() < 2 {
        return Err("fewer than two scorable configurations".to_string());
    }
    let shortest = columns.iter().map(Vec::len).min().unwrap_or(0);
    // The default split count the statistic is usually reported with, and
    // it needs twice that many periods.
    let blocks = 16;
    if shortest < 2 * blocks {
        return Err(format!(
            "{shortest} sampled returns per configuration; {} are needed for {blocks} blocks",
            2 * blocks
        ));
    }
    let cut: Vec<Vec<f64>> = columns.iter().map(|c| c[..shortest].to_vec()).collect();
    let matrix = PerformanceMatrix::from_columns(&cut).map_err(|e| format!("{e}"))?;
    probability_of_backtest_overfitting(&matrix, blocks)
        .map(|r| r.pbo)
        .map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::Cash;

    #[test]
    fn returns_are_simple_and_come_from_consecutive_samples() {
        let curve = vec![Cash(1000), Cash(1100), Cash(990)];
        let r = returns(&curve);
        assert_eq!(r.len(), 2, "n samples give n-1 returns");
        assert!((r[0] - 0.1).abs() < 1e-12, "{:?}", r[0]);
        assert!((r[1] + 0.1).abs() < 1e-12, "{:?}", r[1]);
    }

    #[test]
    fn a_curve_reaching_zero_ends_the_series_there() {
        // An account at zero has been liquidated. A return computed
        // through that point describes a position nobody could have held,
        // and dividing by it is an infinity that poisons every statistic
        // downstream.
        let curve = vec![Cash(1000), Cash(0), Cash(500)];
        let r = returns(&curve);
        assert_eq!(r.len(), 1, "stops at the zero: {r:?}");
        assert!((r[0] + 1.0).abs() < 1e-12, "and records the total loss");
    }

    #[test]
    fn a_curve_of_one_sample_yields_no_returns_rather_than_a_zero() {
        // A spurious zero return would be a data point that says the
        // account did not move, which is not what one sample means.
        assert!(returns(&[Cash(1000)]).is_empty());
        assert!(returns(&[]).is_empty());
    }

    #[test]
    fn pbo_refuses_a_sweep_too_short_to_split() {
        // Sixteen blocks need thirty-two periods. Reporting a number from
        // fewer would be reporting a split that did not happen.
        let columns = vec![vec![0.01; 10], vec![0.02; 10]];
        let err = pbo_of(&columns).expect_err("too short");
        assert!(err.contains("10 sampled returns"), "{err}");
        assert!(err.contains("32 are needed"), "{err}");
    }

    #[test]
    fn pbo_refuses_a_single_configuration() {
        // Overfitting is a statement about a choice among alternatives.
        // With one candidate there was no choice, so there is nothing to
        // measure and saying zero would be the wrong answer.
        let err = pbo_of(&[vec![0.01; 100]]).expect_err("one config");
        assert!(err.contains("fewer than two"), "{err}");
    }

    #[test]
    fn pbo_cuts_columns_to_the_shortest_rather_than_padding() {
        // Padding would invent returns, and inventing them in the input to
        // an overfitting measure is the least acceptable place for it.
        //
        // The returns vary because a constant series has zero variance and
        // no Sharpe ratio — which the statistic correctly refuses, and
        // which a fixture of repeated values would hide behind.
        let a: Vec<f64> = (0..100).map(|i| 0.01 * f64::from(i % 7) - 0.02).collect();
        let b: Vec<f64> = (0..60).map(|i| 0.01 * f64::from(i % 5) - 0.01).collect();
        let got = pbo_of(&[a, b]);
        assert!(got.is_ok(), "60 is enough for 16 blocks: {got:?}");
        let pbo = got.expect("computed");
        assert!((0.0..=1.0).contains(&pbo), "a probability: {pbo}");
    }

    #[test]
    fn a_constant_return_series_is_refused_rather_than_scored() {
        // Zero variance has no Sharpe ratio. Reporting one would put a
        // number where there is none, and every statistic built on it
        // would inherit the fiction.
        let err = pbo_of(&[vec![0.01; 100], vec![0.01; 100]]).expect_err("no variance");
        assert!(err.contains("variance"), "{err}");
    }
}

/// Thresholds a sweep's statistics must clear to be packaged.
///
/// `FR-RESEARCH-3` asks that results past an overfitting threshold be
/// marked, and in strict mode refused for deployment packaging. Marking
/// alone is what every tool already does: it prints a number and leaves
/// acting on it to somebody who has already decided the strategy works.
/// The refusal is the part that changes behaviour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Largest acceptable probability of backtest overfitting.
    ///
    /// A sweep whose best configuration is a coin flip out of sample has
    /// a PBO near 0.5, so anything approaching it is a search that found
    /// its own noise.
    pub max_pbo: f64,
    /// Smallest acceptable deflated Sharpe ratio.
    ///
    /// The deflated ratio is a probability that the result survives the
    /// number of trials that produced it. Below a half, the search is
    /// more likely to have manufactured the winner than found it.
    pub min_deflated_sharpe: f64,
}

impl Default for Thresholds {
    /// Not derived from anything, and stated as such.
    ///
    /// A PBO of 0.5 is a coin flip, so 0.35 leaves room to be wrong
    /// about the estimate itself; a deflated Sharpe of 0.95 is the
    /// conventional confidence level. Both are conventions rather than
    /// findings, and both are fields precisely so a caller who disagrees
    /// changes a number rather than removing the check.
    fn default() -> Self {
        Self {
            max_pbo: 0.35,
            min_deflated_sharpe: 0.95,
        }
    }
}

/// Why a sweep must not be packaged for deployment.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The probability of overfitting is too high.
    Overfit {
        /// What the sweep measured.
        pbo: f64,
        /// What it had to be at most.
        limit: f64,
    },
    /// The deflated Sharpe ratio is too low.
    Deflated {
        /// What the sweep measured.
        value: f64,
        /// What it had to be at least.
        limit: f64,
    },
    /// A statistic could not be computed at all.
    ///
    /// Refused rather than waved through. A sweep too short to score is
    /// not a sweep that scored well, and the one place that distinction
    /// must hold is the gate that decides whether it gets deployed.
    Unscored {
        /// Which statistic.
        statistic: &'static str,
        /// Why it could not be computed.
        why: String,
    },
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Overfit { pbo, limit } => write!(
                f,
                "probability of backtest overfitting is {pbo:.3}, above the limit of \
                 {limit:.3}: this search is likelier to have found its own noise than an edge"
            ),
            Self::Deflated { value, limit } => write!(
                f,
                "deflated Sharpe ratio is {value:.3}, below the limit of {limit:.3}: \
                 the result does not survive the number of trials that produced it"
            ),
            Self::Unscored { statistic, why } => write!(
                f,
                "{statistic} could not be computed ({why}); a sweep that could not be \
                 scored is not a sweep that scored well"
            ),
        }
    }
}

impl SweepReport {
    /// Every reason this sweep must not be packaged for deployment.
    ///
    /// Empty means it clears the thresholds. Returning all of them
    /// rather than the first: a result that fails on both statistics is
    /// a different situation from one that fails on either, and fixing
    /// the first only to be told about the second wastes the run it
    /// takes to find out.
    #[must_use]
    pub fn refusals(&self, thresholds: Thresholds) -> Vec<Refusal> {
        let mut out = Vec::new();
        match &self.pbo {
            Ok(p) if *p > thresholds.max_pbo => out.push(Refusal::Overfit {
                pbo: *p,
                limit: thresholds.max_pbo,
            }),
            Ok(_) => {}
            Err(why) => out.push(Refusal::Unscored {
                statistic: "probability of backtest overfitting",
                why: why.clone(),
            }),
        }
        match &self.deflated_sharpe {
            Ok(d) if *d < thresholds.min_deflated_sharpe => out.push(Refusal::Deflated {
                value: *d,
                limit: thresholds.min_deflated_sharpe,
            }),
            Ok(_) => {}
            Err(why) => out.push(Refusal::Unscored {
                statistic: "deflated Sharpe ratio",
                why: why.clone(),
            }),
        }
        out
    }

    /// Whether this sweep may be packaged for deployment.
    #[must_use]
    pub fn deployable(&self, thresholds: Thresholds) -> bool {
        self.refusals(thresholds).is_empty()
    }
}

#[cfg(test)]
mod strict_mode {
    use super::*;

    fn report(pbo: Result<f64, String>, dsr: Result<f64, String>) -> SweepReport {
        SweepReport {
            results: Vec::new(),
            equity_every: 1,
            deflated_sharpe: dsr,
            pbo,
            unscorable: Vec::new(),
        }
    }

    /// A sweep that clears both thresholds is deployable, or the gate
    /// would refuse everything and be removed within a week.
    #[test]
    fn a_sweep_that_clears_both_thresholds_is_deployable() {
        let r = report(Ok(0.10), Ok(0.99));
        assert_eq!(r.refusals(Thresholds::default()), Vec::new());
        assert!(r.deployable(Thresholds::default()));
    }

    /// **The point of the mode.** Marking a number and leaving it there
    /// is what every tool already does; the refusal is the part that
    /// changes what happens next.
    #[test]
    fn an_overfit_sweep_is_refused_and_the_reason_carries_the_numbers() {
        let r = report(Ok(0.48), Ok(0.99));
        let refusals = r.refusals(Thresholds::default());
        assert_eq!(refusals.len(), 1);
        assert!(matches!(refusals[0], Refusal::Overfit { .. }));
        assert!(!r.deployable(Thresholds::default()));

        let text = refusals[0].to_string();
        assert!(text.contains("0.480") && text.contains("0.350"), "{text}");
        assert!(text.contains("own noise"), "{text}");
    }

    /// A statistic that could not be computed is refused rather than
    /// waved through. A sweep too short to score is not a sweep that
    /// scored well, and the gate that decides deployment is the one
    /// place that distinction has to hold.
    #[test]
    fn an_unscored_sweep_is_refused_rather_than_passed() {
        let r = report(Err("too few configurations".into()), Ok(0.99));
        let refusals = r.refusals(Thresholds::default());
        assert_eq!(refusals.len(), 1);
        assert!(matches!(refusals[0], Refusal::Unscored { .. }));
        assert!(!r.deployable(Thresholds::default()));
        assert!(
            refusals[0]
                .to_string()
                .contains("not a sweep that scored well"),
            "{}",
            refusals[0]
        );
    }

    /// Both failures are reported. Fixing the first only to be told
    /// about the second wastes the run it takes to find out, and a sweep
    /// is not a cheap thing to repeat.
    #[test]
    fn a_sweep_that_fails_on_both_says_so_on_both() {
        let r = report(Ok(0.60), Ok(0.20));
        assert_eq!(r.refusals(Thresholds::default()).len(), 2);
    }

    /// The thresholds are fields so a caller who disagrees changes a
    /// number rather than removing the check — which is the thing that
    /// actually happens to a gate somebody cannot configure.
    #[test]
    fn a_caller_who_disagrees_changes_the_number_not_the_check() {
        let r = report(Ok(0.48), Ok(0.99));
        assert!(!r.deployable(Thresholds::default()));
        assert!(r.deployable(Thresholds {
            max_pbo: 0.50,
            ..Thresholds::default()
        }));
    }

    /// Exactly at the limit passes. A boundary that refused its own
    /// stated threshold would make the number in the documentation wrong
    /// by one representable step.
    #[test]
    fn the_limit_itself_is_acceptable() {
        let t = Thresholds::default();
        let r = report(Ok(t.max_pbo), Ok(t.min_deflated_sharpe));
        assert!(r.deployable(t), "{:?}", r.refusals(t));
    }
}
