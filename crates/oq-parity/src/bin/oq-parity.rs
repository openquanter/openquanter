//! `oq-parity` — compare a run against a baseline.
//!
//! ```text
//! oq-parity baseline.run candidate.run
//! ```
//!
//! Two runs, written by `oq_parity::wire`, diffed fill by fill. This is
//! the tool the roadmap listed as a planned subcommand and then recorded
//! as **not coming**, on the grounds that `compare` needs a manifest and
//! an output and neither had a serialised form. That was the honest
//! answer at the time; the file format is the thing that was actually
//! missing, and now that it exists the command follows.
//!
//! # What the exit code means
//!
//! `0` the runs agree, `1` they differ, `2` the arguments were wrong,
//! `3` the baseline is invalidated — its data or configuration moved, so
//! nothing about the engine can be concluded until it is rebased.
//!
//! Three outcomes and not two, for the same reason `oq-recon` has three:
//! *"I could not check"* is not *"I checked and it is fine"*. A CI job
//! that treats an invalidated baseline as a pass is a CI job whose
//! regression guard silently stopped guarding.

use std::process::ExitCode;

use oq_parity::diff::compare;
use oq_parity::manifest::BaselineStatus;
use oq_parity::wire::Run;

const USAGE: &str = "\
oq-parity — compare a run against a baseline

USAGE:
    oq-parity <BASELINE.run> <CANDIDATE.run> [--pnl-tolerance FRACTION]

Both files are written by oq_parity::wire. Each carries its own manifest,
so a baseline cannot be separated from the experiment it describes.

EXIT CODES:
    0  the runs agree
    1  they differ
    2  the arguments were wrong
    3  the baseline is invalidated: its data or configuration moved, and
       nothing about the engine can be concluded until it is rebased.
       Not 0 — an invalidated baseline is not a passing one.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() != 2 {
        eprintln!("oq-parity: needs exactly two files, a baseline and a candidate");
        return ExitCode::from(2);
    }
    let tolerance = match args.iter().position(|a| a == "--pnl-tolerance") {
        None => 0.0,
        Some(i) => match args.get(i + 1).and_then(|v| v.parse::<f64>().ok()) {
            Some(v) if (0.0..=1.0).contains(&v) => v,
            _ => {
                eprintln!("oq-parity: --pnl-tolerance needs a fraction between 0 and 1");
                return ExitCode::from(2);
            }
        },
    };

    let load = |path: &str| -> Result<Run, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Run::parse(&text).map_err(|e| format!("{path}: {e}"))
    };
    let baseline = match load(positional[0]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("oq-parity: {e}");
            // 3, not 1. A baseline that cannot be read has not been
            // compared, and reporting a difference would invent one.
            return ExitCode::from(3);
        }
    };
    let candidate = match load(positional[1]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("oq-parity: {e}");
            return ExitCode::from(3);
        }
    };

    let report = compare(
        &baseline.manifest,
        &baseline.output,
        &candidate.manifest,
        &candidate.output,
    );

    println!("baseline    {}", positional[0]);
    println!("candidate   {}", positional[1]);
    println!(
        "identity    code {} → {}",
        short(&baseline.manifest.code_commit),
        short(&candidate.manifest.code_commit)
    );

    match &report.baseline_status {
        BaselineStatus::Comparable => println!("            same code, data and configuration"),
        BaselineStatus::CodeChanged => println!(
            "            the code moved; data and configuration did not, so differences\n\
             \x20           below are attributable to the code change"
        ),
        BaselineStatus::Invalidated { changed } => {
            println!();
            println!("baseline invalidated — rebase required");
            for element in changed {
                println!("  {element}: {}", element.explanation());
            }
            println!();
            println!(
                "Nothing was compared. A difference reported now would be about a\n\
                 different experiment, and reporting none would be worse."
            );
            return ExitCode::from(3);
        }
    }

    println!(
        "fills       {} → {}",
        report.fill_counts.0, report.fill_counts.1
    );
    println!("pnl         {} → {}", report.pnl.0, report.pnl.1);
    if let Some(e) = report.pnl_relative_error {
        println!("            relative error {:.6}", e);
    }
    println!(
        "matched     {} fill(s) before the first difference",
        report.matched_prefix
    );

    if report.differences.is_empty() {
        println!();
        println!("the runs agree");
    } else {
        println!();
        println!("{} difference(s):", report.differences.len());
        // Bounded, and it says so. A port that changed everything
        // produces thousands, and a terminal full of them is a terminal
        // nobody reads — but a silent cap would read as "only twenty".
        for d in report.differences.iter().take(20) {
            println!("  - {}", d.describe());
        }
        if report.differences.len() > 20 {
            println!("  … and {} more", report.differences.len() - 20);
        }
    }

    if report.passes(tolerance) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The first twelve characters of a hash, which is what a person reads.
fn short(hash: &str) -> &str {
    &hash[..12.min(hash.len())]
}
