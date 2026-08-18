//! The data plane: what was knowable, and when.
//!
//! Everything here exists to prevent one class of error — a backtest
//! consuming information that did not exist at the moment it claims to
//! have acted. That error does not announce itself. Nothing crashes, no
//! test goes red, and the equity curve simply bends upward. It is
//! caught by construction or not at all, which is why the boundary
//! rules live in types rather than in guidance.
//!
//! Three layers, each answering a different question:
//!
//! - [`ticks`] — the observation stream, carrying both timestamps, in a
//!   form a replay loop can walk without allocating.
//! - [`asof`] — attaching a value to an instant, with the boundary
//!   strictly *before* by default and joins keyed on arrival rather
//!   than event time.
//! - [`bitemporal`] — reference data that remembers what was believed
//!   and when, so a run replayed today reproduces the answer it gave
//!   when it ran rather than picking up later corrections.
//!
//! ## Dependencies
//!
//! The default build has none beyond the workspace. Columnar archive
//! support belongs behind an off-by-default feature: it pulls a large
//! dependency tree, and per the capture format's design a columnar file
//! is a post-sealing conversion target rather than anything the replay
//! path needs.

#![forbid(unsafe_code)]

#[cfg(feature = "parquet")]
pub mod columnar;

pub mod asof;
pub mod bitemporal;
pub mod ticks;

pub use asof::{AsOf, Series, Timed, Timeline};
pub use bitemporal::{Bitemporal, Version};
pub use ticks::{FeedLatency, Header, TickStream, decode, encode, read_header};

/// Anything that went wrong reading data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated {
        needed: usize,
        available: usize,
    },
    BadMagic {
        found: u32,
    },
    UnknownVersion {
        found: u16,
    },
    ChecksumMismatch {
        expected: u32,
        computed: u32,
    },
    /// Ticks must not go backwards in exchange time.
    ///
    /// Reported with the offending index because "the data is out of
    /// order" is not actionable and "record 41,209 goes backwards from
    /// t=... to t=..." is.
    OutOfOrder {
        index: usize,
        previous: i64,
        found: i64,
    },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated { needed, available } => {
                write!(f, "truncated: needed {needed} bytes, {available} available")
            }
            Self::BadMagic { found } => write!(f, "not a tick file: magic {found:#010x}"),
            Self::UnknownVersion { found } => write!(f, "unknown tick file version {found}"),
            Self::ChecksumMismatch { expected, computed } => write!(
                f,
                "checksum mismatch: expected {expected:#010x}, computed {computed:#010x}"
            ),
            Self::OutOfOrder {
                index,
                previous,
                found,
            } => write!(
                f,
                "tick {index} goes backwards: previous exchange time {previous}, found {found}"
            ),
        }
    }
}

impl core::error::Error for Error {}
