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
use std::io::{BufWriter, Read, Seek, Write};
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
    /// Holds the kernel lock that stops a second writer, for as long as
    /// this writer lives. Released by closing, never by deleting.
    _lock: File,
    file: BufWriter<File>,
    policy: SyncPolicy,
    next_seq: u64,
    bytes_written: u64,
    scratch: Vec<u8>,
    /// Set by the first failed write or flush; see [`JournalError::Broken`].
    broken: bool,
    /// Appends left before a simulated failure; see [`Writer::fail_after`].
    fail_in: Option<u64>,
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
            _lock: lock,
            file: BufWriter::with_capacity(1 << 16, file),
            policy,
            next_seq,
            bytes_written: clean_len,
            scratch: Vec::with_capacity(1024),
            broken: false,
            fail_in: None,
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
        if self.broken {
            return Err(JournalError::Broken);
        }
        let seq = self.next_seq;
        let next = seq.checked_add(1).ok_or(JournalError::SequenceExhausted)?;
        if let Some(left) = self.fail_in {
            if left == 0 {
                self.broken = true;
                return Err(JournalError::Io(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "simulated: no space left on device",
                )));
            }
            self.fail_in = Some(left - 1);
        }
        self.scratch.clear();
        Frame::new(seq, kind, payload.to_vec()).encode_into(&mut self.scratch);
        self.guard(|w| {
            w.file.write_all(&w.scratch)?;
            match w.policy {
                SyncPolicy::EveryRecord => {
                    w.file.flush()?;
                    w.file.get_ref().sync_data()
                }
                SyncPolicy::EveryRecordNoFsync => w.file.flush(),
                SyncPolicy::Never => Ok(()),
            }
        })?;
        self.bytes_written += self.scratch.len() as u64;
        self.next_seq = next;
        Ok(seq)
    }

    /// Run one I/O step, and stop appending for good if it fails.
    fn guard(&mut self, step: impl FnOnce(&mut Self) -> std::io::Result<()>) -> Result<()> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        step(self).map_err(|e| {
            self.broken = true;
            JournalError::Io(e)
        })
    }

    /// Accept `appends` more records, then fail the next write as a full
    /// disk does, and every write after it.
    ///
    /// For testing what a caller does when its journal stops accepting
    /// records: a real failure — a full disk, a vanished device — is not
    /// something a test can arrange portably, and a simulation has to be
    /// able to arrange it at a chosen point in a run.
    #[doc(hidden)]
    pub fn fail_after(&mut self, appends: u64) {
        self.fail_in = Some(appends);
    }

    /// Flush buffered records to the OS.
    ///
    /// # Errors
    /// I/O failures.
    pub fn flush(&mut self) -> Result<()> {
        self.guard(|w| w.file.flush())
    }

    /// Flush and fsync, whatever the policy.
    ///
    /// # Errors
    /// I/O failures.
    pub fn sync(&mut self) -> Result<()> {
        self.guard(|w| {
            w.file.flush()?;
            w.file.get_ref().sync_data()
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // A buffered record that never reached the OS would be a record
        // the journal claims to have and does not. Errors cannot be
        // returned from drop; callers that need the guarantee call
        // `sync` explicitly, which is why `sync` is public.
        let _ = self.file.flush();
        // The lock goes with the handle. The file is left in place:
        // deleting it by name could remove one a successor has already
        // locked, and a second writer would then lock a new file beside
        // the first.
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
/// A kernel lock on a file beside the journal, held until the writer is
/// dropped. The kernel releases it when the holder exits however it
/// exits, so a process killed outright leaves nothing that refuses the
/// next start — the old lock, a file whose existence was the claim, did,
/// until someone deleted it by hand. And no check-then-act: the kernel
/// decides, once, which caller holds it, which is the failure a `pgrep`
/// in a start script cannot avoid.
///
/// Beside the journal rather than on it: on Windows a lock on the
/// journal itself would also stop other processes reading it.
fn acquire(journal: &Path) -> Result<File> {
    let lock = journal.with_extension("lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?;
    match file.try_lock() {
        Ok(()) => {
            // For a human wondering who holds it; the lock, not this
            // text, is what refuses a second writer.
            let _ = file.set_len(0);
            let _ = writeln!(
                file,
                "pid {} writing {}",
                std::process::id(),
                journal.display()
            );
            let _ = file.sync_all();
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            // Read through the handle already open, not the path.
            let mut text = String::new();
            let _ = file.rewind();
            let _ = Read::by_ref(&mut file).take(1024).read_to_string(&mut text);
            let text = text.trim();
            Err(JournalError::AlreadyOpen {
                lock,
                held_by: if text.is_empty() {
                    "nothing about itself".to_string()
                } else {
                    text.to_string()
                },
            })
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {

    /// The failure this exists to prevent, reproduced.
    ///
    /// Two live trading processes were once started ninety-two seconds
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
        drop(_second);

        // A lock file left by a process that died without dropping —
        // killed, out of memory, power lost — holds no lock, and refuses
        // nothing. The old lock was the file itself, and did.
        std::fs::write(path.with_extension("lock"), "pid 1 writing a journal\n").expect("stale");
        let _third = Writer::open(&path, SyncPolicy::Never).expect("a stale file is not a lock");

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

    /// After one failed write the writer appends nothing more, and a
    /// reopen starts from the last whole record.
    ///
    /// The failure is real: the file underneath is swapped for a
    /// read-only handle, so the flush fails the way a full disk does.
    #[test]
    fn a_failed_write_stops_every_later_append() {
        let path = temp_path("broken");
        {
            let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            w.append(1, b"kept").expect("append");
            w.file = BufWriter::new(File::open(&path).expect("read-only handle"));

            assert!(matches!(w.append(1, b"lost"), Err(JournalError::Io(_))));
            assert!(matches!(w.append(1, b"after"), Err(JournalError::Broken)));
            assert!(matches!(w.flush(), Err(JournalError::Broken)));
            assert!(matches!(w.sync(), Err(JournalError::Broken)));
            assert_eq!(w.next_seq(), 1, "a failed append assigns no number");
        }
        let w = Writer::open(&path, SyncPolicy::Never).expect("reopen");
        assert_eq!(
            w.next_seq(),
            1,
            "the journal holds exactly the record that was written"
        );
        drop(w);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_test_hook_fails_a_writer_the_same_way_at_the_chosen_point() {
        let path = temp_path("hook");
        let mut w = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
        w.fail_after(2);
        w.append(1, b"one").expect("first");
        w.append(1, b"two").expect("second");
        assert!(matches!(w.append(1, b"three"), Err(JournalError::Io(_))));
        assert!(matches!(w.append(1, b"four"), Err(JournalError::Broken)));
        drop(w);
        let w = Writer::open(&path, SyncPolicy::Never).expect("reopen");
        assert_eq!(w.next_seq(), 2);
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
