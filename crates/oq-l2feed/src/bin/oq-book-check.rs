//! `oq-book-check` — replay a capture file into an order book and
//! report whether it reconstructs.
//!
//! ```text
//! oq-book-check --file archive/binance-perp/BTCUSDT/depth/2026-08-16.oqcap
//! ```
//!
//! This answers the question a capture archive cannot answer about
//! itself: the files exist and the bytes are intact, but do the
//! messages in them actually rebuild a book?
//!
//! Run it early and run it often. A capture defect — a mishandled
//! reconnect, a misread sequence field, a stream that turned out to be
//! coalesced — is cheap to fix on day one and unrecoverable after six
//! months, because the window it corrupted cannot be recaptured.
//!
//! It reports rather than judges. Gaps are expected in any real
//! capture; what matters is whether they are *marked*, whether the
//! stream resynchronizes after them, and whether the book stays
//! consistent in between.

use std::process::ExitCode;

use oq_l2feed::book::{Applied, Book, SequenceError};
use oq_l2feed::depth::{Scales, parse_depth};
use oq_l2feed::frame::{Kind, decode_all};
use oq_l2feed::manifest::is_gap;

const USAGE: &str = "\
oq-book-check — replay a capture file into an order book

USAGE:
    oq-book-check --file <PATH> [OPTIONS]

OPTIONS:
    --file <PATH>        Capture file (.oqcap)
    --venue <NAME>       Venue whose instrument table to use [default: binance-perp]
    --symbol <SYMBOL>    Symbol, for looking up quoting precision
    --price-scale <N>    Override the looked-up price precision
    --qty-scale <N>      Override the looked-up quantity precision
    --max-report <N>     Sequence problems to list [default: 10]
    --help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let Some(path) = value("--file") else {
        eprintln!("oq-book-check: missing --file\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    // Precision comes from the venue's instrument table, not from a
    // default. Guessing it is not a small error: replaying HYPEUSDT with
    // two decimals reported eleven thousand unparseable messages for
    // prices like "57.45300" that are valid at five, which reads as a
    // corrupt archive rather than a mis-set flag.
    let venue_id = value("--venue").unwrap_or_else(|| "binance-perp".to_string());
    let symbol = value("--symbol").or_else(|| symbol_from_path(&path));
    let looked_up = symbol
        .as_deref()
        .and_then(|s| oq_l2feed::venue::by_id(&venue_id).and_then(|v| v.instrument(s)));

    if looked_up.is_none() && value("--price-scale").is_none() {
        eprintln!(
            "oq-book-check: no instrument definition for {:?} on {venue_id}; \
             pass --symbol, or --price-scale and --qty-scale explicitly, \
             rather than letting a default decide how to read prices",
            symbol.as_deref().unwrap_or("<unknown>")
        );
        return ExitCode::FAILURE;
    }

    let scales = Scales {
        price: value("--price-scale")
            .and_then(|v| v.parse().ok())
            .or_else(|| looked_up.map(|i| u32::from(i.price_scale)))
            .unwrap_or(2),
        qty: value("--qty-scale")
            .and_then(|v| v.parse().ok())
            .or_else(|| looked_up.map(|i| u32::from(i.qty_scale)))
            .unwrap_or(3),
    };
    println!(
        "scales          price {} / qty {}",
        scales.price, scales.qty
    );
    let max_report: usize = value("--max-report")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("oq-book-check: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (records, torn) = match decode_all(&bytes) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("oq-book-check: {path} is damaged: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut book = Book::new();
    let mut stats = Stats::default();
    let mut problems: Vec<String> = Vec::new();

    for (index, record) in records.iter().enumerate() {
        if record.kind == Kind::Control {
            if is_gap(record) {
                stats.gap_markers += 1;
                // A marked gap means the capture knows it stopped
                // listening. The book cannot span it, so it is dropped
                // and waits for a fresh snapshot.
                book = Book::new();
            }
            continue;
        }

        let update = match parse_depth(&record.payload, scales) {
            Ok(u) => u,
            Err(e) => {
                stats.unparseable += 1;
                if problems.len() < max_report {
                    problems.push(format!("  [{index}] unparseable: {e}"));
                }
                continue;
            }
        };
        stats.updates += 1;

        // The archive holds diffs; without a captured REST snapshot the
        // first update bootstraps the book. Prices are still exact and
        // sequencing is still checked — only the levels that existed
        // before the capture began are missing, and they are not
        // knowable from this file.
        if stats.updates == 1 {
            book.install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
            stats.bootstrapped = true;
        }

        match book.apply(&update) {
            Ok(Applied::Updated) => {
                stats.applied += 1;
                if book.is_crossed() {
                    stats.crossed += 1;
                    if problems.len() < max_report {
                        problems.push(format!("  [{index}] book crossed after update"));
                    }
                }
                stats.max_bid_depth = stats.max_bid_depth.max(book.bids().depth());
                stats.max_ask_depth = stats.max_ask_depth.max(book.asks().depth());
            }
            Ok(Applied::AlreadyInSnapshot) => stats.pre_snapshot += 1,
            Err(e) => {
                stats.sequence_errors += 1;
                if problems.len() < max_report {
                    problems.push(format!("  [{index}] {e}"));
                }
                // Resynchronize the way a live consumer would: drop the
                // book and rebuild from the next update.
                book = Book::new();
                book.install_snapshot(update.first_id.saturating_sub(1), &[], &[]);
                let _ = book.apply(&update);
                stats.resyncs += 1;
            }
        }
    }

    println!("file            {path}");
    println!("records         {}", records.len());
    if torn > 0 {
        println!(
            "torn tail       {torn} bytes (the writer was interrupted; expected after a crash)"
        );
    }
    println!("depth updates   {}", stats.updates);
    println!("  applied       {}", stats.applied);
    println!("  pre-snapshot  {}", stats.pre_snapshot);
    println!("  unparseable   {}", stats.unparseable);
    println!("gap markers     {}", stats.gap_markers);
    println!("sequence errors {}", stats.sequence_errors);
    println!("resyncs         {}", stats.resyncs);
    println!("crossed book    {}", stats.crossed);
    println!(
        "book depth      {} bids / {} asks at the deepest",
        stats.max_bid_depth, stats.max_ask_depth
    );
    if stats.bootstrapped {
        println!();
        println!("Bootstrapped from the first update rather than a captured REST snapshot,");
        println!("so levels that existed before the capture began are absent. Sequencing and");
        println!("prices are still checked exactly.");
    }

    if !problems.is_empty() {
        println!();
        println!("first {} problem(s):", problems.len());
        for p in &problems {
            println!("{p}");
        }
    }

    println!();
    let verdict = if stats.updates == 0 {
        "NO DEPTH UPDATES — wrong file, or the stream sent nothing"
    } else if stats.unparseable > 0 {
        "UNPARSEABLE MESSAGES — the archive holds bytes this build cannot read"
    } else if stats.crossed > 0 {
        "CROSSED BOOK — reconstruction is wrong, not the market"
    } else if stats.sequence_errors > stats.gap_markers {
        "SEQUENCE BREAKS BEYOND THE MARKED GAPS — messages were lost silently"
    } else {
        "RECONSTRUCTS CLEANLY"
    };
    println!("verdict: {verdict}");

    // A crossed book or an unreadable message means the archive is not
    // what it claims. Sequence breaks that line up with marked gaps are
    // normal and do not fail.
    if stats.updates == 0
        || stats.unparseable > 0
        || stats.crossed > 0
        || stats.sequence_errors > stats.gap_markers
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Recover the symbol from an archive path such as
/// `.../binance-perp/BTCUSDT/depth/2026-08-16/09.oqcap`.
///
/// The archive layout already records it, so requiring the operator to
/// repeat it is an invitation to repeat it wrongly.
fn symbol_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let depth_idx = parts.iter().rposition(|p| {
        matches!(
            *p,
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice"
        )
    })?;
    parts
        .get(depth_idx.checked_sub(1)?)
        .map(|s| (*s).to_string())
}

#[derive(Default)]
struct Stats {
    updates: u64,
    applied: u64,
    pre_snapshot: u64,
    unparseable: u64,
    gap_markers: u64,
    sequence_errors: u64,
    resyncs: u64,
    crossed: u64,
    max_bid_depth: usize,
    max_ask_depth: usize,
    bootstrapped: bool,
}

#[allow(dead_code)]
fn assert_error_is_used(_: SequenceError) {}
