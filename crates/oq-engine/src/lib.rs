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
//! Only L0 is implemented here. The ladder is stated up front because
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
pub mod tick;

pub use book::{Book, Resting};
pub use l0::{FillReason, L0Engine, L0Fill};
pub use tick::Tick;
