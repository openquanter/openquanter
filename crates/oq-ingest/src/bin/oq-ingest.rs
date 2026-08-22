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

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use oq_ingest::{Source, to_ticks};
use oq_l2feed::depth::Scales;
use oq_l2feed::frame::decode_all;

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

    let mut loaded = Vec::new();
    for stream in ["depth", "trade"] {
        let mut bytes = Vec::new();
        for path in files_for(&archive, stream, &day) {
            match std::fs::read(&path) {
                Ok(b) => bytes.extend_from_slice(&b),
                Err(e) => {
                    eprintln!("oq-ingest: cannot read {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
        if bytes.is_empty() {
            println!("{stream:<7} no files for {day}");
            continue;
        }
        match decode_all(&bytes) {
            Ok((records, torn)) => {
                if torn > 0 {
                    println!("{stream:<7} {torn} torn bytes at the end, decoding the rest");
                }
                println!("{stream:<7} {} records", records.len());
                loaded.push((stream, records));
            }
            Err(e) => {
                eprintln!("oq-ingest: {stream} is damaged: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if loaded.is_empty() {
        eprintln!("oq-ingest: nothing to convert under {}", archive.display());
        return ExitCode::FAILURE;
    }

    let sources: Vec<Source<'_>> = loaded
        .iter()
        .map(|(stream, records)| Source { records, stream })
        .collect();

    let (ticks, report) = match to_ticks(&sources, scales, window_ms * 1_000_000) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("oq-ingest: {e}");
            return ExitCode::FAILURE;
        }
    };

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
    println!("gap markers     {}", report.gaps);
    println!("unparseable     {}", report.unparseable);
    println!("wrote           {out} ({} bytes)", encoded.len());

    if report.ticks == 0 {
        eprintln!("\noq-ingest: no ticks produced; wrong day, or an empty archive");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Every capture file for one stream and day, under either rotation.
///
/// Daily rotation writes `<stream>/<day>.oqcap`; hourly writes
/// `<stream>/<day>/HH.oqcap`. Both are read, so a day that was captured
/// across a rotation change still converts as one day.
fn files_for(archive: &Path, stream: &str, day: &str) -> Vec<PathBuf> {
    let dir = archive.join(stream);
    let mut out = Vec::new();

    let daily = dir.join(format!("{day}.oqcap"));
    if daily.is_file() {
        out.push(daily);
    }
    if let Ok(entries) = std::fs::read_dir(dir.join(day)) {
        let mut hourly: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "oqcap"))
            .collect();
        hourly.sort();
        out.extend(hourly);
    }
    out
}

/// FNV-1a, for a stable instrument id that does not depend on hash seeds.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}
