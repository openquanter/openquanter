//! `oq-ingest` — convert a captured archive into a tick file.
//!
//! ```text
//! oq-ingest --archive /data/binance-perp/BTCUSDT --day 2026-08-16 --out btc.ticks
//! ```
//!
//! Reads the `depth` and `trade` streams for one instrument and one day,
//! folds them into fixed windows, and writes the format the engine
//! replays. The archive is not modified; this produces a projection
//! beside it.
//!
//! Quoting precision comes from the venue's instrument table. It is not
//! guessable and getting it wrong does not fail loudly — it silently
//! rescales every price — so this refuses to run rather than assume.

use std::path::PathBuf;
use std::process::ExitCode;

use oq_ingest::batches::{hours, load_hour};
use oq_ingest::{Aggregator, Report, Source, fold_into};
use oq_l2feed::depth::Scales;

const USAGE: &str = "\
oq-ingest — convert a captured archive into a tick file

USAGE:
    oq-ingest --archive <DIR> --day <YYYY-MM-DD> --out <FILE> [OPTIONS]

OPTIONS:
    --archive <DIR>      Instrument directory, e.g. .../binance-perp/BTCUSDT
    --day <DATE>         UTC day to convert
    --out <FILE>         Tick file to write
    --window-ms <N>      Window length in milliseconds [default: 1000]
    --venue <NAME>       Venue whose instrument table to use [default: binance-perp]
    --symbol <SYMBOL>    Override the symbol inferred from the archive path
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

    let (Some(archive), Some(day), Some(out)) =
        (value("--archive"), value("--day"), value("--out"))
    else {
        eprintln!("oq-ingest: --archive, --day and --out are all required\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let archive = PathBuf::from(archive);

    let venue_id = value("--venue").unwrap_or_else(|| "binance-perp".to_string());
    let symbol = value("--symbol").or_else(|| {
        archive
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
    });
    let Some(symbol) = symbol else {
        eprintln!(
            "oq-ingest: cannot tell which symbol {} holds; pass --symbol",
            archive.display()
        );
        return ExitCode::FAILURE;
    };
    let Some(venue) = oq_l2feed::venue::by_id(&venue_id) else {
        eprintln!(
            "oq-ingest: unknown venue {venue_id:?}; known: {}",
            oq_l2feed::venue::known_ids().join(", ")
        );
        return ExitCode::FAILURE;
    };
    let Some(instrument) = venue.instrument(&symbol) else {
        eprintln!(
            "oq-ingest: no instrument definition for {symbol:?} on {venue_id}. \
             Quoting precision cannot be guessed: a wrong scale rescales every \
             price without failing, so this stops rather than assume one."
        );
        return ExitCode::FAILURE;
    };
    let scales = Scales {
        price: u32::from(instrument.price_scale),
        qty: u32::from(instrument.qty_scale),
    };

    let window_ms: i64 = value("--window-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);

    // One hour at a time, with the aggregator carried across hours.
    //
    // Loading a whole day and sorting it was measured, on the machine
    // that holds the data, as a process the kernel killed: a day of one
    // instrument's depth is millions of records, the parsed form is
    // larger than the bytes, and that host has 1 GiB. The archive is
    // written one file per hour, so an hour is the batch the data
    // already offers, and carrying the aggregator is what keeps the
    // result identical — the book, the cumulative volume and the open
    // window all span hours.
    let mut agg = match Aggregator::new(window_ms * 1_000_000) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("oq-ingest: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut report = Report::default();
    let mut ticks = Vec::new();
    let mut batches = 0_u32;

    let hours = hours(&archive, &day);
    if hours.is_empty() {
        eprintln!("oq-ingest: nothing to convert under {}", archive.display());
        return ExitCode::FAILURE;
    }

    for hour in &hours {
        let loaded = match load_hour(&archive, &day, hour) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("oq-ingest: {e}");
                return ExitCode::FAILURE;
            }
        };
        for batch in &loaded {
            if batch.torn > 0 {
                println!(
                    "{:<7} {hour}: {} torn bytes at the end, decoding the rest",
                    batch.stream, batch.torn
                );
            }
        }
        if loaded.is_empty() {
            continue;
        }
        let sources: Vec<Source<'_>> = loaded
            .iter()
            .map(|b| Source {
                records: &b.records,
                stream: b.stream,
            })
            .collect();
        ticks.extend(fold_into(
            venue.as_ref(),
            &sources,
            scales,
            &mut agg,
            &mut report,
        ));
        batches += 1;
        // Freed here, at the end of each hour, which is the whole point.
        drop(loaded);
    }
    ticks.extend(agg.flush());
    report.absorb(agg.counts(), ticks.len());
    println!("batches {batches} hour(s) folded one at a time");
    // The instrument id is opaque to the tick format; a stable hash of
    // venue and symbol keeps two different instruments from sharing one,
    // which is the failure that silently mixes two books together.
    let instrument_id = fnv1a(format!("{venue_id}:{symbol}").as_bytes());
    let encoded = oq_data::ticks::encode(instrument_id, &ticks);
    if let Err(e) = std::fs::write(&out, &encoded) {
        eprintln!("oq-ingest: cannot write {out}: {e}");
        return ExitCode::FAILURE;
    }

    println!();
    println!("symbol          {symbol} on {venue_id}");
    println!(
        "scales          price {} / qty {}",
        scales.price, scales.qty
    );
    println!("window          {window_ms} ms");
    println!("ticks           {}", report.ticks);
    println!("  from trades   {}", report.trades);
    println!("  from depth    {}", report.depth_applied);
    println!("  quiet windows {}", report.quiet_windows);
    if report.windows_before_first_trade > 0 {
        // Separate from the quiet-window count: a quiet window is a
        // market that did not trade, these are windows the capture
        // reached before its trade stream had said anything at all.
        // They are the only ones dropped, so this is the difference
        // between the windows crossed and the ticks written, and a
        // reader comparing a re-converted file against an older one
        // should not have to derive it.
        println!(
            "  before 1st trade {} dropped — the trade stream had not spoken yet",
            report.windows_before_first_trade
        );
    }
    println!("gap markers     {}", report.gaps);
    println!("unparseable     {}", report.unparseable);
    if report.non_trades > 0 {
        println!(
            "not a trade     {} — records the venue publishes on the trade stream \
             that declare no trade",
            report.non_trades
        );
    }
    println!("wrote           {out} ({} bytes)", encoded.len());

    if report.ticks == 0 {
        eprintln!("\noq-ingest: no ticks produced; wrong day, or an empty archive");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Every capture file for one stream and day, under either rotation.
///
/// FNV-1a, for a stable instrument id that does not depend on hash seeds.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}
