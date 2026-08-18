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
";

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        print!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if path == "--help" || path == "-h" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
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
