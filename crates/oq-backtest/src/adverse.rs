//! What happens to a strategy right after it is filled.
//!
//! A resting order is filled when somebody chooses to trade against it,
//! and the people who choose are disproportionately the ones who know
//! where the price is going. So a strategy that makes liquidity is
//! filled, on average, just before the market moves against it — adverse
//! selection — and a backtest that fills it at the touch whenever the
//! price trades there has no way to know that, unless it looks.
//!
//! This looks. Each fill is marked out at a set of horizons with
//! [`oq_parity::markout`], the same measurement `oq-parity markout`
//! makes on a live run, against the same price (the last trade), so a
//! backtest's number and a live run's can be set side by side with
//! [`oq_parity::markout::contrast`].
//!
//! # A gate, not a footnote
//!
//! A markout is a diagnostic until something acts on it. For a strategy
//! whose fills are mostly maker fills it is the number that decides
//! whether the spread it books is real, so [`AdverseReport::refusals`]
//! refuses such a strategy when its makers' markout cannot be measured
//! — too few fills is not the same as no adverse selection — or when
//! the market moved against its maker fills by more, on average, than
//! the fill price gained it.
//!
//! L0 stamps a fill caused by the price passing through an order with
//! the observation *before* the one that revealed it, so the move that
//! crossed the order falls inside the shortest horizon. That is the
//! conservative reading, and for a maker it is the real one: a bid is
//! filled because the price came down through it.
//!
//! The markout is taken from the fill price, so the half-spread a maker
//! earns is already inside it. A mean below zero means the moves after
//! the fills cost more than the spread the fills captured: the strategy
//! is paid to stand in front of informed flow and paid too little.

use oq_engine::Tick;
use oq_parity::markout::{DEFAULT_HORIZONS, Markout, Point, markout};
use oq_parity::record::{Fill as ParityFill, Nanos as ParityNanos};
use oq_types::{Fill, Liquidity};

use crate::run::RunResult;

/// Where a run's fills went afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct AdverseReport {
    /// Share of fills that made liquidity, or `None` with no fills.
    pub maker_share: Option<f64>,
    /// Markouts of the maker fills, one per horizon.
    pub maker: Vec<Markout>,
    /// Markouts of the taker fills, one per horizon.
    pub taker: Vec<Markout>,
}

/// When a maker strategy's markout refuses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdverseThresholds {
    /// The share of maker fills at or above which the run is judged as a
    /// maker strategy.
    pub maker_share: f64,
    /// The smallest acceptable mean maker markout, in basis points, at
    /// every horizon.
    pub min_mean_bps: f64,
}

impl Default for AdverseThresholds {
    /// Half, because past it the strategy's result is mostly what its
    /// resting orders earned; zero, because that is where the moves after
    /// the fills start costing more than the spread they captured. The
    /// second is the definition's own boundary rather than a choice.
    fn default() -> Self {
        Self {
            maker_share: 0.5,
            min_mean_bps: 0.0,
        }
    }
}

/// Why a maker strategy's fills must not be trusted as they stand.
#[derive(Debug, Clone, PartialEq)]
pub enum AdverseRefusal {
    /// The market moved against the maker fills by more than they earned.
    Selected {
        /// Horizon, in nanoseconds.
        horizon_ns: i64,
        /// Mean maker markout at that horizon, in basis points.
        mean_bps: f64,
        /// Share of maker fills the market moved against.
        adverse_share: f64,
        /// What the mean had to be at least.
        limit: f64,
    },
    /// Too few maker fills to measure at a horizon.
    Unmeasured {
        /// Horizon, in nanoseconds.
        horizon_ns: i64,
        /// Fills that could be marked out there.
        samples: usize,
    },
}

impl core::fmt::Display for AdverseRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Selected {
                horizon_ns,
                mean_bps,
                adverse_share,
                limit,
            } => write!(
                f,
                "maker fills marked out {} later average {mean_bps:+.2} bps, below {limit:+.2}, \
                 with {:.0}% moving against them: the spread these orders book is less than \
                 what being filled costs",
                horizon(*horizon_ns),
                adverse_share * 100.0
            ),
            Self::Unmeasured {
                horizon_ns,
                samples,
            } => write!(
                f,
                "only {samples} maker fill(s) can be marked out {} later, fewer than {}: \
                 a maker strategy whose adverse selection was not measured is not one that \
                 has none",
                horizon(*horizon_ns),
                oq_parity::markout::MIN_SAMPLES
            ),
        }
    }
}

fn horizon(ns: i64) -> String {
    if ns % 1_000_000_000 == 0 {
        format!("{} s", ns / 1_000_000_000)
    } else {
        format!("{} ms", ns / 1_000_000)
    }
}

/// Mark out `result`'s fills against the ticks it was run on, at the
/// default horizons of one, ten and sixty seconds.
#[must_use]
pub fn adverse_selection(result: &RunResult, ticks: &[Tick]) -> AdverseReport {
    adverse_selection_at(result, ticks, &DEFAULT_HORIZONS)
}

/// As [`adverse_selection`], at the given horizons.
#[must_use]
pub fn adverse_selection_at(
    result: &RunResult,
    ticks: &[Tick],
    horizons: &[ParityNanos],
) -> AdverseReport {
    let of = |liquidity: Liquidity| -> Vec<ParityFill> {
        result
            .fills
            .iter()
            .filter(|f| f.liquidity == liquidity)
            .map(parity)
            .collect()
    };
    let path = || {
        ticks.iter().map(|t| Point {
            at: ParityNanos(t.stamp.exch.0),
            price: t.last,
        })
    };
    let makers = of(Liquidity::Maker);
    let maker_share =
        (!result.fills.is_empty()).then(|| makers.len() as f64 / result.fills.len() as f64);
    AdverseReport {
        maker_share,
        maker: markout(&makers, path(), horizons),
        taker: markout(&of(Liquidity::Taker), path(), horizons),
    }
}

fn parity(fill: &Fill) -> ParityFill {
    ParityFill::new(fill.stamp.exch.0, "", fill.side, fill.price.0, fill.qty.0)
}

impl AdverseReport {
    /// Whether the run counts as a maker strategy under `thresholds`.
    #[must_use]
    pub fn is_maker(&self, thresholds: AdverseThresholds) -> bool {
        self.maker_share
            .is_some_and(|share| share >= thresholds.maker_share)
    }

    /// Every reason this run's maker fills cannot be trusted, at every
    /// horizon. Empty for a run that is not a maker strategy: a taker's
    /// markout is the signal it traded on, not a cost it was selected
    /// into.
    #[must_use]
    pub fn refusals(&self, thresholds: AdverseThresholds) -> Vec<AdverseRefusal> {
        if !self.is_maker(thresholds) {
            return Vec::new();
        }
        self.maker
            .iter()
            .filter_map(|m| match m {
                Markout::TooFew { horizon, samples } => Some(AdverseRefusal::Unmeasured {
                    horizon_ns: horizon.0,
                    samples: *samples,
                }),
                Markout::Measured(d) if d.mean_bps < thresholds.min_mean_bps => {
                    Some(AdverseRefusal::Selected {
                        horizon_ns: d.horizon.0,
                        mean_bps: d.mean_bps,
                        adverse_share: d.adverse_share,
                        limit: thresholds.min_mean_bps,
                    })
                }
                Markout::Measured(_) => None,
            })
            .collect()
    }

    /// The whole report on one line: the maker share, then the maker
    /// markout at each horizon.
    #[must_use]
    pub fn summary(&self) -> String {
        let share = self.maker_share.map_or_else(
            || "no fills".to_string(),
            |s| format!("maker {:.1}%", s * 100.0),
        );
        let horizons: Vec<String> = self
            .maker
            .iter()
            .map(|m| match m {
                Markout::Measured(d) => format!(
                    "{} {:+.2} bps ({:.0}% against, n={})",
                    horizon(d.horizon.0),
                    d.mean_bps,
                    d.adverse_share * 100.0,
                    d.samples
                ),
                Markout::TooFew {
                    horizon: h,
                    samples,
                } => {
                    format!("{} too few ({samples})", horizon(h.0))
                }
            })
            .collect();
        if horizons.is_empty() {
            share
        } else {
            format!("{share}: {}", horizons.join(", "))
        }
    }

    /// One line per horizon, for a report.
    #[must_use]
    pub fn render(&self) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();
        let _ = match self.maker_share {
            Some(s) => writeln!(out, "maker share      {:.1}%", s * 100.0),
            None => writeln!(out, "maker share      - (no fills)"),
        };
        for (name, set) in [("maker", &self.maker), ("taker", &self.taker)] {
            for m in set {
                let _ = match m {
                    Markout::Measured(d) => writeln!(
                        out,
                        "{name} markout {:>6}  mean {:+.2} bps  median {:+.2}  against {:.0}%  n={}",
                        horizon(d.horizon.0),
                        d.mean_bps,
                        d.median_bps,
                        d.adverse_share * 100.0,
                        d.samples
                    ),
                    Markout::TooFew {
                        horizon: h,
                        samples,
                    } => writeln!(
                        out,
                        "{name} markout {:>6}  too few to say ({samples})",
                        horizon(h.0)
                    ),
                };
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RunConfig, run_stream, tick_at};
    use oq_margin::{Contract, TierTable};
    use oq_strategy::{Context, Intent, Strategy};
    use oq_types::{Cash, InstrumentId, OrderId, PriceTicks, QtyLots, Side};

    const S: i64 = 1_000_000_000;

    fn config() -> RunConfig {
        RunConfig::new(
            InstrumentId::new(1),
            Contract::new(1_000),
            TierTable::example_btcusdt(),
            Cash::from_units(1_000_000),
        )
    }

    /// A staircase: every step down is followed by more steps down, so a
    /// bid resting one tick under the market is filled into a fall.
    fn falling(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let p = 6_000_000 - i as i64 * 20;
                tick_at(i as i64 * S, p, p, p)
            })
            .collect()
    }

    /// A flat market observed every half second with a dip every ninth
    /// observation, so a resting bid is filled only in the dip and the
    /// price comes back. The fill is stamped at the observation before
    /// the dip (L0's convention), and nine is chosen so that none of the
    /// 1, 10 and 60 s horizons from there lands in a dip.
    fn mean_reverting(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let p = 6_000_000 + [0, 0, 0, -40, 0, 0, 0, 0, 0][i % 9];
                tick_at(i as i64 * S / 2, p, p, p)
            })
            .collect()
    }

    /// Rests a one-lot bid a tick under the last price, every tick.
    struct Bidder {
        next: u64,
    }

    impl Strategy for Bidder {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            out.push(Intent::CancelAll);
            self.next += 1;
            out.push(ctx.limit(
                OrderId::new(self.next),
                Side::Buy,
                PriceTicks(ctx.tick.last.0 - 1),
                QtyLots(1),
            ));
        }

        fn name(&self) -> &str {
            "bidder"
        }
    }

    fn run(ticks: &[Tick]) -> RunResult {
        let mut s = Bidder { next: 0 };
        run_stream(&config(), &mut s, ticks.iter().copied())
    }

    #[test]
    fn a_bid_filled_into_a_falling_market_is_refused() {
        let ticks = falling(200);
        let result = run(&ticks);
        let report = adverse_selection(&result, &ticks);
        assert!(report.is_maker(AdverseThresholds::default()), "{report:?}");
        let refusals = report.refusals(AdverseThresholds::default());
        assert_eq!(refusals.len(), 3, "every horizon: {refusals:?}");
        assert!(
            refusals
                .iter()
                .all(|r| matches!(r, AdverseRefusal::Selected { .. }))
        );
        assert!(
            refusals[0]
                .to_string()
                .contains("less than what being filled costs")
        );
    }

    #[test]
    fn a_bid_filled_on_wobbles_that_revert_is_not() {
        let ticks = mean_reverting(400);
        let result = run(&ticks);
        let report = adverse_selection(&result, &ticks);
        assert!(report.is_maker(AdverseThresholds::default()), "{report:?}");
        assert_eq!(
            report.refusals(AdverseThresholds::default()),
            Vec::new(),
            "{}",
            report.render()
        );
    }

    #[test]
    fn too_few_maker_fills_is_refused_rather_than_passed() {
        let ticks = falling(12);
        let result = run(&ticks);
        let report = adverse_selection(&result, &ticks);
        let refusals = report.refusals(AdverseThresholds::default());
        assert!(!refusals.is_empty());
        assert!(
            refusals
                .iter()
                .all(|r| matches!(r, AdverseRefusal::Unmeasured { .. })),
            "{refusals:?}"
        );
        assert!(refusals[0].to_string().contains("not one that has none"));
    }

    #[test]
    fn a_run_with_no_fills_is_not_a_maker_strategy() {
        let report = adverse_selection(&run(&[]), &[]);
        assert_eq!(report.maker_share, None);
        assert!(report.refusals(AdverseThresholds::default()).is_empty());
    }
}
