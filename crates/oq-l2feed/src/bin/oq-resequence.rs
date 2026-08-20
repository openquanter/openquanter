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

    let bytes = match oq_l2feed::archive::read(&file) {
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

    let r = reorder(&records, venue.as_ref(), scales);

    println!("file             {file}");
    println!("records          {} (torn {torn})", records.len());
    println!("  sequenced      {}", r.sequenced);
    println!("  control        {}", r.controls);
    println!("  unsequenced    {}", r.unsequenced);
    println!("out of order     {}", r.out_of_order);
    println!("repeats removed  {}", r.removed);

    let out_of_order = r.out_of_order;
    let removed = r.removed;
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
    for rec in &r.kept {
        rec.encode(&mut encoded);
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
    match rebuild_manifest(&out, &venue_id, &encoded, &r.kept) {
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

/// The file put back into the venue's order.
struct Reordered<'a> {
    /// Every record that survives, in the order to write them.
    kept: Vec<&'a Record>,
    sequenced: usize,
    controls: usize,
    unsequenced: usize,
    /// Sequenced records that arrived after a later one.
    out_of_order: usize,
    /// Repeated ids dropped.
    removed: usize,
}

/// Sort by the venue's sequence, keeping every other record where it
/// was.
///
/// A control record has no sequence number of its own, and its position
/// *is* its meaning: a gap marker says the capture stopped listening
/// **here**, and `oq-book-check` reads it positionally to tell a
/// disconnect the capture declared from messages that went missing
/// silently. Collecting the controls and writing them as a block would
/// move every one of them to the top of the file, which turns a break
/// the capture owned up to into `breaks nobody declared` — a repair that
/// manufactures the appearance of data loss.
///
/// So each one is anchored to the last sequenced record that arrived
/// before it and travels with it. `None` sorts first, which is where a
/// `session_start` written before any data belongs. Records this tool
/// does not sequence are anchored the same way, for the same reason.
fn reorder<'a>(records: &'a [Record], venue: &dyn venue::Venue, scales: Scales) -> Reordered<'a> {
    let mut keyed: Vec<(Option<u64>, &Record, bool)> = Vec::with_capacity(records.len());
    let mut anchor: Option<u64> = None;
    let mut sequenced = 0usize;
    let mut controls = 0usize;
    let mut unsequenced = 0usize;
    let mut arrival: Vec<u64> = Vec::new();

    for r in records {
        if r.kind == Kind::Control {
            controls += 1;
            keyed.push((anchor, r, false));
        } else if let Ok(u) = venue.parse_depth(&r.payload, scales) {
            sequenced += 1;
            anchor = Some(u.final_id);
            arrival.push(u.final_id);
            keyed.push((Some(u.final_id), r, true));
        } else {
            unsequenced += 1;
            keyed.push((anchor, r, false));
        }
    }

    let out_of_order = arrival.windows(2).filter(|w| w[1] < w[0]).count();

    // Stable, so records sharing an anchor keep the order they arrived
    // in — and so a file already in order comes out byte for byte the
    // same.
    keyed.sort_by_key(|(k, _, _)| *k);

    // A repeated id is one message recorded twice, not two messages. The
    // first copy is kept. Checked against everything seen rather than
    // against the previous record, because an anchored control can sit
    // between two copies of the same id.
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::with_capacity(keyed.len());
    let mut removed = 0usize;
    for (key, r, is_sequenced) in keyed {
        if is_sequenced {
            let id = key.expect("a sequenced record carries its id");
            if !seen.insert(id) {
                removed += 1;
                continue;
            }
        }
        kept.push(r);
    }

    Reordered {
        kept,
        sequenced,
        controls,
        unsequenced,
        out_of_order,
        removed,
    }
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
    kept: &[&Record],
) -> std::io::Result<Option<String>> {
    let path = Path::new(out);
    let Some((stream, window, root)) = describe(path, venue_id) else {
        return Ok(None);
    };

    let mut builder = ManifestBuilder::new();
    for r in kept {
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
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice" | "fundingRate"
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
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice" | "fundingRate"
        )
    })?;
    parts.get(idx.checked_sub(1)?).map(|s| (*s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALES: Scales = Scales { price: 2, qty: 3 };

    fn depth(u: u64) -> Record {
        let payload = format!(
            r#"{{"e":"depthUpdate","E":1,"s":"BTCUSDT","U":{u},"u":{u},"pu":{},"b":[["100.0","1.0"]],"a":[]}}"#,
            u - 1
        );
        Record {
            kind: Kind::Payload,
            local_ts: 1,
            exch_ts: 1,
            payload: payload.into_bytes(),
        }
    }

    fn control(json: &str) -> Record {
        Record {
            kind: Kind::Control,
            local_ts: 1,
            exch_ts: 1,
            payload: json.as_bytes().to_vec(),
        }
    }

    fn gap() -> Record {
        control(r#"{"type":"gap","reason":"disconnect"}"#)
    }

    fn start() -> Record {
        control(r#"{"type":"session_start","venue":"binance-perp"}"#)
    }

    /// Payload that is not a book update — a trade sharing the file.
    fn other() -> Record {
        Record {
            kind: Kind::Payload,
            local_ts: 1,
            exch_ts: 1,
            payload: br#"{"e":"trade","t":7,"p":"1"}"#.to_vec(),
        }
    }

    fn run(records: &[Record]) -> Reordered<'_> {
        let venue = venue::by_id("binance-perp").expect("binance");
        reorder(records, venue.as_ref(), SCALES)
    }

    /// Where a gap marker sits is what it means: the capture stopped
    /// listening *here*. `oq-book-check` reads it positionally to
    /// separate a disconnect the capture declared from messages that
    /// went missing silently, so a repair that moves every control to
    /// the top of the file manufactures the appearance of data loss —
    /// measured, it turned a clean file into "1 break nobody declared".
    #[test]
    fn a_gap_marker_keeps_its_place_between_the_records_it_separated() {
        let records = vec![
            start(),
            depth(10),
            depth(11),
            gap(),
            // After the gap the ids jump, and these two arrived swapped.
            depth(51),
            depth(50),
        ];
        let r = run(&records);
        let shape: Vec<String> = r
            .kept
            .iter()
            .map(|rec| {
                if rec.kind == Kind::Control {
                    if String::from_utf8_lossy(&rec.payload).contains("\"type\":\"gap\"") {
                        "gap".to_string()
                    } else {
                        "start".to_string()
                    }
                } else {
                    let venue = venue::by_id("binance-perp").expect("binance");
                    format!(
                        "{}",
                        venue
                            .parse_depth(&rec.payload, SCALES)
                            .expect("depth")
                            .final_id
                    )
                }
            })
            .collect();
        assert_eq!(
            shape,
            vec!["start", "10", "11", "gap", "50", "51"],
            "the gap must still separate the records it separated before"
        );
        assert_eq!(r.out_of_order, 1);
    }

    /// A control written before any data has nothing to anchor to and
    /// belongs at the front, which is where the session header goes.
    #[test]
    fn a_header_written_before_any_data_stays_first() {
        let records = vec![start(), depth(20), depth(19)];
        let r = run(&records);
        assert_eq!(r.kept[0].kind, Kind::Control);
        assert_eq!(r.controls, 1);
    }

    /// The tool must be inert on a healthy file. Anything else means
    /// running it as a precaution is itself a risk.
    #[test]
    fn a_file_already_in_order_comes_out_byte_for_byte_the_same() {
        let records = vec![start(), depth(10), gap(), depth(11), other(), depth(12)];
        let r = run(&records);
        assert_eq!(r.out_of_order, 0);
        assert_eq!(r.removed, 0);

        let mut before = Vec::new();
        for rec in &records {
            rec.encode(&mut before);
        }
        let mut after = Vec::new();
        for rec in &r.kept {
            rec.encode(&mut after);
        }
        assert_eq!(before, after, "a healthy file must survive untouched");
    }

    /// Two writers can leave the same id on either side of a control
    /// record, so the duplicate check cannot look only at the record
    /// before it.
    #[test]
    fn a_repeat_is_dropped_even_with_a_control_between_the_copies() {
        let records = vec![depth(10), gap(), depth(10), depth(11)];
        let r = run(&records);
        assert_eq!(r.removed, 1);
        assert_eq!(r.sequenced, 3, "the input had three sequenced records");
        assert_eq!(
            r.kept.len(),
            3,
            "one duplicate dropped, the gap and both distinct ids kept"
        );
    }

    /// A file that mixed streams keeps the one this tool cannot
    /// sequence, and keeps it where it was rather than in a block at the
    /// front.
    #[test]
    fn a_record_this_tool_cannot_sequence_keeps_its_place() {
        let records = vec![depth(10), other(), depth(12), depth(11)];
        let r = run(&records);
        assert_eq!(r.unsequenced, 1);
        assert_eq!(r.kept.len(), 4);
        assert_eq!(
            r.kept[1].payload,
            other().payload,
            "it followed id 10 on the way in and still does"
        );
    }
}
