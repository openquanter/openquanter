//! Whether a run's result is worth acting on.
//!
//! `FR-MATCH-3` calls this the **fidelity report**: participation rate,
//! maker/taker split, latency assumptions, impact deductions, and —
//! when margin is enabled — peak margin usage and closest approach to
//! liquidation. `FR-MATCH-4` adds the flag: past a threshold
//! participation rate the run is not valid, because a replay-based
//! backtest stops being one once the simulated strategy would have
//! moved the market it is replaying.
//!
//! The module is not called `fidelity` because [`crate::fidelity`]
//! already is, and answers a different question — what a backtest with
//! no margin model is worth. This one answers whether *this* run's
//! numbers describe something that could have happened.
//!
//! # The number that matters is the peak, not the average
//!
//! A strategy that takes 0.2% of the day's volume and 40% of one
//! minute's has invalidated that minute, and the day-long average says
//! it did not. So the report carries both, the flag is on the peak, and
//! the window it occurred in is named so a reader can go and look.
//!
//! # What L0 assumes, stated rather than omitted
//!
//! L0 models no latency and no impact. Those are two of the five things
//! `FR-MATCH-3` asks the report to carry, and the honest entry is not an
//! empty column — it is the assumption written down. A backtest that
//! reports nothing under "latency" reads as a backtest where latency was
//! zero, which is a claim, and it is the claim L0 is actually making.
//! Saying so is the difference between an assumption and an oversight.

use oq_engine::Tick;
use oq_types::{Cash, Fill, Liquidity, QtyLots};

use crate::run::{MarginUsage, RunResult};

/// The share of market volume a run took.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Participation {
    /// Measured.
    Measured {
        /// Traded volume divided by market volume, over the whole run.
        overall: f64,
        /// The largest share taken in any single window.
        peak: f64,
        /// Index of the window the peak occurred in.
        peak_window: usize,
        /// Ticks per window.
        window: usize,
    },
    /// The tick series does not carry usable volume.
    ///
    /// Cumulative volume that decreases is not cumulative, and a
    /// participation rate computed across the break is a ratio of two
    /// unrelated numbers. Refused rather than reported: this figure
    /// decides whether the whole run is valid, and a wrong one is worse
    /// than a missing one.
    Unmeasurable(&'static str),
}

impl Participation {
    /// The peak, when it was measured.
    #[must_use]
    pub const fn peak(&self) -> Option<f64> {
        match self {
            Self::Measured { peak, .. } => Some(*peak),
            Self::Unmeasurable(_) => None,
        }
    }
}

/// What the run assumed about the things its tier does not measure.
///
/// Carried as text because they are assumptions rather than
/// measurements, and because the report has to say *something* under
/// each heading — an empty column reads as a zero, and a zero here is a
/// claim nobody made deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assumptions {
    /// The tier that produced the run.
    ///
    /// Part of the assumptions rather than beside them, because the tier
    /// *is* an assumption — `oq-engine`'s ladder says a market-making
    /// P&L measured at L0 is not a pessimistic estimate but a different
    /// quantity with the same units, and a report that named the wrong
    /// tier would be the exact confusion it warns about.
    pub tier: String,
    /// What the matcher assumed about latency.
    pub latency: String,
    /// What it assumed about market impact.
    pub impact: String,
    /// What it assumed about queue position, when the tier has one.
    pub queue: Option<String>,
}

impl Assumptions {
    /// L0's assumptions, which are that none of the three exist.
    #[must_use]
    pub fn l0() -> Self {
        Self {
            tier: "L0".to_string(),
            latency: "none modelled: an order matches against the observation \
                      that triggered it, with no delay"
                .to_string(),
            impact: "none modelled: fills do not move the price, at any size".to_string(),
            queue: None,
        }
    }

    /// The assumptions an L1 policy actually holds.
    ///
    /// Taken from the policy rather than described here, so a report
    /// cannot drift from the engine that produced it — the failure this
    /// exists to prevent is a run at one tier reporting another's
    /// assumptions, and two sources for the same sentence is how that
    /// happens.
    #[must_use]
    pub fn of_l1(policy: &oq_engine::Policy) -> Self {
        if policy.models_nothing() {
            // Not L1's assumptions, because it is not making any. Naming
            // it L1 would put a higher tier on a lower-fidelity answer.
            let mut base = Self::l0();
            base.tier = "L1 (transparent: equivalent to L0)".to_string();
            return base;
        }
        Self {
            tier: "L1".to_string(),
            latency: format!(
                "entry {}, response {} — assumed, not measured. Feed latency is \
                 not modelled here: it belongs to the event producer.",
                policy.latency.entry.describe(),
                policy.latency.response.describe()
            ),
            impact: format!(
                "square-root penalty, coefficient {}.{:02} — assumed, not calibrated",
                policy.impact.coefficient / 100,
                policy.impact.coefficient % 100
            ),
            queue: Some(policy.describe()),
        }
    }
}

/// The fidelity report.
#[derive(Debug, Clone, PartialEq)]
pub struct FidelityReport {
    /// Share of market volume taken.
    pub participation: Participation,
    /// The threshold the flag is set against.
    pub threshold: f64,
    /// Fills that took liquidity, and fills that made it.
    pub taker_maker: (usize, usize),
    /// What the tier assumed about what it does not model.
    pub assumptions: Assumptions,
    /// How close the account came to liquidation.
    pub margin_usage: MarginUsage,
    /// Times the venue closed the account.
    pub liquidations: usize,
}

impl FidelityReport {
    /// Whether the run's participation makes its result unusable.
    ///
    /// `true` is a refusal to conclude, not a claim that the strategy is
    /// bad: the numbers describe a market the strategy would have
    /// changed, so they describe nothing.
    #[must_use]
    pub fn participation_invalidates(&self) -> bool {
        self.participation
            .peak()
            .is_some_and(|p| p > self.threshold)
    }

    /// Maker share of fills, or `None` when there were none.
    #[must_use]
    pub fn maker_share(&self) -> Option<f64> {
        let (taker, maker) = self.taker_maker;
        let total = taker + maker;
        (total > 0).then(|| maker as f64 / total as f64)
    }

    /// The report, as text.
    #[must_use]
    pub fn render(&self) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "fidelity report   tier {}", self.assumptions.tier);

        match self.participation {
            Participation::Measured {
                overall,
                peak,
                peak_window,
                window,
            } => {
                let _ = writeln!(
                    out,
                    "  participation   {:.4}% overall, peak {:.4}% in window {peak_window} \
                     ({window} ticks)",
                    overall * 100.0,
                    peak * 100.0
                );
                if self.participation_invalidates() {
                    let _ = writeln!(
                        out,
                        "                  OVER THRESHOLD ({:.2}%): this run replayed a market \
                         it would\n                  have moved, so its result describes \
                         something that could not\n                  have happened. The peak is \
                         what invalidates it, not the average.",
                        self.threshold * 100.0
                    );
                }
            }
            Participation::Unmeasurable(why) => {
                let _ = writeln!(out, "  participation   NOT MEASURABLE — {why}");
                let _ = writeln!(
                    out,
                    "                  This decides whether the run is valid, so a missing \
                     figure\n                  is not a passing one."
                );
            }
        }

        let (taker, maker) = self.taker_maker;
        match self.maker_share() {
            Some(share) => {
                let _ = writeln!(
                    out,
                    "  maker/taker     {maker} maker, {taker} taker ({:.1}% maker)",
                    share * 100.0
                );
            }
            None => {
                let _ = writeln!(out, "  maker/taker     no fills");
            }
        }

        if let Some(queue) = &self.assumptions.queue {
            let _ = writeln!(out, "  queue           {queue}");
        }
        let _ = writeln!(out, "  latency         {}", self.assumptions.latency);
        let _ = writeln!(out, "  impact          {}", self.assumptions.impact);

        match self.margin_usage {
            MarginUsage::NotTracked => {
                let _ = writeln!(
                    out,
                    "  margin          not tracked — pass RunConfig::tracking_margin() to \
                     measure it"
                );
            }
            MarginUsage::NoPosition => {
                let _ = writeln!(out, "  margin          no position was ever open");
            }
            MarginUsage::Tracked {
                peak_maintenance,
                min_headroom,
            } => {
                let money = |c: Cash| c.0 as f64 / 100_000_000.0;
                let _ = writeln!(
                    out,
                    "  margin          peak requirement {:.2}, closest approach {:.2}",
                    money(peak_maintenance),
                    money(min_headroom)
                );
                if min_headroom.0 <= 0 {
                    let _ = writeln!(
                        out,
                        "                  the account reached or passed the line"
                    );
                }
            }
        }

        if self.liquidations > 0 {
            let _ = writeln!(out, "  LIQUIDATED      {}x", self.liquidations);
        }
        out
    }
}

/// The default threshold: one percent of a window's volume.
///
/// Not derived from anything. It is the order of magnitude at which a
/// replay stops being credible for a liquid perpetual, chosen so the
/// flag fires before a reader would have to wonder — and it is a
/// parameter because the right number differs by venue and by
/// instrument.
pub const DEFAULT_THRESHOLD: f64 = 0.01;

/// Build the fidelity report for a run.
///
/// `window` is how many ticks a participation window spans. The peak
/// across windows is the figure the flag is set against; see the module
/// documentation for why the average is not.
#[must_use]
pub fn report(result: &RunResult, ticks: &[Tick], window: usize, threshold: f64) -> FidelityReport {
    report_at(result, ticks, window, threshold, Assumptions::l0())
}

/// The fidelity report for a run at a named tier.
///
/// Takes the assumptions rather than deriving them, because the engine
/// that produced the run is the only thing that knows which tier it was
/// running under — and a report that guessed would be `oq-engine`'s own
/// warning made real: a market-making P&L measured at L0 is not a
/// pessimistic estimate, it is a different quantity with the same units.
#[must_use]
pub fn report_at(
    result: &RunResult,
    ticks: &[Tick],
    window: usize,
    threshold: f64,
    assumptions: Assumptions,
) -> FidelityReport {
    FidelityReport {
        participation: participation(&result.fills, ticks, window),
        threshold,
        taker_maker: split(&result.fills),
        assumptions,
        margin_usage: result.margin_usage,
        liquidations: result.liquidations.len(),
    }
}

/// Fills that took liquidity, and fills that made it.
fn split(fills: &[Fill]) -> (usize, usize) {
    let taker = fills
        .iter()
        .filter(|f| f.liquidity == Liquidity::Taker)
        .count();
    (taker, fills.len() - taker)
}

/// Traded volume against market volume, overall and per window.
fn participation(fills: &[Fill], ticks: &[Tick], window: usize) -> Participation {
    if window == 0 {
        return Participation::Unmeasurable("a window of zero ticks spans no market");
    }
    if ticks.len() < 2 {
        return Participation::Unmeasurable("fewer than two observations, so no volume elapsed");
    }
    // Cumulative volume that goes backwards is not cumulative, and every
    // figure below would be a ratio of two unrelated numbers.
    if ticks.windows(2).any(|w| w[1].volume.0 < w[0].volume.0) {
        return Participation::Unmeasurable(
            "cumulative volume decreases somewhere in the series, so a share of it \
             is not defined",
        );
    }

    let market_total = ticks[ticks.len() - 1].volume.0 - ticks[0].volume.0;
    if market_total <= 0 {
        return Participation::Unmeasurable("no market volume elapsed over the run");
    }
    let traded_total: i64 = fills.iter().map(|f| f.qty.0).sum();

    // Fills are bucketed by timestamp against the tick series, so a
    // window's traded volume is the volume of fills that happened inside
    // it rather than an even spread of the total.
    let windows = ticks.len().div_ceil(window);
    let mut peak = 0.0f64;
    let mut peak_window = 0usize;
    for w in 0..windows {
        let start = w * window;
        let end = (start + window).min(ticks.len());
        if end - start < 2 {
            continue;
        }
        let market = ticks[end - 1].volume.0 - ticks[start].volume.0;
        if market <= 0 {
            continue;
        }
        let (from, to) = (ticks[start].stamp.exch.0, ticks[end - 1].stamp.exch.0);
        let traded: i64 = fills
            .iter()
            .filter(|f| f.stamp.exch.0 >= from && f.stamp.exch.0 <= to)
            .map(|f| f.qty.0)
            .sum();
        let share = traded as f64 / market as f64;
        if share > peak {
            peak = share;
            peak_window = w;
        }
    }

    Participation::Measured {
        overall: traded_total as f64 / market_total as f64,
        peak,
        peak_window,
        window,
    }
}

/// Total quantity traded, for a caller that wants it directly.
#[must_use]
pub fn traded_volume(fills: &[Fill]) -> QtyLots {
    QtyLots(fills.iter().map(|f| f.qty.0).sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{InstrumentId, Nanos, Offset, OrderId, PriceTicks, Side, Stamp, TradeId};

    fn tick(i: i64, volume: i64) -> Tick {
        Tick {
            stamp: Stamp {
                exch: Nanos(i * 1_000_000_000),
                local: Nanos(i * 1_000_000_000),
            },
            last: PriceTicks(6_000_000),
            high: PriceTicks(6_000_000),
            low: PriceTicks(6_000_000),
            bid: PriceTicks(5_999_999),
            ask: PriceTicks(6_000_001),
            volume: QtyLots(volume),
        }
    }

    fn fill(at: i64, qty: i64, liquidity: Liquidity) -> Fill {
        Fill {
            stamp: Stamp {
                exch: Nanos(at * 1_000_000_000),
                local: Nanos(at * 1_000_000_000),
            },
            instrument: InstrumentId::new(1),
            order: OrderId(1),
            trade: TradeId(1),
            side: Side::Buy,
            offset: Offset::Open,
            price: PriceTicks(6_000_000),
            qty: QtyLots(qty),
            liquidity,
        }
    }

    fn result(fills: Vec<Fill>, margin: MarginUsage) -> RunResult {
        RunResult {
            strategy: "t".into(),
            fills,
            liquidations: Vec::new(),
            ticks: 0,
            final_equity: Cash(0),
            realized: Cash(0),
            funding_paid: Cash(0),
            fees_paid: Cash(0),
            min_equity: Cash(0),
            equity_curve: Vec::new(),
            max_adverse_ticks: 0,
            margin_usage: margin,
            tier: "L0",
            depth_applied: 0,
            depth_refused: 0,
            depth_unused: 0,
        }
    }

    /// **The claim the whole module exists for.** A strategy that takes
    /// a fifth of a percent of the day and forty percent of one minute
    /// has invalidated that minute, and the day-long average says it
    /// did not.
    #[test]
    fn the_peak_catches_what_the_average_hides() {
        // 100 ticks, 100 units of market volume each: 9,900 in total.
        let ticks: Vec<Tick> = (0..100).map(|i| tick(i, i * 100)).collect();
        // Twenty lots, all inside one ten-tick window.
        let fills: Vec<Fill> = (0..4).map(|k| fill(30 + k, 5, Liquidity::Taker)).collect();

        let r = report(&result(fills, MarginUsage::NotTracked), &ticks, 10, 0.01);
        let Participation::Measured {
            overall,
            peak,
            peak_window,
            ..
        } = r.participation
        else {
            panic!("measurable: {:?}", r.participation);
        };

        assert!(overall < 0.01, "the average looks harmless: {overall}");
        assert!(peak > 0.02, "the window does not: {peak}");
        assert_eq!(peak_window, 3, "and the report names which one");
        assert!(
            r.participation_invalidates(),
            "the flag must fire on the peak: {}",
            r.render()
        );
        assert!(r.render().contains("OVER THRESHOLD"));
    }

    /// A run that stayed small everywhere is not flagged, or the flag
    /// would fire on every run and be turned off within a week.
    #[test]
    fn a_run_that_stays_small_is_not_flagged() {
        let ticks: Vec<Tick> = (0..100).map(|i| tick(i, i * 10_000)).collect();
        let fills = vec![fill(50, 1, Liquidity::Maker)];
        let r = report(&result(fills, MarginUsage::NotTracked), &ticks, 10, 0.01);
        assert!(!r.participation_invalidates(), "{}", r.render());
    }

    /// This figure decides whether the whole run is valid, so a wrong
    /// one is worse than a missing one. Volume that goes backwards is
    /// not cumulative, and a share of it is not defined.
    #[test]
    fn volume_that_goes_backwards_makes_participation_unmeasurable() {
        let mut ticks: Vec<Tick> = (0..50).map(|i| tick(i, i * 100)).collect();
        ticks[30].volume = QtyLots(0); // a reset, or a feed defect
        let r = report(
            &result(vec![fill(10, 1, Liquidity::Taker)], MarginUsage::NotTracked),
            &ticks,
            10,
            0.01,
        );
        assert!(matches!(r.participation, Participation::Unmeasurable(_)));
        assert_eq!(r.participation.peak(), None);
        assert!(
            !r.participation_invalidates(),
            "unmeasurable is not the same as over threshold"
        );
        assert!(
            r.render().contains("NOT MEASURABLE") && r.render().contains("not a passing one"),
            "{}",
            r.render()
        );
    }

    /// An empty column under `latency` reads as a backtest where latency
    /// was zero — which is a claim, and it is the claim L0 makes. The
    /// difference between an assumption and an oversight is whether it
    /// is written down.
    #[test]
    fn the_things_the_tier_does_not_model_are_stated_rather_than_blank() {
        let ticks: Vec<Tick> = (0..10).map(|i| tick(i, i * 100)).collect();
        let r = report(
            &result(Vec::new(), MarginUsage::NotTracked),
            &ticks,
            5,
            0.01,
        );
        assert!(r.assumptions.latency.contains("none modelled"));
        assert!(r.assumptions.impact.contains("none modelled"));
        let text = r.render();
        assert!(text.contains("latency") && text.contains("impact"));
        assert!(
            !text.contains("latency         \n"),
            "no blank column: {text}"
        );
    }

    #[test]
    fn the_maker_taker_split_is_counted() {
        let ticks: Vec<Tick> = (0..20).map(|i| tick(i, i * 1_000)).collect();
        let fills = vec![
            fill(1, 1, Liquidity::Maker),
            fill(2, 1, Liquidity::Maker),
            fill(3, 1, Liquidity::Taker),
        ];
        let r = report(&result(fills, MarginUsage::NotTracked), &ticks, 5, 0.5);
        assert_eq!(r.taker_maker, (1, 2));
        let share = r.maker_share().expect("there were fills");
        assert!((share - 2.0 / 3.0).abs() < 1e-12);
    }

    /// No fills is not zero percent maker. A share of nothing is
    /// undefined, and printing 0% would read as a run that took
    /// liquidity every time.
    #[test]
    fn a_run_with_no_fills_has_no_maker_share() {
        let ticks: Vec<Tick> = (0..20).map(|i| tick(i, i * 1_000)).collect();
        let r = report(&result(Vec::new(), MarginUsage::NotTracked), &ticks, 5, 0.5);
        assert_eq!(r.maker_share(), None);
        assert!(r.render().contains("no fills"));
    }

    /// Three states, three sentences. "Nobody measured", "there was
    /// never a position" and "it came within nothing of the line" are
    /// different facts, and the report says which.
    #[test]
    fn the_three_margin_states_read_differently() {
        let ticks: Vec<Tick> = (0..20).map(|i| tick(i, i * 1_000)).collect();
        let render = |m: MarginUsage| report(&result(Vec::new(), m), &ticks, 5, 0.5).render();

        assert!(render(MarginUsage::NotTracked).contains("not tracked"));
        assert!(render(MarginUsage::NoPosition).contains("no position was ever open"));

        let close = render(MarginUsage::Tracked {
            peak_maintenance: Cash(500_000_000),
            min_headroom: Cash(0),
        });
        assert!(close.contains("closest approach 0.00"), "{close}");
        assert!(
            close.contains("reached or passed the line"),
            "standing exactly on it is worth saying: {close}"
        );

        let safe = render(MarginUsage::Tracked {
            peak_maintenance: Cash(500_000_000),
            min_headroom: Cash(900_000_000),
        });
        assert!(!safe.contains("reached or passed"), "{safe}");
    }

    /// A window of zero spans no market, and a series of one has no
    /// elapsed volume. Both are refusals rather than divisions by zero.
    #[test]
    fn degenerate_inputs_are_refused_rather_than_divided_by() {
        let ticks: Vec<Tick> = (0..10).map(|i| tick(i, i * 100)).collect();
        let r = result(Vec::new(), MarginUsage::NotTracked);
        assert!(matches!(
            report(&r, &ticks, 0, 0.01).participation,
            Participation::Unmeasurable(_)
        ));
        assert!(matches!(
            report(&r, &ticks[..1], 5, 0.01).participation,
            Participation::Unmeasurable(_)
        ));
        let flat: Vec<Tick> = (0..10).map(|i| tick(i, 0)).collect();
        assert!(matches!(
            report(&r, &flat, 5, 0.01).participation,
            Participation::Unmeasurable(_)
        ));
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;
    use oq_engine::{Delay, Impact, Latency, Policy, QueueAhead};
    use oq_types::Nanos;

    fn nothing() -> RunResult {
        RunResult {
            strategy: "t".into(),
            fills: Vec::new(),
            liquidations: Vec::new(),
            ticks: 0,
            final_equity: Cash(0),
            realized: Cash(0),
            funding_paid: Cash(0),
            fees_paid: Cash(0),
            min_equity: Cash(0),
            equity_curve: Vec::new(),
            max_adverse_ticks: 0,
            margin_usage: MarginUsage::NotTracked,
            tier: "L0",
            depth_applied: 0,
            depth_refused: 0,
            depth_unused: 0,
        }
    }

    fn rendered(assumptions: Assumptions) -> String {
        report_at(&nothing(), &[], 10, 0.01, assumptions).render()
    }

    /// The confusion `oq-engine`'s ladder warns about, made impossible
    /// to produce by accident: an L1 run must not report L0's
    /// assumptions, because a P&L measured at one tier is a different
    /// quantity from the same P&L measured at another.
    #[test]
    fn an_l1_run_does_not_report_l0s_assumptions() {
        let policy = Policy {
            queue: QueueAhead::Fixed(oq_types::QtyLots(250)),
            latency: Latency {
                entry: Delay::Fixed(Nanos(3_000_000)),
                response: Delay::Fixed(Nanos(9_000_000)),
            },
            impact: Impact { coefficient: 175 },
        };
        let text = rendered(Assumptions::of_l1(&policy));

        assert!(text.contains("tier L1"), "{text}");
        assert!(
            !text.contains("none modelled"),
            "L0's sentences must not appear: {text}"
        );
        assert!(text.contains("250 lots"), "{text}");
        assert!(
            text.contains("3000000") && text.contains("9000000"),
            "{text}"
        );
        assert!(text.contains("1.75"), "{text}");
    }

    /// A transparent L1 policy is L0's answer, and calling it L1 would
    /// put a higher tier's name on a lower tier's fidelity.
    #[test]
    fn a_transparent_l1_policy_is_reported_as_equivalent_to_l0() {
        let text = rendered(Assumptions::of_l1(&Policy::TRANSPARENT));
        assert!(text.contains("equivalent to L0"), "{text}");
        assert!(
            text.contains("none modelled"),
            "and it carries L0's sentences: {text}"
        );
    }

    /// L1's own words, not a second description that can drift from
    /// them. Two sources for one sentence is how a report ends up
    /// describing an engine that is no longer there.
    #[test]
    fn the_queue_line_comes_from_the_policy_itself() {
        let policy = Policy {
            queue: QueueAhead::VolumeMultiple(275),
            ..Policy::TRANSPARENT
        };
        let a = Assumptions::of_l1(&policy);
        assert_eq!(a.queue.as_deref(), Some(policy.describe().as_str()));
    }

    /// An L0 run has no queue line at all, rather than one saying there
    /// is no queue — the tier does not have the concept, and a heading
    /// that appears with "none" under it implies the tier considered it.
    #[test]
    fn an_l0_run_has_no_queue_heading() {
        let text = rendered(Assumptions::l0());
        assert!(text.contains("tier L0"), "{text}");
        assert!(!text.contains("queue  "), "{text}");
    }
}
