//! Matching: what would have happened to this order?
//!
//! The engine is organized as a **fidelity ladder**. Each rung answers
//! the same question with more of the market's mechanics modelled, and
//! costs more to run:
//!
//! | Tier | Models | Cost | Honest use |
//! |------|--------|------|------------|
//! | L0   | price path through aggregated windows | lowest | parameter sweeps, strategies whose size is small against displayed liquidity |
//! | L1   | queue position, latency, taker impact | moderate | pre-deployment validation, first look at maker strategies |
//! | L2   | reconstructed order book | high | market making, anything whose edge is in the microstructure |
//!
//! L0, L1 and L2's queue are implemented here. Each wraps the one
//! below rather than replacing it, so `FR-MATCH-2`'s freeze on L0 holds
//! by construction rather than by test. L1 wraps L0 rather than
//! replacing it, which is how `FR-MATCH-2`'s promise that L0 is frozen
//! is kept — by construction rather than by vigilance. A transparent L1
//! policy reproduces L0's fills exactly, and a test asserts it.
//!
//! Every L1 parameter is an **assumption**, not a measurement: the tick
//! format carries a price path and a cumulative volume, and neither book
//! depth nor this deployment's real latency is in it. `Policy` therefore
//! has no `Default`, so a run cannot acquire assumptions it did not
//! choose, and `Policy::describe` renders them for a fidelity report.
//!
//! The ladder is stated up front because
//! the tiers must not be confused in a report: a market-making P&L
//! measured at L0 is not a pessimistic estimate, it is a different
//! quantity that happens to have the same units.
//!
//! Two properties hold across every tier:
//!
//! - **Matching is a pure function of state and event.** No clock, no
//!   randomness, no I/O. The same inputs produce the same fills on any
//!   machine, which is what makes replay and parity meaningful.
//! - **Account realism is orthogonal.** Margin, liquidation, and
//!   funding live in `oq-margin` and compose with any tier. A strategy
//!   can be run at L0 with full margin modelling, and for a leveraged
//!   strategy that combination catches more real risk than L2 without
//!   margin would.

#![forbid(unsafe_code)]

pub mod book;
pub mod l0;
pub mod l1;
pub mod l2;
pub mod observation;
pub mod tick;

pub use book::{Book, Resting};
// The venue's depth, re-exported so a caller holding an engine does not
// have to depend on the reconstruction crate to hand it an update.
pub use l0::{FillReason, L0Engine, L0Fill, limit_order, market_order};
pub use l1::{Delay, Impact, L1Engine, Latency, Policy, QueueAhead};
pub use l2::L2Engine;
pub use observation::Observation;
pub use oq_book::{DepthUpdate, Level, SequenceError};
pub use tick::Tick;
