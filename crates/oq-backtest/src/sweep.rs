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
    /// Whether the best in-sample configuration survives out of sample,
    /// or the reason it could not be computed.
    ///
    /// The whole report and not the headline number. `PboReport` also
    /// carries the split logits and a performance-degradation slope
    /// whose own documentation calls it *the defining symptom of an
    /// overfit search*, and both were computed on every sweep and
    /// discarded at this line.
    pub pbo: Result<oq_stats::PboReport, String>,
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

fn pbo_of(columns: &[Vec<f64>]) -> Result<oq_stats::PboReport, String> {
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
    probability_of_backtest_overfitting(&matrix, blocks).map_err(|e| format!("{e}"))
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
        let report = got.expect("computed");
        assert!(
            (0.0..=1.0).contains(&report.pbo),
            "a probability: {}",
            report.pbo
        );
        // The diagnostics travel with it now. They were computed here
        // all along and thrown away at the call site, which is why a
        // sweep could never report the slope its own documentation
        // calls the defining symptom of an overfit search.
        assert_eq!(report.logits.len(), report.n_splits);
        assert!(report.performance_degradation.is_finite());
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
    /// Smallest acceptable slope of out-of-sample Sharpe on in-sample
    /// Sharpe.
    ///
    /// At or below zero, in-sample rank carries no out-of-sample
    /// information — a search that ordered its configurations by noise.
    /// A sweep can pass the PBO threshold and fail this, which is why
    /// it is its own refusal rather than a footnote to that one.
    pub min_degradation_slope: f64,
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
            // Zero, and not a margin above it, because the statistic's
            // own meaning changes sign there: above zero, being better
            // in sample predicted something; at or below, it predicted
            // nothing. A tolerance would be inventing a threshold where
            // the definition already supplies one.
            min_degradation_slope: 0.0,
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
    /// In-sample rank carries no out-of-sample information.
    ///
    /// Separate from `Overfit` because they measure different things
    /// and can disagree. PBO asks how often the best configuration
    /// fails to stay best; this asks whether being better in sample
    /// predicted anything at all. A search can pass the first and fail
    /// this, and the failure means the ranking it produced was noise.
    NoInformation {
        /// What the sweep measured.
        slope: f64,
        /// What it had to exceed.
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
            Self::NoInformation { slope, limit } => write!(
                f,
                "out-of-sample Sharpe regressed on in-sample Sharpe has slope {slope:.3}, \
                 at or below {limit:.3}: ranking the configurations in sample predicted \
                 nothing about them out of sample, so the winner is the one that fit the \
                 noise best"
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
            Ok(r) => {
                // Two independent checks, not two arms of one match. A
                // guard clause would report whichever fired first and
                // hide the other, which is precisely what the paragraph
                // above says this function must not do — and the sweep
                // example proved it: a slope of -0.95 went unreported
                // because the PBO threshold matched first.
                if r.pbo > thresholds.max_pbo {
                    out.push(Refusal::Overfit {
                        pbo: r.pbo,
                        limit: thresholds.max_pbo,
                    });
                }
                // They can disagree. PBO asks how often the winner stops
                // winning; the slope asks whether winning meant anything.
                if r.performance_degradation <= thresholds.min_degradation_slope {
                    out.push(Refusal::NoInformation {
                        slope: r.performance_degradation,
                        limit: thresholds.min_degradation_slope,
                    });
                }
            }
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

    /// A report with a given PBO and a slope that passes, so a test
    /// about one threshold is not silently also about the other.
    fn report(pbo: Result<f64, String>, dsr: Result<f64, String>) -> SweepReport {
        with_slope(pbo, dsr, 1.0)
    }

    fn with_slope(pbo: Result<f64, String>, dsr: Result<f64, String>, slope: f64) -> SweepReport {
        SweepReport {
            results: Vec::new(),
            equity_every: 1,
            deflated_sharpe: dsr,
            pbo: pbo.map(|p| oq_stats::PboReport {
                pbo: p,
                n_splits: 16,
                logits: vec![0.0; 16],
                probability_of_loss: 0.0,
                median_oos_sharpe: 0.0,
                performance_degradation: slope,
            }),
            unscorable: Vec::new(),
        }
    }

    /// A sweep can clear the PBO threshold and still be a search that
    /// ordered its configurations by noise.
    ///
    /// The two statistics answer different questions — how often the
    /// winner stops winning, and whether winning meant anything — so a
    /// gate that checked only the first would pass exactly the sweep
    /// this one exists to stop.
    #[test]
    fn a_flat_slope_is_refused_even_when_the_pbo_passes() {
        let r = with_slope(Ok(0.05), Ok(0.99), 0.0);
        let refusals = r.refusals(Thresholds::default());
        assert!(
            refusals
                .iter()
                .any(|x| matches!(x, Refusal::NoInformation { .. })),
            "a zero slope was accepted: {refusals:?}"
        );
    }

    /// Failing both reports both.
    ///
    /// Written after getting it wrong: the first version made these two
    /// arms of one `match`, so a sweep that failed both reported only
    /// the PBO and the slope went unmentioned. The sweep example caught
    /// it — a measured slope of -0.95 produced no refusal — which is
    /// the sort of thing a guard clause hides and a run does not.
    #[test]
    fn a_sweep_failing_the_pbo_and_the_slope_is_refused_on_both() {
        let r = with_slope(Ok(0.9), Ok(0.99), -0.9);
        let refusals = r.refusals(Thresholds::default());
        assert!(
            refusals
                .iter()
                .any(|x| matches!(x, Refusal::Overfit { .. }))
        );
        assert!(
            refusals
                .iter()
                .any(|x| matches!(x, Refusal::NoInformation { .. })),
            "only one of two failures was reported: {refusals:?}"
        );
    }

    /// And a positive slope does not add a refusal of its own.
    #[test]
    fn a_positive_slope_is_not_refused() {
        let r = with_slope(Ok(0.05), Ok(0.99), 0.4);
        assert!(r.refusals(Thresholds::default()).is_empty());
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
