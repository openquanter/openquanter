//! Running strategies over historical data, and asking what the result
//! depended on.
//!
//! The host itself is deliberately thin: a loop that advances the
//! kernel, shows the strategy what it is allowed to see, and turns its
//! intents into events. Everything that decides an outcome — matching,
//! margin, the ledger — lives in the core, so a backtest and a live
//! session differ only in who produces the ticks.
//!
//! The part of this crate that earns its place is [`deviation`]. A
//! backtest without a margin model is not a simplified backtest; it is
//! a backtest of an account with unlimited collateral, which no venue
//! offers. Running both arms and reporting the gap turns that from an
//! unstated assumption into a measured quantity.

#![forbid(unsafe_code)]

pub mod deviation;
pub mod run;
pub mod strategy;

pub use deviation::{DeviationReport, Verdict};
pub use run::{Liquidation, MarginMode, RunConfig, RunResult, run, tick_at};
pub use strategy::{Context, Intent, Strategy};

/// Re-exported because [`RunConfig::with_fees`] takes it. Configuring a
/// run should not require naming a second crate.
pub use oq_core::Fees;
