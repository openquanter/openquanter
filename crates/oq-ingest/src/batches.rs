//! Walking an archive one hour at a time.
//!
//! The batch exists because memory does. A day of one instrument's depth
//! is millions of records and the parsed form is larger than the bytes
//! on disk; loading a whole day at once was measured on the capture host
//! as a process the kernel killed, after it had reported 2,114,759 depth
//! records, on 1 GiB of RAM.
//!
//! The archive is already written one file per hour, so an hour is the
//! batch the data offers. Whoever consumes these carries their own
//! aggregator or matcher across the batches: the book, the cumulative
//! volume and the open window are state that spans hours, and per-hour
//! work that started fresh would report an unknown quote at the top of
//! every hour and restart the volume counter twenty-four times a day.
//!
//! This lives here rather than in a binary because two of them need it,
//! and a second copy is where the two would come to disagree about what
//! an archive contains.

use std::path::{Path, PathBuf};

use oq_l2feed::frame::{Record, decode_all};

/// The streams a run reads. Depth first, so a book is seeded before the
/// trades that match against it are folded.
pub const STREAMS: [&str; 2] = ["depth", "trade"];

/// Files holding one stream for one day.
///
/// Daily rotation writes `<stream>/<day>.oqcap`; hourly writes
/// `<stream>/<day>/HH.oqcap`. Both are read, so a day captured across a
/// rotation change still converts as one day. Either may carry a `.zst`
/// suffix -- everything that has been through the archive step does --
/// so the test is `archive::is_capture` rather than an extension.
#[must_use]
pub fn files_for(archive: &Path, stream: &str, day: &str) -> Vec<PathBuf> {
    let dir = archive.join(stream);
    let mut out = Vec::new();

    for daily in [
        dir.join(format!("{day}.oqcap")),
        dir.join(format!("{day}.oqcap.zst")),
    ] {
        if daily.is_file() {
            out.push(daily);
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir.join(day)) {
        let mut hourly: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| oq_l2feed::archive::is_capture(p))
            .collect();
        hourly.sort();
        out.extend(hourly);
    }
    out
}

/// The hours an archive holds for one day, in order.
///
/// Named by the file stem: an hourly rotation gives `00`..`23` and a
/// daily one gives the date itself, which sorts as the single batch it
/// is.
#[must_use]
pub fn hours(archive: &Path, day: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for stream in STREAMS {
        for path in files_for(archive, stream, day) {
            if let Some(stem) = oq_l2feed::archive::stem(&path)
                && !out.contains(&stem)
            {
                out.push(stem);
            }
        }
    }
    out.sort();
    out
}

/// One stream's records for one hour, decoded.
pub struct Batch {
    pub stream: &'static str,
    pub records: Vec<Record>,
    /// Bytes at the end of the last file that did not form a record.
    ///
    /// The normal result of a crash during capture, and not damage: it
    /// means "stop reading here". Reported rather than silently dropped,
    /// because a torn tail in the *middle* of an archive would be, and
    /// the count is the only thing that distinguishes them.
    pub torn: usize,
}

/// Load one hour of every stream.
///
/// # Errors
/// The path and the reason, for a file that cannot be read or whose
/// records do not decode. A damaged archive stops the run: continuing
/// past it would produce a book with a hole in it, which reconstructs
/// into plausible prices that are wrong.
pub fn load_hour(archive: &Path, day: &str, hour: &str) -> Result<Vec<Batch>, String> {
    let mut out = Vec::new();
    for stream in STREAMS {
        let mut bytes = Vec::new();
        for path in files_for(archive, stream, day) {
            if oq_l2feed::archive::stem(&path).as_deref() != Some(hour) {
                continue;
            }
            match oq_l2feed::archive::read(&path) {
                Ok(b) => bytes.extend_from_slice(&b),
                Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            }
        }
        if bytes.is_empty() {
            continue;
        }
        match decode_all(&bytes) {
            Ok((records, torn)) => out.push(Batch {
                stream,
                records,
                torn,
            }),
            Err(e) => return Err(format!("{stream} {hour} is damaged: {e}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive that is not there is empty, not an error.
    ///
    /// A caller asking for a day that was never captured gets nothing to
    /// convert and says so; making it a failure would put the same shape
    /// on "you asked for the wrong day" and "this archive is damaged".
    #[test]
    fn a_missing_archive_holds_no_hours() {
        let missing = Path::new("/nonexistent-archive-root/binance-perp/BTCUSDT");
        assert!(hours(missing, "2026-08-19").is_empty());
        assert!(files_for(missing, "depth", "2026-08-19").is_empty());
        assert!(
            load_hour(missing, "2026-08-19", "12")
                .expect("no files")
                .is_empty()
        );
    }

    /// Depth is loaded before trades, because a book has to exist
    /// before the trades matched against it are folded in.
    #[test]
    fn depth_is_read_before_trades() {
        assert_eq!(STREAMS, ["depth", "trade"]);
    }
}
