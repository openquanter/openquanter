//! `oq-merge` — combine two capture trees recorded over the same period.
//!
//! ```text
//! oq-merge --primary archive --secondary archive-overlap --out archive-merged
//! ```
//!
//! This exists for the overlapping upgrade: a new capture generation is
//! started on a second archive root before the old one is stopped, so
//! that no message is missed during the changeover. That leaves the two
//! generations holding the same window, and this puts them back
//! together.
//!
//! ## What counts as the same message
//!
//! Two websocket connections subscribed to the same stream receive the
//! same broadcast, so a duplicated message is byte-identical in both
//! trees. The key is therefore `(exch_ts, payload)`, which needs no
//! per-stream parsing and is right for every stream at once.
//!
//! REST-polled streams are the interesting case and fall out correctly:
//! two generations poll independently, so their samples carry different
//! event times and are not duplicates at all. Both survive, which is
//! what should happen — they are two genuine observations, not one
//! observation recorded twice.
//!
//! ## Which copy is kept
//!
//! The primary's, wherever both have a message.
//!
//! This is not arbitrary. `local_ts` records when this host saw a
//! message, and it exists so that latency can be modelled later.
//! Choosing per-message between two connections — taking the earlier,
//! say — would quietly bias that: the minimum of two draws is not
//! distributed like one draw, so the merged file would imply a market
//! closer than either connection actually saw. Keeping one coherent
//! connection's timeline, with exactly one switchover, leaves the
//! statistics meaning what they meant in the source.
//!
//! ## Ordering
//!
//! Primary's records in their original order, then the secondary
//! records the primary does not have, in theirs. Because the primary
//! covers the earlier part of the window and the secondary the later,
//! what survives deduplication from the secondary is its tail, and
//! appending it keeps the file in arrival order. Sorting by timestamp
//! was rejected: the file records the order this host observed, and
//! reordering it would destroy exactly the evidence it was kept for.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use oq_l2feed::day::{Rotation, Window};
use oq_l2feed::frame::{Kind, Record, decode_all};
use oq_l2feed::manifest::ManifestBuilder;
use oq_l2feed::stream::{Software, StreamId};

const USAGE: &str = "\
oq-merge — combine two capture trees recorded over the same period

USAGE:
    oq-merge --primary <DIR> --secondary <DIR> --out <DIR> [OPTIONS]

OPTIONS:
    --primary <DIR>     Tree whose copy wins where both have a message
    --secondary <DIR>   Tree contributing what the primary is missing
    --out <DIR>         Where the merged tree is written
    --dry-run           Report what would be merged, write nothing
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

    let (Some(primary), Some(secondary), Some(out)) =
        (value("--primary"), value("--secondary"), value("--out"))
    else {
        eprintln!("oq-merge: --primary, --secondary and --out are all required\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let (primary, secondary, out) = (
        PathBuf::from(primary),
        PathBuf::from(secondary),
        PathBuf::from(out),
    );

    let mut relatives: Vec<PathBuf> = Vec::new();
    for root in [&primary, &secondary] {
        collect(root, root, &mut relatives);
    }
    relatives.sort();
    relatives.dedup();

    if relatives.is_empty() {
        eprintln!("oq-merge: neither tree holds any .oqcap file");
        return ExitCode::FAILURE;
    }

    let mut totals = Totals::default();
    for rel in &relatives {
        match merge_one(&primary, &secondary, &out, rel, dry_run) {
            Ok(stats) => {
                println!(
                    "{:<58} primary {:>7}  new from secondary {:>7}  duplicates dropped {:>7}",
                    rel.display(),
                    stats.from_primary,
                    stats.from_secondary,
                    stats.duplicates
                );
                totals.add(&stats);
            }
            Err(e) => {
                eprintln!("oq-merge: {} failed: {e}", rel.display());
                totals.failed += 1;
            }
        }
    }

    println!();
    println!("files            : {}", relatives.len());
    println!("from primary     : {}", totals.from_primary);
    println!("from secondary   : {}", totals.from_secondary);
    println!("duplicates       : {}", totals.duplicates);
    println!("failed           : {}", totals.failed);
    if dry_run {
        println!("\nDry run: nothing was written.");
    }

    if totals.failed > 0 {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[derive(Default)]
struct Totals {
    from_primary: u64,
    from_secondary: u64,
    duplicates: u64,
    failed: u64,
}

impl Totals {
    fn add(&mut self, s: &Stats) {
        self.from_primary += s.from_primary;
        self.from_secondary += s.from_secondary;
        self.duplicates += s.duplicates;
    }
}

#[derive(Default)]
struct Stats {
    from_primary: u64,
    from_secondary: u64,
    duplicates: u64,
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "oqcap")
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_path_buf());
        }
    }
}

fn read_records(path: &Path) -> std::io::Result<Vec<Record>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)?;
    let (records, torn) = decode_all(&bytes).map_err(std::io::Error::other)?;
    if torn > 0 {
        eprintln!(
            "oq-merge: {} ends with {torn} torn bytes; merging what decodes",
            path.display()
        );
    }
    Ok(records)
}

fn merge_one(
    primary_root: &Path,
    secondary_root: &Path,
    out_root: &Path,
    rel: &Path,
    dry_run: bool,
) -> std::io::Result<Stats> {
    let primary = read_records(&primary_root.join(rel))?;
    let secondary = read_records(&secondary_root.join(rel))?;

    let mut seen: HashSet<(i64, &[u8])> = HashSet::with_capacity(primary.len());
    for r in &primary {
        if r.kind == Kind::Payload {
            seen.insert((r.exch_ts, r.payload.as_slice()));
        }
    }
    let last_local = primary.last().map_or(i64::MIN, |r| r.local_ts);

    let mut stats = Stats {
        from_primary: primary.len() as u64,
        ..Stats::default()
    };
    let mut merged: Vec<&Record> = primary.iter().collect();

    for r in &secondary {
        let keep = match r.kind {
            Kind::Payload => {
                if seen.contains(&(r.exch_ts, r.payload.as_slice())) {
                    stats.duplicates += 1;
                    false
                } else {
                    true
                }
            }
            // A control record from the secondary belongs in the merged
            // file only where the secondary's data does. Its
            // session_start sits in the overlap, describing records that
            // were dropped as duplicates; carrying it into the middle of
            // the primary's timeline would document a handover that the
            // merged file does not contain.
            Kind::Control => r.local_ts > last_local,
        };
        if keep {
            stats.from_secondary += 1;
            merged.push(r);
        }
    }

    if dry_run {
        return Ok(stats);
    }

    let Some((stream, rotation)) = describe(rel) else {
        return Err(std::io::Error::other(format!(
            "cannot tell which stream {} belongs to",
            rel.display()
        )));
    };
    // The window comes from the records, not from the filename. It is a
    // property of the data, and deriving it here also catches a file
    // that ended up under the wrong path.
    let Some(first) = merged.first() else {
        return Ok(stats);
    };
    let window = Window::from_nanos(first.day_ts(), rotation);

    let mut bytes = Vec::new();
    let mut builder = ManifestBuilder::new();
    for r in &merged {
        r.encode(&mut bytes);
        builder.observe(r);
    }

    let out_path = out_root.join(rel);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out_path, &bytes)?;

    let software = Software::new(
        concat!("oq-merge ", env!("CARGO_PKG_VERSION")),
        option_env!("OQ_BUILD_COMMIT").unwrap_or("unknown"),
    );
    let manifest = builder.build(&stream, window, &software, &bytes);
    std::fs::write(
        out_root.join(stream_manifest_rel(rel)),
        manifest.to_json(),
    )?;

    Ok(stats)
}

/// Recover the stream identity and rotation from an archive-relative path.
///
/// The layout is `<venue>/<symbol>/<stream>/<window>`, where the window
/// is `YYYY-MM-DD.oqcap` when rotating daily and `YYYY-MM-DD/HH.oqcap`
/// when rotating hourly — so the depth of the path says which.
fn describe(rel: &Path) -> Option<(StreamId, Rotation)> {
    let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
    parts.last()?.strip_suffix(".oqcap")?;
    let rotation = match parts.len() {
        4 => Rotation::Daily,
        5 => Rotation::Hourly,
        _ => return None,
    };
    Some((StreamId::new(parts[0], parts[1], parts[2]), rotation))
}

fn stream_manifest_rel(rel: &Path) -> PathBuf {
    let mut out = rel.to_path_buf();
    let stem = rel
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".oqcap"))
        .unwrap_or("window");
    out.set_file_name(format!("{stem}.manifest.json"));
    out
}
