//! `oq-data` — characterise a tick file before trusting it.
//!
//! ```text
//! oq-data ticks.oqtk
//! ```
//!
//! Every number a backtest produces is downstream of the file it read, and
//! a file can be wrong in ways that produce plausible results: a window
//! that stops early, a run of ticks with no quote, timestamps that do not
//! advance. Reading the header tells you what the file claims; this tells
//! you what it contains.
//!
//! The two are checked against each other, because a header that disagrees
//! with its own records is the one case where believing either is worse
//! than believing neither.

use std::process::ExitCode;

const USAGE: &str = "\
oq-data — characterise a tick file before trusting it

USAGE:
    oq-data <FILE.oqtk>
    oq-data <FILE.oqtk> --parquet <OUT.parquet>

The export keeps both timestamps as separate columns and every price as
an integer in its native tick unit. It is only available when this build
was made with `--features parquet`, because the columnar stack is about
ninety crates and a backtest must not pay for it.
";

/// Log returns of the traded price, over ticks that carry one.
///
/// Ticks without a trade are skipped rather than carried forward. A
/// forward-filled zero is a return the market did not produce, and
/// enough of them pull kurtosis toward the normal -- which would flatter
/// exactly the fact this is measured to check.
fn log_returns(ticks: &[oq_engine::Tick]) -> Vec<f64> {
    let traded: Vec<f64> = ticks
        .iter()
        .filter(|t| t.last.0 > 0)
        .map(|t| {
            #[allow(clippy::cast_precision_loss)]
            let p = t.last.0 as f64;
            p
        })
        .collect();
    traded.windows(2).map(|w| (w[1] / w[0]).ln()).collect()
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        print!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if path == "--help" || path == "-h" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let args: Vec<String> = std::env::args().collect();
    let export = args
        .iter()
        .position(|a| a == "--parquet")
        .and_then(|i| args.get(i + 1).cloned());
    if args.iter().any(|a| a == "--parquet") && export.is_none() {
        eprintln!("oq-data: --parquet needs an output path");
        return ExitCode::FAILURE;
    }

    let (header, ticks) = match oq_data::ticks::read_file(std::path::Path::new(&path)) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("oq-data: {path}: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    println!("file             {path}");
    println!("instrument       {:#018x}", header.instrument);
    println!("records          {}", ticks.len());
    if header.count as usize != ticks.len() {
        // The header is metadata and the records are the data. A
        // disagreement means one of them is lying and nothing here can
        // say which, so it is reported rather than resolved.
        println!(
            "                 header claims {}, which disagrees",
            header.count
        );
    }
    if ticks.is_empty() {
        println!("verdict          empty");
        return ExitCode::SUCCESS;
    }

    let first = ticks[0].stamp.exch.0;
    let last = ticks[ticks.len() - 1].stamp.exch.0;
    println!("span             {first} .. {last} ns");
    println!(
        "                 {:.2} hours",
        (last - first) as f64 / 3.6e12
    );

    // What a strategy will actually see.
    let quoted = ticks.iter().filter(|t| t.bid.0 > 0 && t.ask.0 > 0).count();
    let traded = ticks.iter().filter(|t| t.last.0 > 0).count();
    println!(
        "with a quote     {quoted} ({:.1}%)",
        100.0 * quoted as f64 / ticks.len() as f64
    );
    println!(
        "with a trade     {traded} ({:.1}%)",
        100.0 * traded as f64 / ticks.len() as f64
    );

    // Ordering, because everything downstream assumes it and nothing
    // downstream checks it.
    let backwards = ticks
        .windows(2)
        .filter(|w| w[1].stamp.exch.0 < w[0].stamp.exch.0)
        .count();
    let repeated = ticks
        .windows(2)
        .filter(|w| w[1].stamp.exch.0 == w[0].stamp.exch.0)
        .count();
    println!("out of order     {backwards}");
    println!("same timestamp   {repeated}");

    // Volume is cumulative by convention, so a decrease means the venue
    // reset its counter or two sources were mixed — either way a consumer
    // differencing consecutive ticks gets a negative quantity.
    let volume_drops = ticks
        .windows(2)
        .filter(|w| w[1].volume.0 < w[0].volume.0)
        .count();
    println!("volume went down {volume_drops}");

    let crossed = ticks
        .iter()
        .filter(|t| t.bid.0 > 0 && t.ask.0 > 0 && t.bid.0 >= t.ask.0)
        .count();
    println!("crossed book     {crossed}");

    // The reason both timestamps are in the format at all. A dataset
    // whose arrival times equal its exchange times carries no latency
    // information — captured without them, or synthesized — and that is
    // worth knowing *before* somebody calibrates a latency model
    // against it, which is what M4 is waiting on.
    // Rebuilt rather than carried: the stream was decoded above into a
    // plain vector, and re-wrapping is cheaper than threading it through
    // every check between here and there. A construction failure here
    // means the ticks that already decoded no longer form a stream,
    // which is worth saying rather than skipping the line.
    let feed = match oq_data::TickStream::new(header.instrument, ticks.clone()) {
        Ok(stream) => Some(stream.feed_latency_summary()),
        Err(e) => {
            println!("feed latency     unavailable: {e:?}");
            None
        }
    };
    if let Some(feed) = feed {
        if feed.carries_latency {
            println!(
                "feed latency     min {} / mean {} / max {} ns",
                feed.min, feed.mean, feed.max
            );
            if feed.negative > 0 {
                // Not an error. It bounds how far the figures above can be
                // trusted, which is why it prints beside them rather than
                // in a footnote nobody reaches.
                println!(
                    "  arrived early  {} record(s) — the capture host's clock ran behind \
                 the venue's, so the latencies above are understated by that much",
                    feed.negative
                );
            }
        } else {
            println!(
                "feed latency     none carried: every arrival time equals its exchange \
             time, so this file cannot support latency-aware work"
            );
        }
    }

    // What kind of series this is, as opposed to whether it is intact.
    //
    // Everything above asks whether the file holds what it says it
    // holds. This asks whether what it holds behaves like a market --
    // a different question, and the one that decides whether a result
    // measured on it means anything outside it. The generated fixtures
    // in this workspace hold almost none of these facts, which is fine
    // for a fixture and not fine for a claim about a strategy.
    println!();
    let returns = log_returns(&ticks);
    match oq_stats::StylizedFacts::measure(&returns) {
        Ok(facts) => {
            println!(
                "stylized facts   {} of 4 hold, from {} returns",
                facts.held(),
                facts.n
            );
            for line in facts.render().lines() {
                println!("  {line}");
            }
        }
        Err(e) => {
            // Not a defect in the file. A short window or a quiet
            // instrument produces too few traded ticks to say anything,
            // and saying nothing is the correct answer to that.
            println!(
                "stylized facts   not measurable: {e} (from {} returns)",
                returns.len()
            );
        }
    }

    println!();
    let mut problems = Vec::new();
    if backwards > 0 {
        problems.push(format!(
            "{backwards} ticks arrive before the one before them"
        ));
    }
    if volume_drops > 0 {
        problems.push(format!(
            "{volume_drops} volume decreases in a cumulative series"
        ));
    }
    if crossed > 0 {
        problems.push(format!(
            "{crossed} ticks where the bid is at or above the ask"
        ));
    }
    if header.count as usize != ticks.len() {
        problems.push("the header's count disagrees with the records".to_string());
    }
    if let Some(out) = export
        && !write_parquet(&path, header.instrument, ticks, &out)
    {
        return ExitCode::FAILURE;
    }

    if problems.is_empty() {
        println!("verdict          nothing here contradicts itself");
        ExitCode::SUCCESS
    } else {
        println!("verdict          {} problem(s):", problems.len());
        for p in &problems {
            println!("  - {p}");
        }
        println!();
        println!("None of these stop a backtest running. That is why they are worth");
        println!("printing: the run would produce numbers either way.");
        ExitCode::FAILURE
    }
}

/// Export the ticks as Parquet, reporting what it cost.
///
/// The size comparison is printed rather than assumed: a columnar
/// export that turns out to be larger than the native file is a fact the
/// person running it should have before they build a pipeline on it.
#[cfg(feature = "parquet")]
fn write_parquet(src: &str, instrument: u64, ticks: Vec<oq_engine::Tick>, out: &str) -> bool {
    let n = ticks.len();
    let stream = match oq_data::TickStream::new(instrument, ticks) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oq-data: {src}: {e:?}");
            return false;
        }
    };
    if let Err(e) = oq_data::columnar::write_parquet(&stream, out) {
        eprintln!("oq-data: {out}: {e}");
        return false;
    }
    let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let (before, after) = (size(src), size(out));
    println!();
    println!("parquet          {out}");
    println!(
        "                 {n} rows, {after} bytes vs {before} native ({:.0}%)",
        if before == 0 {
            0.0
        } else {
            after as f64 * 100.0 / before as f64
        }
    );
    true
}

/// Without the feature there is no exporter, and saying so beats an
/// unrecognised flag: the flag is real, this build just cannot serve it.
#[cfg(not(feature = "parquet"))]
fn write_parquet(_src: &str, _instrument: u64, _ticks: Vec<oq_engine::Tick>, _out: &str) -> bool {
    eprintln!(
        "oq-data: this build has no Parquet support; rebuild with \
         `cargo build -p oq-data --features parquet`"
    );
    false
}
