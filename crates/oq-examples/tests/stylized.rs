//! What the generated markets are, measured rather than assumed.
//!
//! Every example in this workspace, every golden that pins a quoted
//! number, and the whole margin-fidelity argument runs on a series from
//! `series` or `crash_series`. Those are **fixtures**, not simulations,
//! and this file is where that distinction stops being a word in a
//! comment and becomes four numbers per market.
//!
//! `oq_stats::StylizedFacts` measures the properties that hold across
//! essentially every liquid market. The generated ones do not have most
//! of them, which is not a defect — a fixture built to fall 45% on cue
//! is supposed to be predictable, and that is the whole reason it can
//! pin a number. What would be a defect is quoting a result from one as
//! though the market had been real.
//!
//! So these tests do two things and neither is a quality gate:
//!
//! - **Pin the measured values**, so changing a generator is noticed by
//!   whoever changes it rather than by a reader much later.
//! - **State the shape in assertions**, so the claims below are checked
//!   rather than believed.

use oq_examples::{MarketShape, crash_series, series};
use oq_stats::{StylizedFacts, Verdict};

/// Log returns of the traded price.
fn returns(ticks: &[oq_engine::Tick]) -> Vec<f64> {
    ticks
        .windows(2)
        .filter(|w| w[0].last.0 > 0 && w[1].last.0 > 0)
        .map(|w| {
            #[allow(clippy::cast_precision_loss)]
            let (a, b) = (w[0].last.0 as f64, w[1].last.0 as f64);
            (b / a).ln()
        })
        .collect()
}

fn measure(ticks: &[oq_engine::Tick]) -> StylizedFacts {
    StylizedFacts::measure(&returns(ticks)).expect("the fixtures are long enough")
}

fn near(v: Verdict, want: f64) -> bool {
    v.value().is_some_and(|x| (x - want).abs() < 0.01)
}

/// **None of the generated markets has heavy tails.**
///
/// Excess kurtosis of 0.03, 0.07 and −0.05 against a real perpetual's
/// tens. This is the fact that matters most here, because heavy tails
/// are the reason a margin model is not optional: liquidation is a tail
/// event, and a series without a tail cannot produce one except where
/// the fixture was told to.
///
/// It also fixes the direction of the error. The examples understate
/// how bad a real market gets, so the margin-free-versus-enforced gap
/// they report is a **floor** on the real one, not an estimate of it.
#[test]
fn no_generated_market_is_heavy_tailed() {
    for (name, ticks) in [
        ("calm", series(MarketShape::calm(4_000), 5)),
        ("trending", series(MarketShape::trending(4_000), 42)),
        ("crash", crash_series(11, 3_000, 900, 0.45)),
    ] {
        let f = measure(&ticks);
        assert!(
            !f.heavy_tails.holds(),
            "{name} became heavy-tailed: {:?}. If a generator was changed \
             deliberately, the claim in QUICKSTART that these fixtures \
             understate real tails needs changing with it.",
            f.heavy_tails
        );
    }
}

/// **`crash_series` returns are strongly autocorrelated**, at ρ(1) ≈
/// 0.54, and no real market is.
///
/// The crash is a sustained monotone move, so consecutive returns share
/// a sign for hundreds of observations. A one-lag rule would predict it.
/// That is exactly what makes it a usable fixture — the liquidation
/// happens where it was put — and exactly why a strategy result from it
/// is a statement about this series and not about trading.
#[test]
fn the_crash_fixture_is_a_designed_trend_not_a_market() {
    let f = measure(&crash_series(11, 3_000, 900, 0.45));
    assert!(
        !f.uncorrelated_returns.holds(),
        "the crash fixture stopped being autocorrelated: {:?}",
        f.uncorrelated_returns
    );
    assert!(
        near(f.uncorrelated_returns, 0.536),
        "measured ρ(1) moved: {:?}",
        f.uncorrelated_returns
    );
}

/// The two `series` markets are uncorrelated, which is the one fact
/// they do have.
///
/// Worth asserting rather than assuming: a random walk is supposed to
/// produce this, and a generator that had drifted into producing
/// momentum would still look like a plausible price chart.
#[test]
fn the_random_walk_fixtures_are_uncorrelated() {
    for (name, ticks) in [
        ("calm", series(MarketShape::calm(4_000), 5)),
        ("trending", series(MarketShape::trending(4_000), 42)),
    ] {
        let f = measure(&ticks);
        assert!(
            f.uncorrelated_returns.holds(),
            "{name} developed autocorrelation: {:?}",
            f.uncorrelated_returns
        );
    }
}

/// Only the crash clusters, and only because it has two regimes.
///
/// Real volatility clustering is continuous and slowly decaying. This
/// is a step function: 3,000 calm observations then 900 violent ones.
/// It registers on the measurement, and it is not the same phenomenon —
/// which is the sort of thing a single number hides and a named test
/// can say.
#[test]
fn only_the_crash_clusters_and_it_is_a_step_not_a_process() {
    let calm = measure(&series(MarketShape::calm(4_000), 5));
    let crash = measure(&crash_series(11, 3_000, 900, 0.45));
    assert!(!calm.volatility_clustering.holds());
    assert!(crash.volatility_clustering.holds());
    assert!(
        near(crash.volatility_clustering, 0.301),
        "measured clustering moved: {:?}",
        crash.volatility_clustering
    );
}

/// Two of the four hold on the crash fixture, one on the calm one.
///
/// The count is pinned because it is the honest summary: these are not
/// markets, and any drift toward or away from that is a change somebody
/// made and should have to acknowledge.
#[test]
fn the_tally_is_what_it_is() {
    assert_eq!(measure(&series(MarketShape::calm(4_000), 5)).held(), 2);
    assert_eq!(measure(&series(MarketShape::trending(4_000), 42)).held(), 1);
    assert_eq!(measure(&crash_series(11, 3_000, 900, 0.45)).held(), 2);
}
