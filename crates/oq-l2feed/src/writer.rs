//! Writing capture files.
//!
//! One file per UTC day, rotated on the exchange timestamp rather than a
//! local timer, so a file holds exactly its own day even if the capture
//! host's clock drifts or the process restarts across midnight.
//!
//! The writer never compresses and never deletes. Sealing a completed
//! day — compressing it, verifying it at the archive, and only then
//! reclaiming local space — is a separate step on purpose: capture is
//! the part that cannot be redone, so it does the least work it can.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::day::{Rotation, Window};
use crate::frame::Record;
use crate::manifest::{Manifest, ManifestBuilder, control};
use crate::stream::{Software, StreamId};

/// A day that finished and is ready to be sealed.
#[derive(Debug, Clone)]
pub struct SealedDay {
    /// The window it covers.
    pub window: Window,
    /// Path of the raw file.
    pub path: PathBuf,
    /// Manifest describing it.
    pub manifest: Manifest,
    /// Path the manifest was written to.
    pub manifest_path: PathBuf,
}

/// Appends framed records, rotating by UTC day.
#[derive(Debug)]
pub struct CaptureWriter {
    root: PathBuf,
    stream: StreamId,
    software: Software,
    open: Option<OpenDay>,
    /// The day that just rolled over, kept open for late arrivals.
    previous: Option<OpenDay>,
    /// How far into a new window late records for the previous one are
    /// still accepted.
    grace_ns: i64,
    /// How often a new file is started.
    rotation: Rotation,
    /// How this venue divides time into windows. `None` uses the clock,
    /// which is right for a market that never closes.
    window_of: Option<fn(i64, Rotation) -> Window>,
}

/// Default grace period after a day boundary: one minute, which covers
/// clock skew between the venue and the host plus a reconnect backlog,
/// and is far short of anything that would blur two days together.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct OpenDay {
    window: Window,
    path: PathBuf,
    file: BufWriter<File>,
    builder: ManifestBuilder,
    /// Records that arrived after the day had already rolled over.
    late_records: u64,
    /// `local_ts` of the last record already in the file when this
    /// window was opened, for a window that was resumed rather than
    /// started. `None` for a fresh file.
    resumed_after: Option<i64>,
}

impl CaptureWriter {
    /// Open a writer rooted at `root`.
    ///
    /// # Errors
    ///
    /// Propagates directory creation failures.
    pub fn new(root: impl Into<PathBuf>, stream: StreamId, software: Software) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(stream.directory(&root))?;
        Ok(Self {
            root,
            stream,
            software,
            open: None,
            previous: None,
            grace_ns: i64::try_from(DEFAULT_GRACE.as_nanos()).unwrap_or(i64::MAX),
            rotation: Rotation::Daily,
            window_of: None,
        })
    }

    /// Set how often a new file is started.
    ///
    /// Daily is the archival default. Hourly is for hosts that cannot
    /// hold two days of raw capture, since the open file cannot be
    /// compressed and the local peak is therefore about two rotation
    /// periods.
    #[must_use]
    pub fn with_rotation(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }

    /// Divide time the way `f` does rather than by the clock.
    ///
    /// For markets with sessions, where a trading day and a UTC day are
    /// not the same thing.
    #[must_use]
    pub fn with_windowing(mut self, f: fn(i64, Rotation) -> Window) -> Self {
        self.window_of = Some(f);
        self
    }

    /// Set how long the previous window stays open for late records.
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace_ns = i64::try_from(grace.as_nanos()).unwrap_or(i64::MAX);
        self
    }

    /// Append a record, rotating first if it belongs to a new day.
    ///
    /// Returns the sealed previous day when a rotation happened.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from rotation and appending.
    pub fn append(&mut self, record: &Record) -> io::Result<Option<SealedDay>> {
        let window = self.window_of.map_or_else(
            || Window::from_nanos(record.day_ts(), self.rotation),
            |f| f(record.day_ts(), self.rotation),
        );

        // A record for the day that just rolled over is normal, not an
        // error. Exchange timestamps and the host clock cross midnight
        // at slightly different moments, and a reconnect can deliver a
        // backlog stamped just before the boundary. The previous day
        // therefore stays open for a grace period.
        //
        // The alternative — refusing the record — killed the capture
        // process, since a write failure is fatal by design. Losing a
        // whole stream at midnight to preserve tidiness at a boundary is
        // the wrong trade: a late record in the right file costs
        // nothing, and a dead capture costs the rest of the day.
        if let Some(previous) = &mut self.previous
            && previous.window == window
        {
            let mut buffer = Vec::with_capacity(record.encoded_len());
            record.encode(&mut buffer);
            previous.file.write_all(&buffer)?;
            previous.builder.observe(record);
            previous.late_records += 1;
            return Ok(None);
        }

        let mut sealed = None;
        match &self.open {
            Some(open) if open.window == window => {}
            Some(open) if window < open.window => {
                // Older than even the grace window: the archive's
                // meaning depends on a file holding its own day, and
                // this record cannot be placed without breaking that.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "record for {} arrived after {} was already open, beyond the grace window",
                        window, open.window
                    ),
                ));
            }
            Some(_) => {
                // Rotating: seal whatever was already waiting, then keep
                // the outgoing day open for late arrivals.
                sealed = self.seal_previous()?;
                self.previous = self.open.take();
            }
            None => {}
        }

        if self.open.is_none() {
            self.open_window(window)?;
        }

        let open = self.open.as_mut().expect("just opened");
        let mut buffer = Vec::with_capacity(record.encoded_len());
        record.encode(&mut buffer);
        open.file.write_all(&buffer)?;
        open.builder.observe(record);

        // Once the new day is far enough along, nothing more can
        // legitimately belong to the old one.
        if self.previous.is_some() {
            let window_start = window.start_nanos();
            if record.day_ts().saturating_sub(window_start) > self.grace_ns {
                sealed = self.seal_previous()?;
            }
        }

        Ok(sealed)
    }

    /// Record a feed gap. The marker is written into the stream *and*
    /// counted in the manifest, so a reader can tell "nothing happened"
    /// from "we were not listening" without reparsing the file.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn append_gap(
        &mut self,
        local_ts: i64,
        reason: &str,
        last_seq: Option<u64>,
        outage_ns: i64,
    ) -> io::Result<Option<SealedDay>> {
        let sealed = self.append(&Record::control(
            local_ts,
            control::gap(reason, last_seq, outage_ns),
        ))?;
        if let Some(open) = self.open.as_mut() {
            open.builder.observe_gap(outage_ns);
        }
        self.flush()?;
        Ok(sealed)
    }

    /// Record a clock offset estimate.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn append_clock_offset(
        &mut self,
        local_ts: i64,
        offset_ns: i64,
        dispersion_ns: i64,
    ) -> io::Result<Option<SealedDay>> {
        let sealed = self.append(&Record::control(
            local_ts,
            control::clock_offset(offset_ns, dispersion_ns),
        ))?;
        if let Some(open) = self.open.as_mut() {
            open.builder.observe_clock_offset(offset_ns);
        }
        self.flush()?;
        Ok(sealed)
    }

    /// Record the start of a capture session.
    ///
    /// Flushed on the way out, as every control record is. They are
    /// rare enough that the write costs nothing, and the alternative
    /// was measured in production: the capture loop flushes when a
    /// message arrives, so on a stream that receives nothing the
    /// session marker sat in a one-megabyte buffer indefinitely. The
    /// file on disk then said the stream had not started, which is
    /// exactly what an operator checking on a silent stream must not be
    /// told -- and a crash would have lost the only record that it had.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn append_session_start(&mut self, local_ts: i64) -> io::Result<Option<SealedDay>> {
        let payload = control::session_start(
            &self.software.version,
            &self.stream.venue,
            &self.stream.symbol,
            &self.stream.stream,
        );
        let mut sealed = self.append(&Record::control(local_ts, payload))?;

        // Starting a session in a window that already held records means
        // this process replaced one that stopped, and nothing was
        // listening in between. That is a gap, and it is written into
        // the stream as one rather than only counted in the manifest.
        //
        // The distinction matters. A replay tool reads the file, not the
        // manifest beside it; when the seam existed only in the manifest
        // an order-book check reported "messages were lost silently"
        // even though the loss was known and recorded. The stream has to
        // be able to describe itself, or every reader needs a second
        // source to interpret the first.
        let resumed = self.open.as_mut().and_then(|o| o.resumed_after.take());
        if let Some(previous) = resumed {
            let outage = local_ts.saturating_sub(previous);
            if outage > 0
                && let Some(s) = self.append_gap(local_ts, "capture restarted", None, outage)?
            {
                sealed = Some(s);
            }
        }
        self.flush()?;
        Ok(sealed)
    }

    /// Flush buffered bytes to the operating system.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(open) = self.open.as_mut() {
            open.file.flush()?;
        }
        Ok(())
    }

    /// Flush and ask the operating system to persist the file.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(open) = self.open.as_mut() {
            open.file.flush()?;
            open.file.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Close the current day and write its manifest.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from flushing, reading back, or writing
    /// the manifest.
    pub fn seal(&mut self) -> io::Result<SealedDay> {
        // Any day still waiting for late records is finished too: the
        // caller is closing up, so nothing more can arrive.
        self.seal_previous()?;

        let open = self
            .open
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no day is open"))?;
        self.seal_day(open)
    }

    /// Seal the day kept open for late arrivals, if there is one.
    ///
    /// # Errors
    ///
    /// As [`CaptureWriter::seal`].
    pub fn seal_previous(&mut self) -> io::Result<Option<SealedDay>> {
        match self.previous.take() {
            Some(day) => Ok(Some(self.seal_day(day)?)),
            None => Ok(None),
        }
    }

    fn seal_day(&self, mut open: OpenDay) -> io::Result<SealedDay> {
        open.file.flush()?;
        open.file.get_ref().sync_all()?;
        drop(open.file);

        // Hash what is actually on disk rather than what was intended:
        // the manifest's job is to describe the artifact.
        let raw = fs::read(&open.path)?;
        let manifest = open
            .builder
            .build(&self.stream, open.window, &self.software, &raw);

        let manifest_path = self.stream.manifest_for(&self.root, open.window);
        fs::write(&manifest_path, manifest.to_json())?;

        Ok(SealedDay {
            window: open.window,
            path: open.path,
            manifest,
            manifest_path,
        })
    }

    /// The window currently open, if any.
    #[must_use]
    pub fn current_window(&self) -> Option<Window> {
        self.open.as_ref().map(|o| o.window)
    }

    fn open_window(&mut self, window: Window) -> io::Result<()> {
        let path = self.stream.file_for(&self.root, window);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Reopening a window that already holds records means a restart
        // landed inside it, which with hourly rotation is what every
        // restart does. Counting only from here would seal a manifest
        // describing part of its own file -- a manifest that undercounts
        // is worse than none, because nothing downstream can tell it is
        // wrong, and the whole point of the manifest is to say whether
        // an hour is complete.
        //
        // The accounting is rebuilt from the bytes rather than from the
        // previous manifest: the file is the only thing that cannot be
        // stale, and decoding also tolerates a torn tail left by a hard
        // kill.
        let mut builder = ManifestBuilder::new();
        let existing = fs::read(&path).unwrap_or_default();
        if !existing.is_empty() {
            let (records, _torn) = crate::frame::decode_all(&existing)
                .map_err(|e| io::Error::other(format!("cannot reopen {}: {e}", path.display())))?;
            for record in &records {
                builder.observe(record);
            }
        }

        // Drop the previous manifest now that it no longer describes the
        // file. If this process is killed before sealing, the archive
        // should see an honest orphan rather than a manifest that lies.
        let manifest_path = self.stream.manifest_for(&self.root, window);
        if manifest_path.exists() {
            fs::remove_file(&manifest_path)?;
        }

        // Append rather than truncate: a restart continues the day, and
        // the seam is visible in the data through a session_start record
        // rather than inferred from file timestamps.
        let resumed_after = builder.local_last();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.open = Some(OpenDay {
            window,
            path,
            file: BufWriter::with_capacity(1 << 20, file),
            builder,
            late_records: 0,
            resumed_after,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::day::UtcDay;
    use crate::frame::decode_all;

    const DAY_NS: i64 = 86_400_000_000_000;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oq-l2feed-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn writer(name: &str) -> (CaptureWriter, PathBuf) {
        let root = temp_root(name);
        let w = CaptureWriter::new(
            &root,
            StreamId::new("venue", "SYM", "depth"),
            Software::new("test 0.1", "commit"),
        )
        .expect("open");
        (w, root)
    }

    #[test]
    fn a_session_marker_reaches_disk_without_waiting_for_data() {
        // Measured in production: the capture loop flushes when a
        // message arrives, so a stream that received none left its
        // session marker in the buffer for as long as the silence
        // lasted. An operator checking whether a quiet stream had
        // restarted read a file that did not mention it.
        let (mut w, root) = writer("silent-session");
        let base = 20_000 * DAY_NS;
        w.append_session_start(base + 1).expect("session start");

        let path = StreamId::new("venue", "SYM", "depth")
            .file_for(&root, Window::from_nanos(base + 1, Rotation::Daily));
        let bytes = fs::read(&path).expect("the file exists before any payload arrives");
        let (records, remainder) = decode_all(&bytes).expect("decode");
        assert_eq!(remainder, 0);
        assert_eq!(records.len(), 1, "the marker is on disk, not in the buffer");
        assert_eq!(records[0].kind, crate::frame::Kind::Control);

        // And a gap declared while nothing else is being written is
        // visible for the same reason.
        w.append_gap(base + 2, "connection lost", None, 1_000)
            .expect("gap");
        let (records, _) = decode_all(&fs::read(&path).expect("read")).expect("decode");
        assert_eq!(records.len(), 2);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn writes_records_that_read_back_identically() {
        let (mut w, root) = writer("roundtrip");
        let base = 20_000 * DAY_NS;
        let records = vec![
            Record::payload(base + 1, base + 1, b"{\"a\":1}".to_vec()),
            Record::payload(base + 2, base + 2, b"{\"a\":2}".to_vec()),
        ];
        for r in &records {
            assert!(w.append(r).expect("append").is_none());
        }
        let sealed = w.seal().expect("seal");

        let bytes = fs::read(&sealed.path).expect("read");
        let (decoded, remainder) = decode_all(&bytes).expect("decode");
        assert_eq!(decoded, records);
        assert_eq!(remainder, 0);
        assert_eq!(sealed.manifest.records, 2);
        assert_eq!(sealed.manifest.sha256_raw, oq_hash::sha256_hex(&bytes));
        assert!(sealed.manifest_path.exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rotates_on_the_exchange_day_not_the_local_clock() {
        let (mut w, root) = writer("rotate");
        let day = 20_000i64;
        // Local timestamps stay in the old day; exchange timestamps cross
        // midnight. The exchange clock must decide.
        let first = Record::payload(day * DAY_NS + 10, day * DAY_NS + 10, b"a".to_vec());
        let second = Record::payload(day * DAY_NS + 20, (day + 1) * DAY_NS + 1, b"b".to_vec());

        assert!(w.append(&first).expect("append").is_none());
        assert!(
            w.append(&second).expect("append").is_none(),
            "rotation switches days but defers sealing, since late records may still arrive"
        );
        assert_eq!(w.current_window().map(|x| x.day), Some(UtcDay(day + 1)));

        let sealed = w
            .seal_previous()
            .expect("seal")
            .expect("the outgoing day was still open");
        assert_eq!(sealed.window.day, UtcDay(day));
        assert_eq!(
            sealed.manifest.records, 1,
            "only the first record belongs to that day"
        );

        let files: Vec<_> = fs::read_dir(StreamId::new("venue", "SYM", "depth").directory(&root))
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        assert!(files.contains(&format!("{}.oqcap", UtcDay(day))));
        assert!(files.contains(&format!("{}.oqcap", UtcDay(day + 1))));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_late_record_at_the_boundary_lands_in_its_own_day_rather_than_killing_capture() {
        // Midnight, as it actually happens: the exchange clock and the
        // host clock cross the boundary moments apart, and a reconnect
        // delivers a backlog stamped just before it. Refusing that
        // record used to fail the write, and a failed write ends the
        // capture — losing the rest of the day to keep a boundary tidy.
        let (mut w, root) = writer("boundary");
        let day = 20_000i64;
        let midnight = (day + 1) * DAY_NS;

        w.append(&Record::payload(0, midnight - 1_000, b"before".to_vec()))
            .expect("append");
        // First record of the new day: rotates, keeps the old day open.
        assert!(
            w.append(&Record::payload(0, midnight + 1_000, b"after".to_vec()))
                .expect("append")
                .is_none(),
            "the outgoing day is not sealed while late records may still arrive"
        );
        // The backlog, stamped before midnight, must be accepted.
        w.append(&Record::payload(0, midnight - 500, b"late".to_vec()))
            .expect("a late record must not fail the write");

        let sealed = w.seal().expect("seal");
        assert_eq!(sealed.window.day, UtcDay(day + 1));

        // Both pre-midnight records are in the old day's file, and only
        // those.
        let old = fs::read(StreamId::new("venue", "SYM", "depth").file_for(
            &root,
            Window {
                day: UtcDay(day),
                hour: None,
            },
        ))
        .expect("read");
        let (records, _) = decode_all(&old).expect("decode");
        let payloads: Vec<_> = records.iter().map(|r| r.payload.clone()).collect();
        assert_eq!(payloads, vec![b"before".to_vec(), b"late".to_vec()]);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_grace_window_closes_once_the_new_day_is_under_way() {
        let (mut w, root) = writer("graceclose");
        let day = 20_000i64;
        let midnight = (day + 1) * DAY_NS;

        w.append(&Record::payload(0, midnight - 1_000, b"before".to_vec()))
            .expect("append");
        w.append(&Record::payload(0, midnight + 1_000, b"after".to_vec()))
            .expect("append");
        // Well past the grace window: the old day is sealed now.
        let sealed = w
            .append(&Record::payload(
                0,
                midnight + 120 * 1_000_000_000,
                b"later".to_vec(),
            ))
            .expect("append")
            .expect("the previous day is sealed once grace expires");
        assert_eq!(sealed.window.day, UtcDay(day));

        // And a record older than that is refused, because it can no
        // longer be placed in a file that still claims its own day.
        let err = w
            .append(&Record::payload(0, midnight - 500, b"too late".to_vec()))
            .expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn gaps_are_written_and_counted() {
        let (mut w, root) = writer("gaps");
        let base = 20_000 * DAY_NS;
        w.append(&Record::payload(base, base, b"a".to_vec()))
            .expect("append");
        w.append_gap(base + 5, "disconnect", Some(42), 3_000)
            .expect("gap");
        let sealed = w.seal().expect("seal");

        assert_eq!(sealed.manifest.gaps, 1);
        assert_eq!(sealed.manifest.gap_ns_total, 3_000);

        let bytes = fs::read(&sealed.path).expect("read");
        let (records, _) = decode_all(&bytes).expect("decode");
        assert!(
            crate::manifest::is_gap(&records[1]),
            "the marker is in the stream too"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_restart_appends_to_the_same_day_rather_than_truncating() {
        let root = temp_root("restart");
        let stream = StreamId::new("venue", "SYM", "depth");
        let base = 20_000 * DAY_NS;

        {
            let mut w =
                CaptureWriter::new(&root, stream.clone(), Software::new("v", "c")).expect("open");
            w.append(&Record::payload(base, base, b"first".to_vec()))
                .expect("append");
            w.seal().expect("seal");
        }
        {
            let mut w =
                CaptureWriter::new(&root, stream.clone(), Software::new("v", "c")).expect("reopen");
            w.append_session_start(base + 1).expect("session start");
            w.append(&Record::payload(base + 2, base + 2, b"second".to_vec()))
                .expect("append");
            w.seal().expect("seal");
        }

        let bytes = fs::read(stream.file_for(
            &root,
            Window {
                day: UtcDay(20_000),
                hour: None,
            },
        ))
        .expect("read");
        let (records, _) = decode_all(&bytes).expect("decode");
        // first, session_start, the gap the restart left, second.
        assert_eq!(records.len(), 4, "first record survived the restart");
        assert_eq!(records[0].payload, b"first");
        assert!(
            core::str::from_utf8(&records[1].payload)
                .expect("utf8")
                .contains("session_start"),
            "the seam is visible in the data"
        );
        assert!(
            crate::manifest::is_gap(&records[2]),
            "and the silence across the seam is marked as a gap, so a \
             reader of the stream alone can see that something is missing"
        );
        assert_eq!(records[3].payload, b"second");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn sealing_without_an_open_day_is_an_error_not_an_empty_manifest() {
        let (mut w, root) = writer("noday");
        assert_eq!(
            w.seal().expect_err("must fail").kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(root).ok();
    }
}
