//! Six classic strategies, and what this framework says about them.
//!
//! ```text
//! cargo run --release -p oq-examples --example classics
//! ```
//!
//! **None of these is a recommendation.** Every one is decades old,
//! published, and traded by enough people that whatever edge it had is
//! not waiting in a public repository. They are here because they are
//! the strategies a reader has already heard of, so the framework can be
//! learned by recognising something rather than by learning two things
//! at once.
//!
//! # Why this example does not print an equity curve and stop
//!
//! That is what a backtester prints, and `WHY.md` argues the expensive
//! failure in this field is being wrong while looking right. So each
//! strategy is run through the instruments this project exists to
//! provide, and what those say is the output:
//!
//! - **the margin arms** — the same strategy with liquidation modelled
//!   and without, because a curve that never gets liquidated is a curve
//!   about an account no venue offers
//! - **the fidelity report** — participation, maker/taker, and what the
//!   tier assumes about what it does not model
//! - **the closest approach to liquidation** — how near the account came
//!   to ending, which a final equity figure cannot show
//!
//! The lesson is not "here is a strategy". It is "here is what this
//! framework tells you about a strategy you already believed in".

use oq_backtest::validity::DEFAULT_THRESHOLD;
use oq_backtest::{MarginMode, RunConfig, RunResult, Strategy, fidelity_report, run};
use oq_engine::Tick;
use oq_examples::classics::{
    BollingerReversion, DonchianBreakout, DualThrust, GridTrader, MacdTrend, RsiReversion,
    catalogue,
};
use oq_examples::{MarketShape, crash_series, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId};

/// Run one strategy under both margin modes over the same observations.
fn both_arms<S: Strategy, F: Fn() -> S>(
    build: F,
    ticks: &[Tick],
    balance: i64,
) -> (RunResult, RunResult) {
    let base = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
    // The fidelity report's margin line needs it, and it costs
    // throughput, so a run has to ask.
    .tracking_margin();

    let enforced = run(
        &base.clone().with_margin(MarginMode::Enforced),
        &mut build(),
        ticks,
    );
    let ignored = run(&base.with_margin(MarginMode::Ignored), &mut build(), ticks);
    (enforced, ignored)
}

fn money(c: Cash) -> String {
    format!("{:.2}", c.0 as f64 / 100_000_000.0)
}

/// One row of the summary.
fn row(name: &str, enforced: &RunResult, ignored: &RunResult) {
    let liq = if enforced.liquidations.is_empty() {
        String::new()
    } else {
        format!("  LIQUIDATED {}x", enforced.liquidations.len())
    };
    println!(
        "  {name:<20} {:>5} {:>14} {:>16}{liq}",
        enforced.fills.len(),
        money(enforced.final_equity),
        money(ignored.final_equity),
    );
}

fn main() {
    // A market with a calm stretch, a fall, and a partial recovery. The
    // shape that separates a strategy which survives from one whose
    // backtest merely says it did.
    let ticks = crash_series(11, 3_000, 900, 0.45);
    let calm = series(MarketShape::calm(4_000), 5);

    println!("classic strategies, as teaching references");
    println!("==========================================");
    println!();
    for c in catalogue() {
        println!("  {:<20} {}", c.name, c.origin);
        println!("  {:<20} bets on: {}", "", c.premise);
        println!("  {:<20} breaks when: {}", "", c.weakness);
        println!();
    }

    // Two balances, and the second is the point.
    //
    // At 10,000 units against one-lot positions the margin overlay never
    // bites, so both arms report the same number and the comparison this
    // example exists to make is invisible. That is not a flaw in the
    // fixture — it is the finding: **a margin model is invisible until
    // leverage is real**, and the strategies that end accounts are the
    // ones run with leverage. So the same six run twice, and the second
    // pass is where the two columns separate.
    for (balance, leverage) in [(10_000i64, "unlevered"), (60, "levered")] {
        for (label, series) in [
            ("a market that falls 45%", &ticks),
            ("a calm market", &calm),
        ] {
            println!();
            println!(
                "over {label}, {leverage} ({} observations, {balance} units of capital)",
                series.len()
            );
            println!(
                "  {:<20} {:>5} {:>14} {:>16}",
                "strategy", "fills", "with margin", "margin-free"
            );

            let (a, b) = both_arms(RsiReversion::new, series, balance);
            row("rsi-reversion", &a, &b);
            let (a, b) = both_arms(MacdTrend::new, series, balance);
            row("macd-trend", &a, &b);
            let (a, b) = both_arms(BollingerReversion::new, series, balance);
            row("bollinger-reversion", &a, &b);
            let (a, b) = both_arms(DonchianBreakout::new, series, balance);
            row("donchian-breakout", &a, &b);
            let (a, b) = both_arms(GridTrader::new, series, balance);
            row("grid", &a, &b);
            let (a, b) = both_arms(DualThrust::new, series, balance);
            row("dual-thrust", &a, &b);
        }
    }

    // One strategy in full, because a table of numbers is the thing this
    // example exists to argue against. The grid is chosen deliberately:
    // it is the one people actually run, and it is short volatility with
    // no stop.
    println!();
    println!("the grid, in full — the one most often run and least often measured");
    println!("====================================================================");
    let (enforced, ignored) = both_arms(GridTrader::new, &ticks, 60);
    println!();
    println!("  with margin modelled   {}", money(enforced.final_equity));
    println!("  margin-free            {}", money(ignored.final_equity));
    println!("  lowest equity, real    {}", money(enforced.min_equity));
    println!("  lowest equity, model   {}", money(ignored.min_equity));
    if !enforced.liquidations.is_empty() {
        println!(
            "  the venue closed this account {} time(s); the margin-free run kept trading",
            enforced.liquidations.len()
        );
    }
    println!();
    print!(
        "{}",
        fidelity_report(&enforced, &ticks, 60, DEFAULT_THRESHOLD).render()
    );

    println!();
    println!("Read the two equity columns against each other rather than either alone.");
    println!("Where they differ, the difference is not a modelling nicety — it is the");
    println!("part of the result that belongs to an account the venue would have closed.");
    println!();
    println!("And read the fill counts. A strategy that trades constantly on a calm");
    println!("market is paying the spread for the privilege, which no equity curve");
    println!("labels as a decision anybody made.");
}
