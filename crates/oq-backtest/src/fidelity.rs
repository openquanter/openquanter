//! What a backtest without a margin model is worth.
//!
//! `MarginMode::Ignored` is not a debugging convenience. It is the
//! control arm: it is *exactly* what a backtest that has no margin model
//! silently assumes, which is that the venue will never close the
//! account. Running the same strategy over the same ticks twice, once
//! with each mode, therefore measures a specific thing — the error a
//! margin-free backtest makes — rather than a difference between two
//! models.
//!
//! # Why the tail and not the mean
//!
//! The two arms are identical tick for tick until the first liquidation.
//! Before that instant the overlay changes nothing; after it, one arm
//! holds a position and the other holds nothing. So the divergence is
//! not a level shift spread over the run: it is zero everywhere and then
//! very large in a few places. Summary statistics computed over the
//! whole series — mean return, Sharpe, even max drawdown — average that
//! concentration away and report a small number for a fatal difference.
//! The quantiles of the *return distribution* do not, which is why this
//! reports them.
//!
//! # What this deliberately does not claim
//!
//! After the first liquidation the two series are no longer paired
//! observations of the same thing: the enforced arm is describing a
//! closed account, the ignored arm a position nobody could have held.
//! Differencing them past that point produces a number, and the number
//! is meaningless. [`Fidelity::paired_until`] is where the pairing ends,
//! and every paired statistic here stops there. The unpaired part is
//! reported as what it is — the ignored arm's fiction — and not as a
//! difference.

use oq_types::Cash;

use crate::run::RunResult;
use crate::sweep::returns;

/// One arm of the comparison, reduced to what the report needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    /// Equity at the end of the run.
    pub final_equity: Cash,
    /// The lowest equity the account ever showed.
    pub min_equity: Cash,
    /// How many times the venue closed the account.
    pub liquidations: usize,
    /// Sampled returns, from the equity curve.
    pub returns: Vec<f64>,
}

impl Arm {
    fn of(r: &RunResult) -> Self {
        Self {
            final_equity: r.final_equity,
            min_equity: r.min_equity,
            liquidations: r.liquidations.len(),
            returns: returns(&r.equity_curve),
        }
    }
}

/// One quantile of both return distributions, and the gap between them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TailPoint {
    /// The quantile, in `(0, 1)`. 0.01 is the 1st percentile.
    pub q: f64,
    /// The quantile of the enforced arm's returns.
    pub enforced: f64,
    /// The quantile of the ignored arm's returns.
    pub ignored: f64,
}

impl TailPoint {
    /// How much better the margin-free arm looks at this quantile.
    ///
    /// Positive means the margin-free backtest is optimistic here, which
    /// is the direction the error always takes in the left tail: the
    /// overlay can only ever remove outcomes, never add good ones.
    #[must_use]
    pub fn overstatement(&self) -> f64 {
        self.ignored - self.enforced
    }
}

/// Why a comparison could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    /// `equity_every` was zero, so neither run produced a curve.
    NoEquityCurve,
    /// One arm produced too few samples to take a quantile of.
    TooFewSamples {
        /// How many paired return samples there were.
        have: usize,
        /// How many the smallest requested quantile needs.
        need: usize,
    },
    /// A window's return has no denominator.
    NoStartingBalance,
    /// The two runs did not see the same ticks, so they are not two arms
    /// of one experiment and differencing them measures nothing.
    DifferentRuns {
        /// Ticks the enforced arm saw.
        enforced: usize,
        /// Ticks the ignored arm saw.
        ignored: usize,
    },
}

impl core::fmt::Display for Unusable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoEquityCurve => write!(
                f,
                "neither run sampled equity; set RunConfig::equity_every above zero"
            ),
            Self::TooFewSamples { have, need } => write!(
                f,
                "{have} paired return samples cannot support a quantile needing {need}"
            ),
            Self::NoStartingBalance => write!(
                f,
                "a window return needs a positive starting balance to divide by"
            ),
            Self::DifferentRuns { enforced, ignored } => write!(
                f,
                "the arms saw different ticks ({enforced} vs {ignored}); \
                 they are not two arms of one experiment"
            ),
        }
    }
}

/// The margin fidelity report.
#[derive(Debug, Clone, PartialEq)]
pub struct Fidelity {
    /// The arm that models liquidation. What a real account experiences.
    pub enforced: Arm,
    /// The arm that does not. What a margin-free backtest assumes.
    pub ignored: Arm,
    /// Index of the first return sample at which the arms disagree, or
    /// `None` when they never did — meaning the overlay never bit and
    /// the margin-free backtest happened to be right for these ticks.
    pub diverged_at: Option<usize>,
    /// How many samples the two series describe the same account for.
    ///
    /// Every paired statistic in this report is computed over
    /// `..paired_until` and nothing else. See the module docs.
    pub paired_until: usize,
    /// The requested quantiles of both return distributions.
    pub tail: Vec<TailPoint>,
    /// Terminal equity the margin-free arm claims and the enforced arm
    /// did not reach. Positive means the margin-free run overstates the
    /// result; it is reported separately from the tail because it is a
    /// single number about the end, not a property of the distribution.
    pub terminal_overstatement: Cash,
}

impl Fidelity {
    /// Whether the margin-free arm described an account that had already
    /// been closed.
    ///
    /// This is the tell. If it is true, every number the margin-free
    /// backtest reports after [`Self::paired_until`] is about a position
    /// no venue would have let the account hold.
    #[must_use]
    pub const fn margin_free_traded_a_dead_account(&self) -> bool {
        self.enforced.liquidations > 0
    }

    /// The largest overstatement across the requested quantiles.
    ///
    /// `None` when no quantiles were requested.
    #[must_use]
    pub fn worst_overstatement(&self) -> Option<TailPoint> {
        self.tail
            .iter()
            .copied()
            .max_by(|a, b| a.overstatement().total_cmp(&b.overstatement()))
    }
}

/// Compare a margin-enforced run against the same run with the overlay
/// switched off.
///
/// `quantiles` are the points of the return distribution to report, each
/// in `(0, 1)`; `&[0.01, 0.05, 0.10]` is the usual left tail. They are
/// returned in the order given.
///
/// # Errors
///
/// Returns [`Unusable`] when the two runs are not two arms of one
/// experiment, or when there is not enough sampled data to take the
/// requested quantiles of.
pub fn tail_divergence(
    enforced: &RunResult,
    ignored: &RunResult,
    quantiles: &[f64],
) -> Result<Fidelity, Unusable> {
    if enforced.ticks != ignored.ticks {
        return Err(Unusable::DifferentRuns {
            enforced: enforced.ticks,
            ignored: ignored.ticks,
        });
    }
    if enforced.equity_curve.is_empty() || ignored.equity_curve.is_empty() {
        return Err(Unusable::NoEquityCurve);
    }

    let a = Arm::of(enforced);
    let b = Arm::of(ignored);

    // The arms are the same account only for as long as both series
    // exist. `returns` truncates at zero equity, so the enforced arm's
    // series can simply end where the account did.
    let paired_until = a.returns.len().min(b.returns.len());
    let diverged_at = (0..paired_until).find(|&i| a.returns[i] != b.returns[i]);

    // A quantile at q needs enough samples that `q * n` lands on a real
    // observation rather than being rounded up to the first one, which
    // would silently report the minimum for every q below 1/n.
    if let Some(&smallest) = quantiles
        .iter()
        .filter(|q| q.is_finite())
        .min_by(|x, y| x.total_cmp(y))
        && smallest > 0.0
    {
        let need = (1.0 / smallest).ceil() as usize;
        if paired_until < need {
            return Err(Unusable::TooFewSamples {
                have: paired_until,
                need,
            });
        }
    }

    let tail = quantiles
        .iter()
        .map(|&q| TailPoint {
            q,
            enforced: quantile(&a.returns[..paired_until], q),
            ignored: quantile(&b.returns[..paired_until], q),
        })
        .collect();

    Ok(Fidelity {
        terminal_overstatement: Cash(b.final_equity.0 - a.final_equity.0),
        enforced: a,
        ignored: b,
        diverged_at,
        paired_until,
        tail,
    })
}

/// The `q`-th quantile of `xs`, by nearest rank on a sorted copy.
///
/// Nearest rank rather than interpolation: an interpolated tail quantile
/// is a number that was never observed, and the whole point of this
/// report is what the account actually went through.
fn quantile(xs: &[f64], q: f64) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut sorted: Vec<f64> = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (q.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------
// Across windows
// ---------------------------------------------------------------------

/// What one stress window produced under both arms.
///
/// The unit of a fidelity study is a *window*, not a tick. Within a
/// single run the paired return series ends at the liquidation, so the
/// paired quantiles are near-identical by construction and the damage is
/// entirely in the part that is no longer paired — running
/// [`tail_divergence`] on one window and reading its quantiles will show
/// almost nothing, which is a property of the instrument and not of the
/// strategy. Across windows the observation is one number per window,
/// every window is paired with itself, and nothing is truncated.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// What this window is, for the report to name.
    pub label: String,
    /// Total return over the window with liquidation modelled.
    pub enforced: f64,
    /// Total return over the window without it.
    pub ignored: f64,
    /// Whether the venue closed the account during this window.
    pub liquidated: bool,
}

impl Window {
    /// Build a window's outcome from the two runs over it.
    ///
    /// Both arms must have started from `starting`, which is the
    /// denominator of both returns; a zero or negative starting balance
    /// has no return defined and is refused.
    ///
    /// # Errors
    ///
    /// [`Unusable::DifferentRuns`] when the arms did not see the same
    /// ticks, [`Unusable::NoStartingBalance`] when `starting` is not
    /// positive.
    pub fn of(
        label: impl Into<String>,
        starting: Cash,
        enforced: &RunResult,
        ignored: &RunResult,
    ) -> Result<Self, Unusable> {
        if enforced.ticks != ignored.ticks {
            return Err(Unusable::DifferentRuns {
                enforced: enforced.ticks,
                ignored: ignored.ticks,
            });
        }
        if starting.0 <= 0 {
            return Err(Unusable::NoStartingBalance);
        }
        let base = starting.0 as f64;
        Ok(Self {
            label: label.into(),
            enforced: (enforced.final_equity.0 - starting.0) as f64 / base,
            ignored: (ignored.final_equity.0 - starting.0) as f64 / base,
            liquidated: !enforced.liquidations.is_empty(),
        })
    }

    /// How much better the margin-free arm made this window look.
    #[must_use]
    pub fn overstatement(&self) -> f64 {
        self.ignored - self.enforced
    }
}

/// The cross-window margin fidelity report — the G5 deliverable.
#[derive(Debug, Clone, PartialEq)]
pub struct StressReport {
    /// Windows studied.
    pub windows: usize,
    /// Windows in which the venue closed the account.
    pub liquidated: usize,
    /// Quantiles of both arms' per-window return distributions.
    pub tail: Vec<TailPoint>,
    /// Mean per-window overstatement — the number a naive comparison
    /// reports.
    ///
    /// **It is a function of the window mix, not of the strategy.**
    /// Double the proportion of stressed windows and this roughly
    /// doubles, without anything about the strategy having changed. It
    /// is reported so that a study which quotes it has to quote its mix
    /// alongside, and so a reader can compare it against the statistics
    /// below that do not move with the mix.
    pub mean_overstatement: f64,
    /// Share of the total overstatement contributed by the worst decile
    /// of windows, in `[0, 1]`, or `None` when there is no overstatement
    /// to apportion.
    ///
    /// **This is the headline.** A value near 0.1 would mean the error
    /// is spread evenly and a mean is an adequate summary. Values near
    /// 1.0 mean it is one window in ten carrying essentially all of it,
    /// which is what makes averaged reporting of margin-free backtests
    /// misleading rather than merely imprecise.
    ///
    /// This too moves with the mix — a study of nothing but crashes has
    /// its error spread evenly across all of them and would report a
    /// share near 0.1. It says *whether averaging is adequate for this
    /// study*, which is a narrower claim than it looks.
    pub worst_decile_share: Option<f64>,
    /// Mean return in the windows where the venue closed the account:
    /// `(what the account got, what the margin-free run claimed)`.
    /// `None` when no window liquidated.
    ///
    /// **This is the statistic that does not move with the mix.** It is
    /// conditional on liquidation having happened, so adding or removing
    /// calm windows does not touch it. It is also the sentence a reader
    /// actually needs: in the windows that decided the account, the
    /// margin-free backtest reported *this* and the account got *that*.
    pub given_liquidation: Option<(f64, f64)>,
    /// Every window, in the order given, so a reader can find the ones
    /// the tail is describing rather than being handed only a summary.
    pub per_window: Vec<Window>,
}

impl StressReport {
    /// Windows where the margin-free arm claimed a better result.
    #[must_use]
    pub fn overstated_windows(&self) -> usize {
        self.per_window
            .iter()
            .filter(|w| w.overstatement() > 0.0)
            .count()
    }
}

/// Study a set of stress windows run under both margin modes.
///
/// `quantiles` are points of the per-window return distribution, each in
/// `(0, 1)`; `&[0.01, 0.05, 0.10]` is the usual left tail.
///
/// # Errors
///
/// [`Unusable::TooFewSamples`] when there are fewer windows than the
/// finest requested quantile can land on.
pub fn stress(windows: &[Window], quantiles: &[f64]) -> Result<StressReport, Unusable> {
    if let Some(&smallest) = quantiles
        .iter()
        .filter(|q| q.is_finite() && **q > 0.0)
        .min_by(|x, y| x.total_cmp(y))
    {
        let need = (1.0 / smallest).ceil() as usize;
        if windows.len() < need {
            return Err(Unusable::TooFewSamples {
                have: windows.len(),
                need,
            });
        }
    }

    let enforced: Vec<f64> = windows.iter().map(|w| w.enforced).collect();
    let ignored: Vec<f64> = windows.iter().map(|w| w.ignored).collect();

    let mut gaps: Vec<f64> = windows.iter().map(Window::overstatement).collect();
    let total: f64 = gaps.iter().filter(|g| **g > 0.0).sum();
    // Sorting descending puts the worst windows first; the worst decile
    // is at least one window whenever there is any window at all, so a
    // ten-window study still has a meaningful answer.
    gaps.sort_by(|a, b| b.total_cmp(a));
    let decile = gaps.len().div_ceil(10);
    let worst_decile_share = (total > 0.0).then(|| {
        let top: f64 = gaps[..decile].iter().filter(|g| **g > 0.0).sum();
        top / total
    });

    Ok(StressReport {
        windows: windows.len(),
        liquidated: windows.iter().filter(|w| w.liquidated).count(),
        tail: quantiles
            .iter()
            .map(|&q| TailPoint {
                q,
                enforced: quantile(&enforced, q),
                ignored: quantile(&ignored, q),
            })
            .collect(),
        mean_overstatement: if windows.is_empty() {
            0.0
        } else {
            windows.iter().map(Window::overstatement).sum::<f64>() / windows.len() as f64
        },
        worst_decile_share,
        given_liquidation: {
            let hit: Vec<&Window> = windows.iter().filter(|w| w.liquidated).collect();
            (!hit.is_empty()).then(|| {
                let n = hit.len() as f64;
                (
                    hit.iter().map(|w| w.enforced).sum::<f64>() / n,
                    hit.iter().map(|w| w.ignored).sum::<f64>() / n,
                )
            })
        },
        per_window: windows.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(curve: &[i64], liquidations: usize, ticks: usize) -> RunResult {
        let mut r = RunResult {
            strategy: "t".into(),
            fills: Vec::new(),
            liquidations: Vec::new(),
            ticks,
            final_equity: Cash(*curve.last().unwrap_or(&0)),
            realized: Cash(0),
            funding_paid: Cash(0),
            fees_paid: Cash(0),
            min_equity: Cash(curve.iter().copied().min().unwrap_or(0)),
            equity_curve: curve.iter().map(|&c| Cash(c)).collect(),
            max_adverse_ticks: 0,
            margin_usage: crate::run::MarginUsage::NotTracked,
            tier: "L0",
            depth_applied: 0,
            depth_refused: 0,
            depth_unused: 0,
            misrouted_orders: 0,
        };
        for _ in 0..liquidations {
            r.liquidations.push(crate::run::Liquidation {
                at: oq_types::Nanos(0),
                price: oq_types::PriceTicks(0),
                qty: oq_types::QtyLots(0),
                equity: Cash(0),
            });
        }
        r
    }

    /// The whole report rests on the arms being the same experiment. Two
    /// runs over different data produce a perfectly plausible-looking
    /// divergence that measures nothing at all, so this is refused
    /// rather than reported.
    #[test]
    fn two_runs_over_different_ticks_are_not_a_comparison() {
        let a = result(&[100; 200], 0, 500);
        let b = result(&[100; 200], 0, 501);
        assert_eq!(
            tail_divergence(&a, &b, &[0.05]),
            Err(Unusable::DifferentRuns {
                enforced: 500,
                ignored: 501
            })
        );
    }

    /// A quantile below `1/n` cannot land on an observation, so it would
    /// silently return the minimum for every such q — reporting the 1st
    /// and the 0.1th percentile as the same number and hiding that the
    /// data could not tell them apart.
    #[test]
    fn a_quantile_finer_than_the_data_is_refused_rather_than_rounded() {
        let curve: Vec<i64> = (0..20).map(|i| 100 + i).collect();
        let a = result(&curve, 0, 10);
        let b = result(&curve, 0, 10);
        assert_eq!(
            tail_divergence(&a, &b, &[0.01]),
            Err(Unusable::TooFewSamples {
                have: 19,
                need: 100
            })
        );
        // The same data supports a coarser quantile.
        assert!(tail_divergence(&a, &b, &[0.10]).is_ok());
    }

    /// Identical arms are the honest answer that the overlay never bit
    /// on this data, not evidence that margin does not matter.
    #[test]
    fn arms_that_never_diverge_report_no_divergence() {
        let curve: Vec<i64> = (0..50).map(|i| 1000 + i * 3).collect();
        let a = result(&curve, 0, 100);
        let b = result(&curve, 0, 100);
        let f = tail_divergence(&a, &b, &[0.05, 0.25]).expect("comparable");
        assert_eq!(f.diverged_at, None);
        assert!(!f.margin_free_traded_a_dead_account());
        assert_eq!(f.terminal_overstatement, Cash(0));
        for p in &f.tail {
            assert_eq!(p.overstatement(), 0.0, "identical arms cannot differ");
        }
    }

    /// The point of the whole module: the arms agree, then the venue
    /// closes the account, and from there the margin-free arm is
    /// describing a position that could not have been held.
    ///
    /// The fixture matters. Liquidation happens when maintenance margin
    /// is breached, which is *above* zero equity — so the realistic
    /// shape is an enforced arm that goes to zero while the margin-free
    /// arm merely dips and recovers. That is precisely the case a
    /// margin-free backtest reports as a profitable strategy.
    #[test]
    fn liquidation_is_where_the_arms_stop_being_the_same_account() {
        let mut enforced = Vec::new();
        let mut ignored = Vec::new();
        for i in 0..40i64 {
            // Both fall together for 25 samples.
            let fall = 1000 - i * 38;
            enforced.push(if i < 26 { fall.max(0) } else { 0 });
            // The margin-free arm bottoms out just above zero and then
            // recovers past where it started — the strategy "works", as
            // long as nobody closes the position at the bottom.
            ignored.push(if i < 26 {
                fall.max(20)
            } else {
                20 + (i - 25) * 130
            });
        }

        let a = result(&enforced, 1, 100);
        let b = result(&ignored, 0, 100);
        let f = tail_divergence(&a, &b, &[0.10]).expect("comparable");

        assert!(
            f.margin_free_traded_a_dead_account(),
            "the enforced arm was liquidated; that is the fact the report exists to state"
        );
        assert!(
            f.terminal_overstatement.0 > 0,
            "the margin-free arm claims equity the real account never had, got {:?}",
            f.terminal_overstatement
        );
        // `returns` ends a series at zero equity, so the enforced arm's
        // stops where the account did while the margin-free arm's runs
        // on. That gap is the pairing boundary.
        assert!(
            f.paired_until < f.ignored.returns.len(),
            "pairing must end where the account did: paired_until={} ignored has {}",
            f.paired_until,
            f.ignored.returns.len()
        );
        assert!(
            f.diverged_at.is_some(),
            "arms that end at different equities must have diverged somewhere"
        );
    }

    /// The most damning case is the one the return series cannot show:
    /// a margin-free arm whose equity goes *negative*. `returns` ends
    /// the series there, so the tail statistics quietly stop before the
    /// worst of it — and the report would understate the error if the
    /// terminal number were derived from the same truncated series.
    /// It is not; it comes from the runs' final equity.
    #[test]
    fn negative_equity_is_still_caught_even_though_returns_stops_at_it() {
        let enforced: Vec<i64> = (0..30)
            .map(|i| if i < 20 { 1000 - i * 50 } else { 0 })
            .collect();
        let ignored: Vec<i64> = (0..30).map(|i| 1000 - i * 60).collect(); // ends deeply negative

        let f = tail_divergence(&result(&enforced, 1, 50), &result(&ignored, 0, 50), &[0.25])
            .expect("comparable");

        assert!(f.margin_free_traded_a_dead_account());
        assert!(
            f.terminal_overstatement.0 < 0,
            "here the margin-free arm ends *below* the closed account, and the report \
             must say so rather than assuming the error only ever runs one way: got {:?}",
            f.terminal_overstatement
        );
    }

    /// Nearest rank, not interpolation — the reported tail number has to
    /// be one the account actually experienced.
    #[test]
    fn a_reported_quantile_is_an_observation_and_not_an_average_of_two() {
        let xs = [-0.5, -0.1, 0.0, 0.2, 0.4];
        for q in [0.05, 0.2, 0.4, 0.6, 0.99] {
            let got = quantile(&xs, q);
            assert!(
                xs.contains(&got),
                "q={q} produced {got}, which is not an observation"
            );
        }
        assert_eq!(
            quantile(&xs, 0.2),
            -0.5,
            "the 20th percentile of 5 is the 1st"
        );
        assert_eq!(quantile(&xs, 1.0), 0.4);
    }

    /// The headline number a reader wants is the worst point, and it has
    /// to come from the same set that was reported.
    #[test]
    fn the_worst_point_is_one_of_the_reported_points() {
        let enforced: Vec<i64> = (0..40).map(|i| 1000 - i * 5).collect();
        let ignored: Vec<i64> = (0..40).map(|i| 1000 + i * 5).collect();
        let f = tail_divergence(
            &result(&enforced, 0, 10),
            &result(&ignored, 0, 10),
            &[0.05, 0.10, 0.25],
        )
        .expect("comparable");
        let worst = f.worst_overstatement().expect("quantiles were requested");
        assert!(f.tail.contains(&worst));
        assert!(
            worst.overstatement() > 0.0,
            "a falling arm against a rising one must overstate"
        );
    }

    /// Asking for nothing is not an error, but it must not pretend to
    /// have found a worst case.
    #[test]
    fn no_quantiles_means_no_worst_case() {
        let curve: Vec<i64> = (0..50).map(|i| 1000 + i).collect();
        let f = tail_divergence(&result(&curve, 0, 10), &result(&curve, 0, 10), &[])
            .expect("comparable");
        assert!(f.tail.is_empty());
        assert_eq!(f.worst_overstatement(), None);
    }

    fn window(label: &str, enforced: f64, ignored: f64, liq: bool) -> Window {
        Window {
            label: label.into(),
            enforced,
            ignored,
            liquidated: liq,
        }
    }

    /// The methodological claim of the whole report, stated as a test:
    /// margin error is not spread across windows, it is carried by a
    /// few. If this ever came out near 0.1 the mean would be an
    /// adequate summary and the tail report would not need to exist.
    #[test]
    fn the_error_is_concentrated_in_a_few_windows_not_spread_over_all() {
        // Twenty quiet windows where the overlay never bit, and two
        // where the account was closed. This is the realistic shape:
        // almost every drawdown retraces, and the one that does not is
        // the one that ends the account.
        let mut ws: Vec<Window> = (0..20)
            .map(|i| window(&format!("calm-{i}"), 0.02, 0.02, false))
            .collect();
        ws.push(window("crash-a", -0.98, 0.35, true));
        ws.push(window("crash-b", -0.99, 0.20, true));

        let r = stress(&ws, &[0.05, 0.25, 0.50]).expect("22 windows support a 5th percentile");

        assert_eq!(r.windows, 22);
        assert_eq!(r.liquidated, 2);
        assert_eq!(r.overstated_windows(), 2, "only the crashes differ");

        let share = r
            .worst_decile_share
            .expect("there is overstatement to apportion");
        assert!(
            share > 0.9,
            "the worst decile must carry nearly all of the error, got {share}"
        );

        // And the number a naive report would print is an order of
        // magnitude smaller than what any single affected window
        // actually lost, which is exactly the failure mode: averaging
        // turns two ruined accounts into a modest-looking bias.
        let worst_window = ws
            .iter()
            .map(Window::overstatement)
            .fold(f64::MIN, f64::max);
        assert!(
            r.mean_overstatement * 10.0 < worst_window,
            "the mean ({}) has to be far smaller than the worst window ({worst_window}) \
             or averaging would not be hiding anything",
            r.mean_overstatement
        );
    }

    /// The tail is where the two arms visibly part; the median is where
    /// they do not. Reporting both is what lets a reader see that the
    /// difference is a tail property rather than a level shift.
    #[test]
    fn the_median_agrees_where_the_tail_does_not() {
        let mut ws: Vec<Window> = (0..18)
            .map(|i| window(&format!("calm-{i}"), 0.01, 0.01, false))
            .collect();
        ws.push(window("crash-a", -0.95, 0.40, true));
        ws.push(window("crash-b", -0.90, 0.30, true));

        let r = stress(&ws, &[0.05, 0.50]).expect("20 windows");
        let p5 = r.tail.iter().find(|p| p.q == 0.05).expect("requested");
        let p50 = r.tail.iter().find(|p| p.q == 0.50).expect("requested");

        assert_eq!(
            p50.overstatement(),
            0.0,
            "the middle of the distribution agrees"
        );
        assert!(
            p5.overstatement() > 0.5,
            "the 5th percentile must show most of the gap, got {}",
            p5.overstatement()
        );

        // The sharpest way to put it: the crash windows are in the
        // enforced arm's *left* tail and in the margin-free arm's
        // *right* tail. They are the same two windows. A margin-free
        // backtest does not merely understate the risk of these
        // windows — it files them under success.
        assert!(
            p5.enforced < -0.5,
            "the crashes are the enforced arm's worst outcomes, got {}",
            p5.enforced
        );
        assert!(
            p5.ignored > 0.0,
            "and they are nowhere near the margin-free arm's worst, got {}",
            p5.ignored
        );
        let best_ignored = ws.iter().map(|w| w.ignored).fold(f64::MIN, f64::max);
        assert!(
            ws.iter()
                .any(|w| w.liquidated && (w.ignored - best_ignored).abs() < 1e-12),
            "the margin-free arm's best window must be one that liquidated the real account"
        );
    }

    /// A study with no liquidations is a real answer — margin did not
    /// matter for this strategy on this data — and must not be dressed
    /// up as one.
    #[test]
    fn a_study_where_the_overlay_never_bit_reports_nothing_rather_than_something() {
        let ws: Vec<Window> = (0..10)
            .map(|i| {
                window(
                    &format!("w-{i}"),
                    0.03 - f64::from(i) * 0.01,
                    0.03 - f64::from(i) * 0.01,
                    false,
                )
            })
            .collect();
        let r = stress(&ws, &[0.10, 0.50]).expect("10 windows support a 10th percentile");
        assert_eq!(r.liquidated, 0);
        assert_eq!(r.mean_overstatement, 0.0);
        assert_eq!(
            r.worst_decile_share, None,
            "with no error there is nothing to apportion, and 0/0 must not be reported as a share"
        );
    }

    /// Fewer windows than the quantile can land on is refused for the
    /// same reason as within a run: a 1st percentile of nine windows is
    /// the minimum wearing a percentile's name.
    #[test]
    fn a_study_too_small_for_the_quantile_is_refused() {
        let ws: Vec<Window> = (0..9)
            .map(|i| window(&format!("w-{i}"), 0.0, 0.0, false))
            .collect();
        assert_eq!(
            stress(&ws, &[0.05]),
            Err(Unusable::TooFewSamples { have: 9, need: 20 })
        );
        assert!(stress(&ws, &[0.5]).is_ok());
    }

    /// A window return needs a denominator, and the two runs have to be
    /// the same experiment — the same two refusals as within a run.
    #[test]
    fn a_window_will_not_be_built_from_incomparable_runs() {
        let a = result(&[100, 90], 0, 50);
        let b = result(&[100, 130], 0, 51);
        assert!(matches!(
            Window::of("w", Cash(100), &a, &b),
            Err(Unusable::DifferentRuns { .. })
        ));

        let b = result(&[100, 130], 0, 50);
        assert_eq!(
            Window::of("w", Cash(0), &a, &b),
            Err(Unusable::NoStartingBalance)
        );

        let w = Window::of("w", Cash(100), &a, &b).expect("comparable");
        assert_eq!(w.enforced, -0.1);
        assert_eq!(w.ignored, 0.3);
        assert!((w.overstatement() - 0.4).abs() < 1e-12);
    }

    /// The decile has to be at least one window, or a small study would
    /// report a share of nothing.
    #[test]
    fn the_worst_decile_of_a_small_study_is_still_a_window() {
        let mut ws: Vec<Window> = (0..4)
            .map(|i| window(&format!("w-{i}"), 0.01, 0.01, false))
            .collect();
        ws.push(window("bad", -0.5, 0.5, true));
        let r = stress(&ws, &[0.5]).expect("5 windows");
        assert_eq!(
            r.worst_decile_share,
            Some(1.0),
            "one window carries all of it"
        );
    }

    /// The point of `given_liquidation`: adding calm windows changes the
    /// mean and the decile share, and must not change it. A statistic
    /// that moves when you pad the study with irrelevant data cannot be
    /// quoted on its own, and this one can.
    #[test]
    fn the_conditional_statistic_does_not_move_when_the_mix_does() {
        let crashes = vec![
            window("crash-a", -0.95, 0.40, true),
            window("crash-b", -0.90, 0.30, true),
        ];

        let mut lean = crashes.clone();
        lean.extend((0..8).map(|i| window(&format!("calm-{i}"), 0.01, 0.01, false)));

        let mut padded = crashes;
        padded.extend((0..98).map(|i| window(&format!("calm-{i}"), 0.01, 0.01, false)));

        let a = stress(&lean, &[0.5]).expect("10 windows");
        let b = stress(&padded, &[0.5]).expect("100 windows");

        assert_eq!(
            a.given_liquidation, b.given_liquidation,
            "the conditional statistic must survive padding the study"
        );
        assert!(
            b.mean_overstatement * 5.0 < a.mean_overstatement,
            "whereas the mean collapses: {} vs {}",
            b.mean_overstatement,
            a.mean_overstatement
        );

        let (real, claimed) = a.given_liquidation.expect("two windows liquidated");
        assert!(
            real < -0.9 && claimed > 0.3,
            "got real {real}, claimed {claimed}"
        );
    }

    /// No liquidation means there is no conditional statistic, and
    /// reporting a zero would read as "the margin-free run was right"
    /// rather than "the question did not arise".
    #[test]
    fn no_liquidation_means_no_conditional_statistic() {
        let ws: Vec<Window> = (0..10)
            .map(|i| window(&format!("w-{i}"), 0.01, 0.01, false))
            .collect();
        assert_eq!(stress(&ws, &[0.5]).expect("10").given_liquidation, None);
    }
}
