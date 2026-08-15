//! The journal: audit trail, replay source, and recovery mechanism, in
//! one artifact.
//!
//! Every event enters the system by being numbered and appended here
//! *before* the core observes it. That ordering is the whole design.
//! It buys four properties that are otherwise pursued separately and
//! achieved partially:
//!
//! - **Audit.** What happened is not a question of log level. The
//!   journal is the record, and it is the same bytes the core consumed.
//! - **Replay.** A run can be reproduced exactly by feeding the journal
//!   back to a deterministic core. Debugging becomes reading, not
//!   guessing.
//! - **Recovery.** A crashed process rebuilds state from the last
//!   snapshot plus the journal tail. The recovery path and the startup
//!   path are the same code, so recovery is exercised on every start.
//! - **Observation.** Monitoring, analytics, and cross-process
//!   aggregation read the journal. Nothing has to be threaded back
//!   through the core to be watched, so watching cannot perturb it.
//!
//! ## Torn tails are normal
//!
//! A process that dies mid-write leaves a partial record. This is
//! expected, not exceptional, and the reader treats a truncated final
//! record as the end of the journal — reporting where it stopped, so a
//! writer can resume from a clean boundary. Corruption *in the middle*
//! of a journal is a different matter and is reported as an error.
//! Conflating the two would either make normal crashes look like data
//! loss or make data loss look like a normal crash.
//!
//! ## What this crate is not
//!
//! It is not yet memory-mapped. The current backend is buffered file
//! I/O behind an API that does not expose the difference, because the
//! properties above are what callers depend on and none of them require
//! mmap. Replacing the backend is a contained change; getting the
//! framing and the torn-tail semantics wrong would not be.

#![forbid(unsafe_code)]

pub mod frame;
pub mod reader;
pub mod snapshot;
pub mod writer;

pub use frame::{
    FRAMING_LEN, Frame, FrameError, HEADER_LEN, MAGIC, MAX_PAYLOAD, TRAILER_LEN, VERSION,
};
pub use reader::{Reader, Replay, ReplayStop};
pub use snapshot::{Snapshot, SnapshotStore};
pub use writer::{SyncPolicy, Writer};

use std::io;

/// Anything that went wrong reading or writing a journal.
#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    /// A record in the middle of the journal did not decode.
    Corrupt {
        at_offset: u64,
        cause: FrameError,
    },
    /// Sequence numbers must be contiguous; a gap means records were
    /// lost, which no reader can paper over.
    SequenceGap {
        expected: u64,
        found: u64,
    },
}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl core::fmt::Display for JournalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal io: {e}"),
            Self::Corrupt { at_offset, cause } => {
                write!(f, "corrupt record at offset {at_offset}: {cause}")
            }
            Self::SequenceGap { expected, found } => {
                write!(f, "sequence gap: expected {expected}, found {found}")
            }
        }
    }
}

impl core::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Corrupt { cause, .. } => Some(cause),
            Self::SequenceGap { .. } => None,
        }
    }
}

/// Shorthand for results in this crate.
pub type Result<T> = core::result::Result<T, JournalError>;
