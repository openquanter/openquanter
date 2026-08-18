//! The example this framework exists for: the same strategy, the same
//! market, run once with liquidation enforced and once without.
//!
//! ```text
//! cargo run --example martingale_ladder
//! ```
//!
//! Averaging down on a fixed ladder is the classic shape whose backtest
//! is systematically fake without a margin model. It wins in almost
//! every window, because almost every drawdown eventually retraces —
//! and the one that does not is the one that ends the account. A
//! simulation that never liquidates simply carries the position through
//! the hole and books the recovery.
//!
//! Watch the margin-free arm's *lowest equity*. If it went below zero,
//! the account it describes did not exist for the rest of the run, and
//! every fill after that point was placed by a corpse.
//!
//! **This is not a strategy to run.** It is here because it is the
//! clearest demonstration of what the margin overlay is for.

use oq_backtest::{DeviationReport, MarginMode, RunConfig, tail_divergence};
use oq_examples::{crash_series, money, price};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, PriceTicks};

use oq_examples::MartingaleLadder;

fn main() {
    // Calm, then a 50% fall, then a partial recovery. The recovery is
    // what makes the margin-free arm look survivable.
    let ticks = crash_series(11, 400, 200, 0.5);

    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(2_000),
    )
    .with_margin(MarginMode::Enforced)
    // The tail report needs a return series, and a run only produces one
    // when it samples equity. Sample every tick: the enforced arm's
    // series ends when the account does, so how fine a quantile this
    // data can support is bounded not by the length of the run but by
    // how long the account survived — a coarser interval here gets the
    // 1st percentile refused for want of observations.
    .sampling_equity_every(1);

    let report = DeviationReport::compare(&config, MartingaleLadder::new, &ticks);

    println!("market        calm, then -50%, then a partial recovery");
    println!(
        "              start {}  low {}  end {}",
        price(ticks[0].last),
        price(PriceTicks(
            ticks.iter().map(|t| t.last.0).min().unwrap_or_default()
        )),
        price(ticks.last().map(|t| t.last).unwrap_or_default())
    );
    println!();

    println!("                        enforced      margin-free");
    println!(
        "final equity      {} {}",
        money(report.enforced.final_equity),
        money(report.ignored.final_equity)
    );
    println!(
        "lowest equity     {} {}",
        money(report.enforced.min_equity),
        money(report.ignored.min_equity)
    );
    println!(
        "fills             {:>12} {:>16}",
        report.enforced.fills.len(),
        report.ignored.fills.len()
    );
    println!(
        "liquidations      {:>12} {:>16}",
        report.enforced.liquidations.len(),
        report.ignored.liquidations.len()
    );
    println!();

    println!("{}", report.summary_line());

    // The numbers above are two endpoints. The tail report is the
    // distribution: where in the return series the two arms actually
    // part company, and by how much. See docs/MARGIN-FIDELITY.md.
    match tail_divergence(&report.enforced, &report.ignored, &[0.01, 0.05, 0.10, 0.25]) {
        Ok(f) => {
            println!();
            println!(
                "tail divergence   paired over {} return samples",
                f.paired_until
            );
            match f.diverged_at {
                Some(i) => println!(
                    "                  arms part at sample {i} of {}",
                    f.paired_until
                ),
                None => println!("                  the arms never parted on this data"),
            }
            println!();
            println!("  quantile        enforced      margin-free     overstated by");
            for p in &f.tail {
                println!(
                    "  {:>7.0}%    {:>12.4}%   {:>12.4}%    {:>12.4}%",
                    p.q * 100.0,
                    p.enforced * 100.0,
                    p.ignored * 100.0,
                    p.overstatement() * 100.0
                );
            }
            if let Some(worst) = f.worst_overstatement() {
                println!();
                println!(
                    "  Worst at the {:.0}th percentile: the margin-free run reports a return\n  \
                     {:.4} percentage points better than the account could have had.",
                    worst.q * 100.0,
                    worst.overstatement() * 100.0
                );
            }
        }
        // Not a failure of the strategy — a statement that this data
        // cannot support the statistic, which is worth printing rather
        // than silently skipping.
        Err(why) => println!("\ntail divergence   unavailable: {why}"),
    }

    if !report.margin_free_result_is_honest() {
        println!();
        println!(
            "The margin-free account's equity went below zero. That account did not\n\
             exist for the rest of the run, and the {} fill(s) it made after the\n\
             first liquidation were placed by an account the venue had already closed.",
            report.fills_after_first_liquidation()
        );
    }
}
