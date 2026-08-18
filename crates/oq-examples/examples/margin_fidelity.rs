//! The margin fidelity study: how wrong is a backtest with no margin
//! model, and where is the wrongness?
//!
//! ```text
//! cargo run --release --example margin_fidelity
//! ```
//!
//! `martingale_ladder` shows the failure in one window. One window is
//! not a study. This runs the same strategy over many windows — most
//! calm, a few with a crash of varying depth — under both margin modes,
//! and reports the *distribution* of the difference.
//!
//! The methodology, and why it is a cross-window tail rather than a mean
//! or a within-run series, is written up in `docs/MARGIN-FIDELITY.md`.

use oq_backtest::{MarginMode, RunConfig, Window, run, stress};
use oq_examples::{MarketShape, MartingaleLadder, crash_series, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId};

/// Every window the study runs over: mostly calm, a handful of crashes.
///
/// The mix is the honest weak point of any such study, and it is not
/// derived from anything: twenty-eight calm to twelve stressed is a
/// number chosen to put observations in the tail, not an estimate of how
/// often real markets crash. It is stated here, and printed by the run,
/// because two of the statistics below move with it — a study that
/// quotes a mean gap without its mix is quoting a number it made up.
/// The conditional statistic is the one that survives the choice.
fn windows() -> Vec<(String, Vec<oq_engine::Tick>)> {
    let mut out = Vec::new();
    for i in 0..28u64 {
        out.push((
            format!("calm-{i:02}"),
            series(MarketShape::calm(600), 100 + i),
        ));
    }
    for i in 0..12u64 {
        // Depths from a routine 22% pullback to a 68% collapse.
        let depth = 0.22 + f64::from(u32::try_from(i).unwrap_or(0)) * 0.042;
        out.push((
            format!("crash-{:02}-{:.0}%", i, depth * 100.0),
            crash_series(11 + i, 400, 200, depth),
        ));
    }
    out
}

fn main() {
    let starting = Cash::from_units(2_000);
    let base = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        starting,
    );
    let enforced_cfg = base.clone().with_margin(MarginMode::Enforced);
    let ignored_cfg = base.with_margin(MarginMode::Ignored);

    let mut studied = Vec::new();
    for (label, ticks) in windows() {
        let e = run(&enforced_cfg, &mut MartingaleLadder::new(), &ticks);
        let g = run(&ignored_cfg, &mut MartingaleLadder::new(), &ticks);
        match Window::of(&label, starting, &e, &g) {
            Ok(w) => studied.push(w),
            Err(why) => println!("{label}: skipped, {why}"),
        }
    }

    let report = match stress(&studied, &[0.05, 0.10, 0.25, 0.50, 0.75]) {
        Ok(r) => r,
        Err(why) => {
            println!("no study: {why}");
            return;
        }
    };

    println!("margin fidelity study");
    println!("  strategy        martingale-ladder — the same one martingale_ladder runs");
    println!(
        "  windows         {} ({} calm, {} stressed by construction)",
        report.windows,
        report.windows - 12,
        12
    );
    println!(
        "  liquidations    {} windows in which the venue closed the account",
        report.liquidated
    );
    println!(
        "  overstated in   {} of {} windows",
        report.overstated_windows(),
        report.windows
    );
    println!();

    println!("  per-window return, by quantile");
    println!("    quantile      enforced    margin-free       gap");
    for p in &report.tail {
        println!(
            "    {:>7.0}%   {:>10.2}%   {:>12.2}%   {:>7.2}%",
            p.q * 100.0,
            p.enforced * 100.0,
            p.ignored * 100.0,
            p.overstatement() * 100.0
        );
    }
    println!();

    if let Some((real, claimed)) = report.given_liquidation {
        println!(
            "  in the {} windows that closed the account:",
            report.liquidated
        );
        println!("    the account got           {:>10.2}%", real * 100.0);
        println!("    the margin-free run said  {:>10.2}%", claimed * 100.0);
        println!("    <- this one does not move when the window mix does");
        println!();
    }

    println!("  mix-dependent, quoted only with the mix above:");
    println!(
        "  mean gap        {:>8.2}%   <- what a naive comparison reports",
        report.mean_overstatement * 100.0
    );
    match report.worst_decile_share {
        Some(share) => println!(
            "  worst decile    {:>8.1}%   <- share of the total gap it carries",
            share * 100.0
        ),
        None => println!("  worst decile         n/a   <- no overstatement to apportion"),
    }
    println!();

    // Naming the windows matters: a reader who wants to disbelieve the
    // report should be able to go and look at the ones driving it.
    let mut worst: Vec<_> = report.per_window.iter().collect();
    worst.sort_by(|a, b| b.overstatement().total_cmp(&a.overstatement()));
    println!("  the windows carrying it");
    for w in worst.iter().take(4) {
        println!(
            "    {:<16} enforced {:>8.2}%   margin-free {:>8.2}%{}",
            w.label,
            w.enforced * 100.0,
            w.ignored * 100.0,
            if w.liquidated { "   LIQUIDATED" } else { "" }
        );
    }
}
