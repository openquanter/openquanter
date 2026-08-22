//! Reading an account at the venue.
//!
//! **This crate cannot place or cancel an order.** Not by convention —
//! there is no code here that does it, and adding some is a change a
//! reviewer sees. That boundary is the point: the first thing worth
//! building against a live account is the ability to check what it
//! actually holds, and that has to be usable long before anything is
//! trusted to trade.
//!
//! ## Why read-only first
//!
//! A backtest engine that agrees with a reference over two years has
//! shown it computes the right answer from a file. It has shown nothing
//! about whether its model of a *live* account matches the account. Those
//! differ in ways a file cannot express: a fill arrives twice, a
//! cancellation races an execution, a position is adjusted by something
//! the strategy never sent.
//!
//! A reader can be pointed at a running production account and asked
//! "does my model match?" every few seconds, for weeks, without placing
//! a single order. What it finds is real evidence about the parts that
//! are hardest to test, obtained at no risk. Building the order path
//! first means finding those same things with money on them.
//!
//! ## What is here
//!
//! [`Credentials`] and request signing, and typed reads of the account:
//! balance, positions, open orders, recent executions. Signing is
//! HMAC-SHA256 over the query string, which is what Binance's USDT-M
//! futures API specifies.
//!
//! ## Naming
//!
//! The market-data side of a venue lives in `oq-l2feed`, which has its
//! own `venue` module for stream and poll specifications. This crate is
//! the *account* side, and the roadmap's name for it is `oq-gateway` —
//! worth stating because two modules called `venue` in one workspace
//! would send a reader to the wrong one.

#![forbid(unsafe_code)]

pub mod binance;
pub mod creds;
pub mod reconcile;
pub mod snapshot;
pub mod watch;

pub use binance::{AccountSnapshot, Binance, OpenOrder, PositionSnapshot, Trade, VenueError};
pub use creds::Credentials;
pub use reconcile::{Divergence, Expectation, ExpectedLeg, Reconciliation, Tolerance, reconcile};
pub use snapshot::{Part, Snapshot, SnapshotBuilder};
pub use watch::{Change, Tally, Watcher};
