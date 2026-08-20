//! Market data capture.
//!
//! Implements `docs/CAPTURE-FORMAT.md`. The rule that shapes everything
//! here: a day of market data that was not captured is gone permanently,
//! and a day captured in the wrong format is nearly as bad. So the bytes
//! the venue sent are the bytes on disk, aggregation is left to the
//! consumer, and a completed day is sealed with a manifest that lets
//! anyone verify it years later.

/// Version of the on-disk capture format, per `docs/CAPTURE-FORMAT.md`.
pub const FORMAT_VERSION: u32 = 1;

pub mod archive;

// The book moved to `oq-book` so the matcher can use it without
// inheriting this crate's TLS stack. Re-exported at the old paths: every
// call site here and in the binaries keeps working, and a reader who
// follows `oq_l2feed::book::Book` lands on the same type the engine
// matches against.
pub use oq_book::{book, depth};
pub mod conformance;
pub mod day;
pub mod disk;
pub mod frame;
pub mod manifest;
pub mod session;
pub mod stream;
pub mod venue;
pub mod writer;
pub mod ws;

pub use day::UtcDay;
pub use frame::{DecodeError, Kind, Record, decode, decode_all};
pub use manifest::{ClockOffset, Manifest, ManifestBuilder, control, is_gap};
pub use stream::{Software, StreamId};
pub use writer::{CaptureWriter, SealedDay};
