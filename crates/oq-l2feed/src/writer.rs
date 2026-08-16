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

use crate::day::UtcDay;
use crate::frame::Record;
use crate::manifest::{Manifest, ManifestBuilder, control};
use crate::stream::{Software, StreamId};

/// A day that finished and is ready to be sealed.
#[derive(Debug, Clone)]
pub struct SealedDay {
    /// The day.
    pub day: UtcDay,
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
    /// How far into a new day late records for the previous one are
    /// still accepted.
    grace_ns: i64,
}

/// Default grace period after a day boundary: one minute, which covers
/// clock skew between the venue and the host plus a reconnect backlog,
/// and is far short of anything that would blur two days together.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct OpenDay {
    day: UtcDay,
    path: PathBuf,
    file: BufWriter<File>,
    builder: ManifestBuilder,
    /// Records that arrived after the day had already rolled over.
    late_records: u64,
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
        })
    }

    /// Set how long the previous day stays open for late records.
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
        let day = UtcDay::from_nanos(record.day_ts());

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
            && previous.day == day
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
            Some(open) if open.day == day => {}
            Some(open) if day < open.day => {
                // Older than even the grace window: the archive's
                // meaning depends on a file holding its own day, and
                // this record cannot be placed without breaking that.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "record for {} arrived after {} was already open, beyond the grace window",
                        day, open.day
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
            self.open_day(day)?;
        }

        let open = self.open.as_mut().expect("just opened");
        let mut buffer = Vec::with_capacity(record.encoded_len());
        record.encode(&mut buffer);
        open.file.write_all(&buffer)?;
        open.builder.observe(record);

        // Once the new day is far enough along, nothing more can
        // legitimately belong to the old one.
        if self.previous.is_some() {
            let day_start = day.start_nanos();
            if record.day_ts().saturating_sub(day_start) > self.grace_ns {
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
        Ok(sealed)
    }

    /// Record the start of a capture session.
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
        self.append(&Record::control(local_ts, payload))
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
            .build(&self.stream, open.day, &self.software, &raw);

        let manifest_path = self.stream.manifest_for(&self.root, open.day);
        fs::write(&manifest_path, manifest.to_json())?;

        Ok(SealedDay {
            day: open.day,
            path: open.path,
            manifest,
            manifest_path,
        })
    }

    /// The day currently open, if any.
    #[must_use]
    pub fn current_day(&self) -> Option<UtcDay> {
        self.open.as_ref().map(|o| o.day)
    }

    fn open_day(&mut self, day: UtcDay) -> io::Result<()> {
        let path = self.stream.file_for(&self.root, day);
        // Append rather than truncate: a restart continues the day, and
        // the seam is visible in the data through a session_start record
        // rather than inferred from file timestamps.
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.open = Some(OpenDay {
            day,
            path,
            file: BufWriter::with_capacity(1 << 20, file),
            builder: ManifestBuilder::new(),
            late_records: 0,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(w.current_day(), Some(UtcDay(day + 1)));

        let sealed = w
            .seal_previous()
            .expect("seal")
            .expect("the outgoing day was still open");
        assert_eq!(sealed.day, UtcDay(day));
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
        assert_eq!(sealed.day, UtcDay(day + 1));

        // Both pre-midnight records are in the old day's file, and only
        // those.
        let old = fs::read(StreamId::new("venue", "SYM", "depth").file_for(&root, UtcDay(day)))
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
        assert_eq!(sealed.day, UtcDay(day));

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

        let bytes = fs::read(stream.file_for(&root, UtcDay(20_000))).expect("read");
        let (records, _) = decode_all(&bytes).expect("decode");
        assert_eq!(records.len(), 3, "first record survived the restart");
        assert_eq!(records[0].payload, b"first");
        assert!(
            core::str::from_utf8(&records[1].payload)
                .expect("utf8")
                .contains("session_start"),
            "the seam is visible in the data"
        );
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
