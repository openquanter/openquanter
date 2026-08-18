//! Appending to the journal.
//!
//! The writer assigns sequence numbers and appends framed records. It
//! is deliberately the only place a sequence number is created: an
//! identifier that could be minted in two places is an identifier that
//! will eventually be minted twice.
//!
//! ## Durability is a policy, and the honest default is explicit
//!
//! Whether a record has reached durable storage when `append` returns
//! depends on [`SyncPolicy`]. Backtests use [`SyncPolicy::Never`] and
//! gain an order of magnitude of throughput; a live trading process
//! that must not lose an acknowledged order uses
//! [`SyncPolicy::EveryRecord`]. The policy is a required constructor
//! argument rather than a default, because a durability guarantee that
//! is acquired by accident is a durability guarantee that is lost by
//! accident.

use crate::{Frame, JournalError, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// When to force records to durable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Flush to the OS on every record, and fsync.
    ///
    /// The only policy under which an acknowledged event survives a
    /// machine power loss. Costs a device round trip per record.
    EveryRecord,
    /// Flush to the OS on every record; leave fsync to the OS.
    ///
    /// Survives a process crash but not a machine crash. The right
    /// choice for a replayable simulation whose inputs exist elsewhere.
    EveryRecordNoFsync,
    /// Buffer, and flush when the buffer fills or on drop.
    ///
    /// For backtests, where the journal's value is replay and audit
    /// rather than durability, and where the input can be re-fed.
    Never,
}

/// An append-only journal writer.
#[derive(Debug)]
pub struct Writer {
    path: PathBuf,
    /// Removed on drop. Its presence is what stops a second writer.
    lock: PathBuf,
    file: BufWriter<File>,
    policy: SyncPolicy,
    next_seq: u64,
    bytes_written: u64,
    scratch: Vec<u8>,
}

impl Writer {
    /// Open `path` for appending, continuing the sequence already in it.
    ///
    /// Continuing rather than restarting matters: a process that
    /// restarted its numbering would produce two different events with
    /// the same sequence number, and every artifact that refers to
    /// events by sequence — snapshots, parity reports, replay ranges —
    /// would become ambiguous.
    ///
    /// A torn final record is truncated away here, so the writer always
    /// starts from a clean record boundary.
    ///
    /// # Errors
    /// I/O failures, or corruption in the middle of the existing file.
    pub fn open(path: impl AsRef<Path>, policy: SyncPolicy) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Taken before anything is read, because the tail scan and the
        // truncation below both assume nobody else is writing.
        let lock = acquire(&path)?;

        let (next_seq, clean_len) = crate::reader::scan_tail(&path)?;

        // Drop a torn tail rather than appending after it: a reader that
        // stops at the tear would never see anything written past it.
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let actual_len = file.metadata()?.len();
        if actual_len > clean_len {
            let truncating = OpenOptions::new().write(true).open(&path)?;
            truncating.set_len(clean_len)?;
            truncating.sync_all()?;
        }

        Ok(Self {
            path,
            lock,
            file: BufWriter::with_capacity(1 << 16, file),
            policy,
            next_seq,
            bytes_written: clean_len,
            scratch: Vec::with_capacity(1024),
        })
    }

    /// The sequence number the next append will assign.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Bytes in the journal, counting only whole records.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.bytes_written
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes_written == 0
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, returning the sequence number it was assigned.
    ///
    /// # Errors
    /// I/O failures.
    pub fn append(&mut self, kind: u16, payload: &[u8]) -> Result<u64> {
        let seq = self.next_seq;
        self.scratch.clear();
        Frame::new(seq, kind, payload.to_vec()).encode_into(&mut self.scratch);
        self.file.write_all(&self.scratch)?;
        self.bytes_written += self.scratch.len() as u64;
        self.next_seq += 1;

        match self.policy {
            SyncPolicy::EveryRecord => {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
            }
            SyncPolicy::EveryRecordNoFsync => self.file.flush()?,
            SyncPolicy::Never => {}
        }
        Ok(seq)
    }

    /// Flush buffered records to the OS.
    ///
    /// # Errors
    /// I/O failures.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    /// Flush and fsync, whatever the policy.
    ///
    /// # Errors
    /// I/O failures.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // A buffered record that never reached the OS would be a record
        // the journal claims to have and does not. Errors cannot be
        // returned from drop; callers that need the guarantee call
        // `sync` explicitly, which is why `sync` is public.
        let _ = self.file.flush();
        // Best effort: a process killed outright leaves this behind, and
        // the next start refuses until someone looks. That is the safe
        // direction for a journal — a stale lock costs a human a minute,
        // a shared journal costs the record.
        let _ = std::fs::remove_file(&self.lock);
    }
}

/// Sequence numbers must be contiguous, and this is where that is
/// enforced on the write path.
impl Writer {
    /// Append with an expected sequence number, refusing a mismatch.
    ///
    /// For callers that derive a sequence number elsewhere — a replica
    /// applying a leader's stream, for instance — and must not silently
    /// renumber it.
    ///
    /// # Errors
    /// [`JournalError::SequenceGap`] if `expected` is not the next
    /// sequence number, plus I/O failures.
    pub fn append_at(&mut self, expected: u64, kind: u16, payload: &[u8]) -> Result<u64> {
        if expected != self.next_seq {
            return Err(JournalError::SequenceGap {
                expected: self.next_seq,
                found: expected,
            });
        }
        self.append(kind, payload)
    }
}

/// Claim exclusive use of a journal, or say who has it.
///
/// `create_new` is the whole mechanism: the file system decides, once,
/// which caller creates the file. No check-then-act, so no window
/// between deciding it is free and taking it — which is the failure a
/// `pgrep` in a start script cannot avoid, and the reason this lives
/// here rather than in one.
fn acquire(journal: &Path) -> Result<PathBuf> {
    let lock = journal.with_extension("lock");
    match OpenOptions::new().write(true).create_new(true).open(&lock) {
        Ok(mut f) => {
            // Written for a human reading it after a crash. The pid is
            // the useful part; the rest says which journal and when, so
            // a stale file can be recognised as stale.
            let _ = writeln!(
                f,
                "pid {} opened {} at {}",
                std::process::id(),
                journal.display(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let _ = f.sync_all();
            Ok(lock)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let held_by = std::fs::read_to_string(&lock)
                .unwrap_or_default()
                .trim()
                .to_string();
            Err(JournalError::AlreadyOpen {
                lock,
                held_by: if held_by.is_empty() {
                    "nothing about itself".to_string()
                } else {
                    held_by
                },
            })
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {

    /// The failure this exists to prevent, reproduced.
    ///
    /// Two `oqp-live` processes were once started ninety-two seconds
    /// apart by a command that ran twice, and both appended here. The
    /// result is not repairable by a reader: sequence numbers stay
    /// contiguous, every frame decodes, and the history describes a
    /// session that never took place.
    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_it() {
        let dir = std::env::temp_dir().join(format!("oqj-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("held.oqj");

        let first = Writer::open(&path, SyncPolicy::Never).expect("first opens");
        match Writer::open(&path, SyncPolicy::Never) {
            Err(JournalError::AlreadyOpen { held_by, .. }) => {
                // The message has to name the holder, or the operator is
                // told only that something is wrong.
                assert!(
                    held_by.contains(&std::process::id().to_string()),
                    "the holder should be named: {held_by}"
                );
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("two writers opened the same journal"),
        }

        // Dropping the first releases it, so a restart after a clean
        // shutdown is not blocked by yesterday's lock.
        drop(first);
        let _second = Writer::open(&path, SyncPolicy::Never).expect("reopens after drop");

        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use crate::reader::Reader;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oq-journal-{}-{}-{}.log",
            name,
            std::process::id(),
            // A per-test counter keeps parallel tests from colliding
            // without reading the clock, which the workspace forbids in
            // library code and which would be gratuitous here.
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn appends_are_numbered_from_zero() {
        let path = temp_path("numbered");
        let mut w = Writer::open(&path, SyncPolicy::Never).expect("open");
        assert_eq!(w.append(1, b"a").expect("append"), 0);
        assert_eq!(w.append(1, b"b").expect("append"), 1);
        assert_eq!(w.next_seq(), 2);
        drop(w);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopening_continues_the_sequence() {
        let path = temp_path("continue");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            w.append(1, b"first").expect("append");
            w.append(1, b"second").expect("append");
        }
        let w = Writer::open(&path, SyncPolicy::Never).expect("reopen");
        assert_eq!(w.next_seq(), 2, "must not restart numbering");
        drop(w);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_torn_tail_is_truncated_on_reopen() {
        let path = temp_path("torn");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            w.append(1, b"complete").expect("append");
            w.sync().expect("sync");
        }
        // Simulate a writer that died mid-record.
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("append");
            f.write_all(&[0x4F, 0x51, 0x52, 0x4A, 0x01])
                .expect("partial write");
            f.flush().expect("flush");
        }

        let mut w = Writer::open(&path, SyncPolicy::Never).expect("reopen after tear");
        assert_eq!(w.next_seq(), 1, "the torn record must not consume a number");
        w.append(1, b"after recovery").expect("append");
        w.sync().expect("sync");

        let records = Reader::open(&path)
            .expect("open reader")
            .replay()
            .expect("replay");
        assert_eq!(records.frames.len(), 2);
        assert_eq!(records.frames[1].payload, b"after recovery");
        drop(w);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn append_at_refuses_a_gap() {
        let path = temp_path("gap");
        let mut w = Writer::open(&path, SyncPolicy::Never).expect("open");
        w.append(1, b"zero").expect("append");
        let err = w.append_at(5, 1, b"jumped").expect_err("must refuse");
        assert!(matches!(
            err,
            JournalError::SequenceGap {
                expected: 1,
                found: 5
            }
        ));
        drop(w);
        std::fs::remove_file(&path).ok();
    }
}
