//! `oq-resequence` — put a capture file back into the venue's order.
//!
//! ```text
//! oq-resequence --file 12.oqcap --symbol BTCUSDT --out 12-fixed.oqcap
//! ```
//!
//! For a file two writers appended to at once. That should not happen,
//! and when it does the damage is not loss but disorder: both writers
//! received the same broadcast, so every message is present, some twice,
//! in the order two sockets happened to deliver them rather than the
//! order the venue produced them.
//!
//! Timestamps cannot undo it. Many depth updates share a millisecond, so
//! sorting by time leaves them in an arbitrary order within each one —
//! measured on a real damaged file, sorting by timestamp took 10
//! reported breaks to 26. The venue's sequence number can, because the
//! venue already stated the order; sorting by it took the same file from
//! 41 breaks to 1, and that last one was a real loss no reordering could
//! recover.
//!
//! What this cannot do is invent messages. A file that is missing
//! updates still reports breaks afterwards, which is the honest outcome
//! and the reason the tool reports before-and-after rather than claiming
//! success.

use std::process::ExitCode;

use std::path::Path;

use oq_l2feed::day::{Rotation, Window};
use oq_l2feed::depth::Scales;
use oq_l2feed::frame::{Kind, Record, decode_all};
use oq_l2feed::manifest::ManifestBuilder;
use oq_l2feed::stream::{Software, StreamId};
use oq_l2feed::venue;

const USAGE: &str = "\
oq-resequence — put a capture file back into the venue's order

USAGE:
    oq-resequence --file <PATH> --out <PATH> [OPTIONS]

OPTIONS:
    --file <PATH>        Damaged capture file
    --out <PATH>         Where to write the reordered file
    --venue <NAME>       Venue whose sequencing to use [default: binance-perp]
    --symbol <SYMBOL>    Symbol, for quoting precision
    --dry-run            Report what would change, write nothing
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
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let Some(file) = value("--file") else {
        eprintln!("oq-resequence: --file is required\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let out = value("--out");
    if out.is_none() && !dry_run {
        eprintln!("oq-resequence: --out is required unless --dry-run\n\n{USAGE}");
        return ExitCode::FAILURE;
    }

    let venue_id = value("--venue").unwrap_or_else(|| "binance-perp".to_string());
    let Some(venue) = venue::by_id(&venue_id) else {
        eprintln!(
            "oq-resequence: unknown venue {venue_id:?}; known: {}",
            venue::known_ids().join(", ")
        );
        return ExitCode::FAILURE;
    };

    let symbol = value("--symbol").or_else(|| symbol_from_path(&file));
    let Some(instrument) = symbol.as_deref().and_then(|s| venue.instrument(s)) else {
        eprintln!(
            "oq-resequence: no instrument definition for {:?} on {venue_id}; \
             pass --symbol. Precision decides how prices parse, and a wrong \
             one silently rescales them.",
            symbol.as_deref().unwrap_or("<unknown>")
        );
        return ExitCode::FAILURE;
    };
    let scales = Scales {
        price: u32::from(instrument.price_scale),
        qty: u32::from(instrument.qty_scale),
    };

    let bytes = match std::fs::read(&file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("oq-resequence: cannot read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (records, torn) = match decode_all(&bytes) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("oq-resequence: {file} is damaged beyond decoding: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut keyed: Vec<(u64, &Record)> = Vec::new();
    let mut controls: Vec<&Record> = Vec::new();
    let mut unsequenced: Vec<&Record> = Vec::new();
    for r in &records {
        if r.kind == Kind::Control {
            controls.push(r);
        } else if let Ok(u) = venue.parse_depth(&r.payload, scales) {
            keyed.push((u.final_id, r));
        } else {
            // Not a book update for this venue. Kept in arrival order
            // rather than dropped: a file that mixed streams would
            // otherwise lose whichever stream this tool does not
            // sequence.
            unsequenced.push(r);
        }
    }

    let out_of_order = keyed.windows(2).filter(|w| w[1].0 < w[0].0).count();

    // A stable sort keeps the first copy of a repeated id; dedup then
    // drops the rest, because a repeat is one message recorded twice
    // rather than two messages.
    keyed.sort_by_key(|(id, _)| *id);
    let before = keyed.len();
    keyed.dedup_by_key(|(id, _)| *id);
    let removed = before - keyed.len();

    println!("file             {file}");
    println!("records          {} (torn {torn})", records.len());
    println!("  sequenced      {before}");
    println!("  control        {}", controls.len());
    println!("  unsequenced    {}", unsequenced.len());
    println!("out of order     {out_of_order}");
    println!("repeats removed  {removed}");

    if out_of_order == 0 && removed == 0 {
        println!();
        println!("verdict: NOTHING TO DO — already in order, with no repeats");
        return ExitCode::SUCCESS;
    }

    if dry_run {
        println!();
        println!("Dry run: nothing was written.");
        return ExitCode::SUCCESS;
    }

    let mut encoded = Vec::with_capacity(bytes.len());
    for r in &controls {
        r.encode(&mut encoded);
    }
    for r in &unsequenced {
        r.encode(&mut encoded);
    }
    for (_, r) in &keyed {
        r.encode(&mut encoded);
    }

    let out = out.expect("checked above");
    if let Err(e) = std::fs::write(&out, &encoded) {
        eprintln!("oq-resequence: cannot write {out}: {e}");
        return ExitCode::FAILURE;
    }
    println!("wrote            {out} ({} bytes)", encoded.len());

    // The manifest beside the old file described the old file. Leaving
    // it would be worse than deleting it: a manifest that miscounts is
    // one nothing downstream can tell is wrong, which is the whole
    // reason the field exists. Rebuild it where the layout says it goes,
    // and say so when the layout cannot be read.
    match rebuild_manifest(&out, &venue_id, &encoded, &keyed, &controls, &unsequenced) {
        Ok(Some(path)) => println!("manifest         rebuilt at {path}"),
        Ok(None) => {
            println!();
            println!("The output is not in an archive layout, so no manifest was");
            println!("written. If you move it into one, delete any manifest already");
            println!("there: one that describes the file before this ran is worse");
            println!("than none, because nothing downstream can tell it is stale.");
        }
        Err(e) => {
            eprintln!("oq-resequence: reordered file written, but its manifest could not be: {e}");
            return ExitCode::FAILURE;
        }
    }

    println!();
    println!("Reordering cannot recover a message that never arrived. Run");
    println!("oq-book-check on the result: breaks that remain are real losses.");
    ExitCode::SUCCESS
}

/// Write a manifest describing the reordered file, if its path says
/// where in an archive it belongs.
///
/// Returns `Ok(None)` when the path is not an archive layout — writing a
/// manifest next to a file somewhere arbitrary would be inventing a
/// claim about an archive that does not exist.
fn rebuild_manifest(
    out: &str,
    venue_id: &str,
    encoded: &[u8],
    keyed: &[(u64, &Record)],
    controls: &[&Record],
    unsequenced: &[&Record],
) -> std::io::Result<Option<String>> {
    let path = Path::new(out);
    let Some((stream, window, root)) = describe(path, venue_id) else {
        return Ok(None);
    };

    let mut builder = ManifestBuilder::new();
    for r in controls.iter().chain(unsequenced.iter()) {
        builder.observe(r);
    }
    for (_, r) in keyed {
        builder.observe(r);
    }

    let software = Software::new(
        concat!("oq-resequence ", env!("CARGO_PKG_VERSION")),
        option_env!("OQ_BUILD_COMMIT").unwrap_or("unknown"),
    );
    let manifest = builder.build(&stream, window, &software, encoded);
    let manifest_path = stream.manifest_for(&root, window);
    std::fs::write(&manifest_path, manifest.to_json())?;
    Ok(Some(manifest_path.display().to_string()))
}

/// Recover stream, window and archive root from a path such as
/// `.../binance-perp/BTCUSDT/depth/2026-08-16/12.oqcap`.
fn describe(path: &Path, venue_id: &str) -> Option<(StreamId, Window, std::path::PathBuf)> {
    let parts: Vec<&str> = path.iter().filter_map(|p| p.to_str()).collect();
    let file = parts.last()?.strip_suffix(".oqcap")?;
    let stream_at = parts.iter().rposition(|p| {
        matches!(
            *p,
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice"
        )
    })?;

    // Hourly puts the day between the stream and the file; daily does not.
    let (rotation, day_text) = if parts.len() == stream_at + 3 {
        (Rotation::Hourly, *parts.get(stream_at + 1)?)
    } else if parts.len() == stream_at + 2 {
        (Rotation::Daily, file)
    } else {
        return None;
    };

    let symbol = parts.get(stream_at.checked_sub(1)?)?;
    let stream = StreamId::new(venue_id, *symbol, parts[stream_at]);

    // The window comes from the data's own day, read back from the path
    // the writer produced, so a file that ended up somewhere unexpected
    // is rejected rather than mislabelled.
    let day = parse_day(day_text)?;
    let hour = if rotation == Rotation::Hourly {
        Some(file.parse::<u32>().ok()?)
    } else {
        None
    };

    let root: std::path::PathBuf = parts[..stream_at.checked_sub(2)?].iter().collect();
    Some((stream, Window { day, hour }, root))
}

/// `YYYY-MM-DD` to a UTC day.
fn parse_day(text: &str) -> Option<oq_l2feed::day::UtcDay> {
    let mut it = text.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(oq_l2feed::day::UtcDay(era * 146_097 + doe - 719_468))
}

/// Recover the symbol from an archive path.
fn symbol_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let idx = parts.iter().rposition(|p| {
        matches!(
            *p,
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice"
        )
    })?;
    parts.get(idx.checked_sub(1)?).map(|s| (*s).to_string())
}
