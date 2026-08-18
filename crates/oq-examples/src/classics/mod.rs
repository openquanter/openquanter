//! Classic strategies, as teaching references.
//!
//! # Read this before reading the strategies
//!
//! **None of these is a recommendation, and none of them is here because
//! it makes money.** They are here because they are the strategies a
//! reader has already heard of, so a framework can be learned by
//! recognising something rather than by learning two things at once.
//!
//! Every one of them is decades old, published, and traded by enough
//! people that whatever edge it had is not waiting in a public
//! repository. If one of them shows a profit on the generated market in
//! `examples/classics`, that is a fact about the generator and not about
//! the strategy.
//!
//! # Why a catalogue of them belongs in *this* project specifically
//!
//! `WHY.md` argues that the expensive failure in this field is being
//! wrong while looking right: a backtest that flatters, a gap nobody can
//! attribute, an overfit that has no price tag. A catalogue of famous
//! strategies is the most direct way to demonstrate that, because these
//! are exactly the strategies whose backtests look good and whose live
//! results do not.
//!
//! So `examples/classics` does not print an equity curve and stop. It
//! runs each one through the instruments this project exists to provide
//! — the fidelity report, the margin arms, the overfitting statistics —
//! and prints what those say. The lesson is not "here is a strategy",
//! it is "here is what this framework tells you about a strategy you
//! already believed in".
//!
//! # What each one assumes, stated
//!
//! Every strategy here is written against a single instrument, uses only
//! the indicators in `oq-strategy`, and takes no parameters that were
//! tuned on the data it runs against. That last is deliberate: the
//! parameters are each strategy's *published* ones — 14 for RSI, 12/26/9
//! for MACD, 20/2 for Bollinger, 20/55 for Donchian — so that nothing
//! here is a fit, not even accidentally.

mod bollinger;
mod donchian;
mod dual_thrust;
mod grid;
pub(crate) mod helpers;
mod macd;
mod rsi;

pub use bollinger::BollingerReversion;
pub use donchian::DonchianBreakout;
pub use dual_thrust::DualThrust;
pub use grid::GridTrader;
pub use macd::MacdTrend;
pub use rsi::RsiReversion;

/// One entry in the catalogue.
pub struct Classic {
    /// What it is called.
    pub name: &'static str,
    /// Where it comes from, in one line.
    pub origin: &'static str,
    /// What it bets on.
    pub premise: &'static str,
    /// The assumption that most often turns out to be false live.
    ///
    /// Carried per strategy because it differs, and because a catalogue
    /// that listed only the rules would teach the rules — which is the
    /// half everybody already has.
    pub weakness: &'static str,
}

/// The catalogue, for a report that names what it ran.
#[must_use]
pub fn catalogue() -> Vec<Classic> {
    vec![
        Classic {
            name: "rsi-reversion",
            origin: "Wilder, 1978 — the oscillator every platform ships",
            premise: "an extreme reading reverts",
            weakness: "an oscillator says 'oversold' all the way down a trend; \
                       the reading that triggers the entry is the same reading a \
                       collapse produces",
        },
        Classic {
            name: "macd-trend",
            origin: "Appel, late 1970s",
            premise: "a fast average crossing a slow one marks a turn",
            weakness: "it is two moving averages, so it is late by construction — \
                       and in a sideways market the lateness costs a round trip per \
                       oscillation",
        },
        Classic {
            name: "bollinger-reversion",
            origin: "Bollinger, early 1980s",
            premise: "price outside two standard deviations comes back",
            weakness: "the bands widen *after* volatility arrives, so the entry that \
                       matters most is the one taken on a band that had not adjusted yet",
        },
        Classic {
            name: "donchian-breakout",
            origin: "Donchian; the rule the Turtles were taught in 1983",
            premise: "a new N-period extreme continues",
            weakness: "most breakouts fail, and the system depends entirely on the \
                       few that do not — which makes its result a property of a \
                       handful of trades and almost nothing else",
        },
        Classic {
            name: "grid",
            origin: "no single author; the default retail strategy in crypto",
            premise: "price oscillates within a range, and each rung captures a slice",
            weakness: "it is short volatility with no stop: every rung is profitable \
                       until the range breaks, and then the accumulated position is \
                       on the wrong side of a trend. This is the one to run with the \
                       margin overlay on",
        },
        Classic {
            name: "dual-thrust",
            origin: "Michael Chalek, 1980s; widely used in Chinese futures",
            premise: "a move beyond a fraction of the recent range starts a day's trend",
            weakness: "the range it measures is yesterday's, so a regime change is \
                       priced a day late",
        },
    ]
}
