//! Reading and replaying a journal.
//!
//! The reader's contract is the interesting part: it distinguishes a
//! **torn tail** from **corruption**, and it never conflates them.
//!
//! - A truncated *final* record means the writer died mid-append. The
//!   replay stops cleanly at the last whole record and reports where.
//!   This is a normal outcome after a crash and must not look like data
//!   loss.
//! - A bad checksum or bad magic *before* the end means bytes that were
//!   once whole are no longer whole. That is data loss, it is reported
//!   as an error, and no caller should be able to mistake it for the
//!   ordinary case.
//!
//! Sequence contiguity is checked as records are read. A gap means
//! records that existed are missing, which cannot be recovered from by
//! reading further.

use crate::{Frame, FrameError, JournalError, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Why a replay stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayStop {
    /// Every byte in the file belonged to a whole record.
    Clean,
    /// The final record was incomplete; the writer died mid-append.
    ///
    /// `bytes` is how many trailing bytes were discarded, and
    /// `offset` where the last whole record ended — the offset a writer
    /// should resume from.
    TornTail { offset: u64, bytes: u64 },
}

/// The result of replaying a journal.
#[derive(Debug)]
pub struct Replay {
    /// Records in sequence order.
    pub frames: Vec<Frame>,
    /// Why reading stopped.
    pub stop: ReplayStop,
    /// The sequence number a writer would assign next.
    pub next_seq: u64,
}

impl Replay {
    /// Records whose sequence number is at or after `from`.
    ///
    /// Recovery reads a snapshot taken at sequence N and then applies
    /// everything from N onward, so this is the shape recovery needs.
    pub fn since(&self, from: u64) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(move |f| f.seq >= from)
    }
}

/// A journal opened for reading.
#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Reader {
    /// Read the whole journal into memory.
    ///
    /// Whole-file reads are appropriate at the sizes this crate targets
    /// (a session's events, not a year of market data) and remove a
    /// class of partial-read bugs from the replay path. Streaming
    /// replay for larger journals is a separate entry point rather than
    /// a mode of this one, so neither has to carry the other's caveats.
    ///
    /// # Errors
    /// I/O failures.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut bytes = Vec::new();
        match File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut bytes)?;
            }
            // An absent journal is an empty journal: a process starting
            // for the first time has nothing to replay, and that is not
            // an error condition it should have to special-case.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(JournalError::Io(e)),
        }
        Ok(Self { path, bytes })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Decode every whole record, stopping at a torn tail.
    ///
    /// # Errors
    /// [`JournalError::Corrupt`] for damage before the end of the file,
    /// [`JournalError::SequenceGap`] for missing records.
    pub fn replay(&self) -> Result<Replay> {
        let mut frames = Vec::new();
        let mut offset = 0usize;
        let mut expected_seq: Option<u64> = None;

        loop {
            if offset == self.bytes.len() {
                let next_seq = expected_seq.unwrap_or(0);
                return Ok(Replay {
                    frames,
                    stop: ReplayStop::Clean,
                    next_seq,
                });
            }
            match Frame::decode(&self.bytes[offset..]) {
                Ok((frame, used)) => {
                    if let Some(expected) = expected_seq
                        && frame.seq != expected
                    {
                        return Err(JournalError::SequenceGap {
                            expected,
                            found: frame.seq,
                        });
                    }
                    expected_seq = Some(frame.seq + 1);
                    frames.push(frame);
                    offset += used;
                }
                Err(FrameError::Incomplete { .. }) => {
                    // The only benign stop: the tail of the file is a
                    // record the writer never finished.
                    let discarded = (self.bytes.len() - offset) as u64;
                    let next_seq = expected_seq.unwrap_or(0);
                    return Ok(Replay {
                        frames,
                        stop: ReplayStop::TornTail {
                            offset: offset as u64,
                            bytes: discarded,
                        },
                        next_seq,
                    });
                }
                Err(cause) => {
                    return Err(JournalError::Corrupt {
                        at_offset: offset as u64,
                        cause,
                    });
                }
            }
        }
    }
}

/// Find the next sequence number and the offset of the last whole
/// record, without materializing the records.
///
/// Used by [`crate::Writer::open`] to resume at a clean boundary.
///
/// # Errors
/// I/O failures, or corruption before the end of the file.
pub(crate) fn scan_tail(path: &Path) -> Result<(u64, u64)> {
    let reader = Reader::open(path)?;
    let replay = reader.replay()?;
    let clean_len = match replay.stop {
        ReplayStop::Clean => reader.bytes.len() as u64,
        ReplayStop::TornTail { offset, .. } => offset,
    };
    Ok((replay.next_seq, clean_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::{SyncPolicy, Writer};

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oq-journal-r-{}-{}-{}.log",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }

    #[test]
    fn absent_journal_replays_as_empty() {
        let path = temp_path("absent");
        let replay = Reader::open(&path).expect("open").replay().expect("replay");
        assert!(replay.frames.is_empty());
        assert_eq!(replay.stop, ReplayStop::Clean);
        assert_eq!(replay.next_seq, 0);
    }

    #[test]
    fn round_trip_preserves_order_and_payloads() {
        let path = temp_path("roundtrip");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            for i in 0..64u16 {
                w.append(i, format!("event-{i}").as_bytes())
                    .expect("append");
            }
            w.sync().expect("sync");
        }
        let replay = Reader::open(&path).expect("open").replay().expect("replay");
        assert_eq!(replay.frames.len(), 64);
        assert_eq!(replay.stop, ReplayStop::Clean);
        assert_eq!(replay.next_seq, 64);
        for (i, f) in replay.frames.iter().enumerate() {
            assert_eq!(f.seq, i as u64);
            assert_eq!(f.payload, format!("event-{i}").into_bytes());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn torn_tail_stops_cleanly_and_reports_the_resume_offset() {
        let path = temp_path("torntail");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            w.append(1, b"one").expect("append");
            w.append(1, b"two").expect("append");
            w.sync().expect("sync");
        }
        let clean_len = std::fs::metadata(&path).expect("metadata").len();
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"\x4F\x51\x52\x4A\x01\x00\x00")
                .expect("partial");
        }

        let replay = Reader::open(&path).expect("open").replay().expect("replay");
        assert_eq!(replay.frames.len(), 2, "whole records survive the tear");
        match replay.stop {
            ReplayStop::TornTail { offset, bytes } => {
                assert_eq!(offset, clean_len);
                assert_eq!(bytes, 7);
            }
            ReplayStop::Clean => panic!("expected a torn tail"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corruption_in_the_middle_is_an_error_not_a_stop() {
        let path = temp_path("corrupt");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            w.append(1, b"first").expect("append");
            w.append(1, b"second").expect("append");
            w.append(1, b"third").expect("append");
            w.sync().expect("sync");
        }
        // Flip a byte inside the first record's payload.
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[crate::HEADER_LEN + 1] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");

        let err = Reader::open(&path)
            .expect("open")
            .replay()
            .expect_err("corruption must not be silently truncated");
        assert!(
            matches!(err, JournalError::Corrupt { at_offset: 0, .. }),
            "got {err:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn since_selects_the_recovery_range() {
        let path = temp_path("since");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            for i in 0..10 {
                w.append(0, &[i]).expect("append");
            }
            w.sync().expect("sync");
        }
        let replay = Reader::open(&path).expect("open").replay().expect("replay");
        let tail: Vec<_> = replay.since(7).map(|f| f.seq).collect();
        assert_eq!(tail, vec![7, 8, 9]);
        std::fs::remove_file(&path).ok();
    }
}
