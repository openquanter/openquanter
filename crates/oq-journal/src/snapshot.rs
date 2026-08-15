//! Snapshots: state as of a sequence number.
//!
//! A journal alone is enough to rebuild state, but replaying six months
//! of events to start a process is not a recovery procedure anyone will
//! run. A snapshot is a checkpoint: state as of sequence N, so recovery
//! is "load the snapshot, apply everything from N onward".
//!
//! The load-bearing detail is that **a snapshot is taken at a sequence
//! boundary, never at a wall-clock moment**. "State at 03:00" is not a
//! well-defined thing in an event-sourced system — events are still in
//! flight, and two components would disagree about what 03:00 meant.
//! "State after event N" is exact, reproducible, and lets a reader
//! verify the snapshot by replaying to the same point.
//!
//! Snapshots are content-checksummed for the same reason journal
//! records are: a snapshot that was half-written during a crash must be
//! rejected, not loaded. A rejected snapshot costs a longer replay; a
//! silently corrupted one costs a wrong position.

use crate::{JournalError, Result};
use oq_hash::crc32;
use std::fs;
use std::path::{Path, PathBuf};

/// Bytes preceding a snapshot payload.
const HEADER_LEN: usize = 20;
/// `OQSN`, little-endian.
const MAGIC: u32 = u32::from_le_bytes(*b"OQSN");
const VERSION: u16 = 1;

/// State captured at a sequence boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot reflects every event with sequence < `upto_seq`.
    ///
    /// Half-open by construction: `upto_seq` is exactly the sequence a
    /// recovering process resumes from, so there is no off-by-one to
    /// get wrong at the call site.
    pub upto_seq: u64,
    /// Opaque serialized state. This crate does not interpret it.
    pub payload: Vec<u8>,
}

impl Snapshot {
    #[must_use]
    pub fn new(upto_seq: u64, payload: Vec<u8>) -> Self {
        Self { upto_seq, payload }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        let mut checked = [0u8; HEADER_LEN - 8];
        checked[0..2].copy_from_slice(&VERSION.to_le_bytes());
        checked[2..4].copy_from_slice(&0u16.to_le_bytes()); // reserved
        checked[4..12].copy_from_slice(&self.upto_seq.to_le_bytes());

        let mut hashed = Vec::with_capacity(checked.len() + self.payload.len());
        hashed.extend_from_slice(&checked);
        hashed.extend_from_slice(&self.payload);
        let checksum = crc32(&hashed);

        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&checked);
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a snapshot, rejecting anything that does not verify.
    ///
    /// # Errors
    /// [`JournalError::Corrupt`] for a truncated or damaged snapshot.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        use crate::frame::FrameError;
        let corrupt = |cause| JournalError::Corrupt {
            at_offset: 0,
            cause,
        };
        if bytes.len() < HEADER_LEN {
            return Err(corrupt(FrameError::Incomplete {
                needed: HEADER_LEN,
                available: bytes.len(),
            }));
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
        if magic != MAGIC {
            return Err(corrupt(FrameError::BadMagic { found: magic }));
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"));
        if version != VERSION {
            return Err(corrupt(FrameError::UnknownVersion { found: version }));
        }
        let upto_seq = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let expected = u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes"));

        let mut hashed = Vec::with_capacity(bytes.len() - 8);
        hashed.extend_from_slice(&bytes[4..16]);
        hashed.extend_from_slice(&bytes[HEADER_LEN..]);
        let computed = crc32(&hashed);
        if computed != expected {
            return Err(corrupt(FrameError::ChecksumMismatch { expected, computed }));
        }

        Ok(Self {
            upto_seq,
            payload: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

/// Snapshots on disk, newest wins.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    #[must_use]
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    fn file_for(&self, upto_seq: u64) -> PathBuf {
        // Zero-padded so lexical order is numeric order, which keeps
        // directory listings and glob-based tooling honest.
        self.dir.join(format!("snapshot-{upto_seq:020}.oqsn"))
    }

    /// Write a snapshot durably.
    ///
    /// Written to a temporary name and renamed, so a reader never
    /// observes a half-written snapshot: rename is atomic within a
    /// directory, `write` is not.
    ///
    /// # Errors
    /// I/O failures.
    pub fn save(&self, snapshot: &Snapshot) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let final_path = self.file_for(snapshot.upto_seq);
        let tmp_path = final_path.with_extension("oqsn.tmp");
        fs::write(&tmp_path, snapshot.encode())?;
        fs::rename(&tmp_path, &final_path)?;
        Ok(final_path)
    }

    /// The newest snapshot at or before `upto_seq`, if any.
    ///
    /// A damaged snapshot is skipped rather than fatal: an older valid
    /// checkpoint plus a longer replay is a strictly better outcome
    /// than refusing to start.
    ///
    /// # Errors
    /// I/O failures other than a missing directory.
    pub fn latest_at_or_before(&self, upto_seq: u64) -> Result<Option<Snapshot>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(JournalError::Io(e)),
        };

        let mut candidates: Vec<PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "oqsn"))
            .collect();
        candidates.sort();

        for path in candidates.iter().rev() {
            let Ok(bytes) = fs::read(path) else { continue };
            let Ok(snapshot) = Snapshot::decode(&bytes) else {
                continue;
            };
            if snapshot.upto_seq <= upto_seq {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }

    /// The newest valid snapshot, if any.
    ///
    /// # Errors
    /// I/O failures other than a missing directory.
    pub fn latest(&self) -> Result<Option<Snapshot>> {
        self.latest_at_or_before(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oq-snap-{}-{}-{}",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }

    #[test]
    fn round_trip() {
        let snap = Snapshot::new(1234, b"serialized state".to_vec());
        let decoded = Snapshot::decode(&snap.encode()).expect("valid");
        assert_eq!(decoded, snap);
    }

    #[test]
    fn truncation_is_rejected() {
        let bytes = Snapshot::new(1, b"state".to_vec()).encode();
        for cut in 0..bytes.len() {
            assert!(
                Snapshot::decode(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn corruption_is_rejected() {
        let mut bytes = Snapshot::new(1, b"state".to_vec()).encode();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(Snapshot::decode(&bytes).is_err());
    }

    #[test]
    fn latest_ignores_damaged_files_and_prefers_the_newest_valid_one() {
        let dir = temp_dir("latest");
        let store = SnapshotStore::new(&dir);
        store
            .save(&Snapshot::new(10, b"ten".to_vec()))
            .expect("save");
        store
            .save(&Snapshot::new(20, b"twenty".to_vec()))
            .expect("save");

        // Damage the newest.
        let newest = store.file_for(20);
        let mut bytes = fs::read(&newest).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&newest, &bytes).expect("write");

        let recovered = store.latest().expect("latest").expect("some snapshot");
        assert_eq!(
            recovered.upto_seq, 10,
            "a damaged checkpoint must fall back, not fail"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn latest_at_or_before_respects_the_bound() {
        let dir = temp_dir("bounded");
        let store = SnapshotStore::new(&dir);
        store
            .save(&Snapshot::new(5, b"five".to_vec()))
            .expect("save");
        store
            .save(&Snapshot::new(50, b"fifty".to_vec()))
            .expect("save");

        let s = store.latest_at_or_before(20).expect("query").expect("some");
        assert_eq!(s.upto_seq, 5);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let store = SnapshotStore::new(temp_dir("missing"));
        assert!(store.latest().expect("query").is_none());
    }
}
