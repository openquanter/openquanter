//! Parity: does this run still do what the reference run did?
//!
//! The measuring instrument, built before the thing it measures. A port,
//! a refactor, a fidelity-tier change, or a second implementation of the
//! same strategy in another language all need the same answer — not
//! "are the results close" but "which fill was the first to differ, and
//! by how much".
//!
//! Two rules shape the design:
//!
//! 1. **Exact where exactness is possible.** Prices and quantities are
//!    fixed-point integers and are compared exactly. Tolerance applies
//!    only to derived monetary values.
//! 2. **A stale baseline is stale, not violated.** Every run carries the
//!    identity triple (code commit, input data hash, configuration
//!    hash). If data or configuration moved, the report says so and
//!    concludes nothing about behavior. See [`manifest`].

pub mod diff;
pub mod manifest;
pub mod record;

pub use diff::{Difference, FieldDifference, ParityReport, compare};
pub use manifest::{BaselineStatus, IdentityElement, RunManifest};
pub use record::{Fill, Nanos, RunOutput};
