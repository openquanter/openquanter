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
}

#[derive(Debug)]
struct OpenDay {
    day: UtcDay,
    path: PathBuf,
    file: BufWriter<File>,
    builder: ManifestBuilder,
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
        })
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

        let mut sealed = None;
        match &self.open {
            Some(open) if open.day == day => {}
            Some(open) if day < open.day => {
                // Out-of-order across a day boundary. Writing it into the
                // current file would put a record in the wrong day and
                // corrupt the archive's meaning; dropping it would lose
                // data. Refusing is the only honest option.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "record for {} arrived after {} was already open",
                        day, open.day
                    ),
                ));
            }
            Some(_) => sealed = Some(self.seal()?),
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
        let mut open = self
            .open
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no day is open"))?;

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
        let sealed = w.append(&second).expect("append").expect("rotation");

        assert_eq!(sealed.day, UtcDay(day));
        assert_eq!(
            sealed.manifest.records, 1,
            "only the first record belongs to that day"
        );
        assert_eq!(w.current_day(), Some(UtcDay(day + 1)));

        let files: Vec<_> = fs::read_dir(StreamId::new("venue", "SYM", "depth").directory(&root))
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        assert!(files.contains(&format!("{}.oqcap", UtcDay(day))));
        assert!(files.contains(&format!("{}.oqcap", UtcDay(day + 1))));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refuses_a_record_belonging_to_an_already_closed_day() {
        let (mut w, root) = writer("backwards");
        let day = 20_000i64;
        w.append(&Record::payload(0, (day + 1) * DAY_NS, b"a".to_vec()))
            .expect("append");
        let err = w
            .append(&Record::payload(0, day * DAY_NS, b"late".to_vec()))
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
