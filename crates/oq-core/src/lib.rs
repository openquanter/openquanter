//! The deterministic core: one engine, three environments.
//!
//! Backtest, sandbox, and live differ by which adapter produces events
//! and which clock the events carry. The kernel, the matching, the
//! margin model, and the ledger are the same code in all three. That is
//! what makes "the strategy I tested is the strategy I deployed" a
//! structural property rather than a discipline someone has to
//! maintain.
//!
//! ```text
//!   adapters ──▶ Sequencer ──▶ journal ──▶ Kernel ──▶ outputs
//!                                 │
//!                                 └──▶ observers (monitoring, replay,
//!                                       parity, analytics)
//! ```
//!
//! Three rules hold everywhere in this crate, and each one is load
//! bearing rather than stylistic:
//!
//! 1. **No ambient authority in the kernel.** No clock, no randomness,
//!    no I/O, no threads. Time arrives as [`Event::Time`].
//! 2. **Journal before apply.** The sequencer records an event durably
//!    before the kernel sees it, so anything acted on can be replayed.
//! 3. **Outputs are values, not callbacks.** Nothing can re-enter the
//!    kernel mid-decision, so the order of effects is a property of the
//!    code rather than of the call graph.
//!
//! The payoff is demonstrated in this crate's own tests: a scenario is
//! run through a sequencer, then replayed from its journal into a fresh
//! kernel, and both the output sequence and the final account state are
//! asserted identical — including the case where the account was
//! liquidated, which is the path a report most needs to be able to
//! reproduce.

#![forbid(unsafe_code)]

pub mod event;
pub mod kernel;
pub mod sequencer;

pub use event::{Event, kind};
pub use kernel::{Kernel, Output, RejectReason, State, Summary};
pub use sequencer::{ReplayResult, Sequencer, replay};
