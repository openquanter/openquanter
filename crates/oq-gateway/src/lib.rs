//! The account side of a venue: reading it, and sending to it.
//!
//! Reads first, and for a long time reads only. The order path in
//! [`exec`] and the socket in [`stream`] were added after the read path
//! had run against a live account for weeks; until then this crate
//! genuinely could not place an order, and that was the point.
//!
//! ## Why the read path came first
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
//! [`Credentials`] and request signing; typed reads of the account —
//! balance, positions, open orders, recent executions; the order path in
//! [`exec`]; the user data socket in [`stream`]; and [`reconcile`] plus
//! [`watch`], which compare what is held against what was expected.
//! Signing is HMAC-SHA256 over the query string, which is what Binance's
//! USDT-M futures API specifies.
//!
//! The reading half remains usable on its own. `oq-recon` is built from
//! this crate and places nothing, which is what makes it safe to point at
//! a production account that something else is trading.
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
pub mod conformance;
pub mod creds;
pub mod exec;
pub(crate) mod json;
pub mod okx;
pub mod reconcile;
pub mod record;
pub mod snapshot;
pub mod stream;
pub mod watch;

pub use binance::parse_user_event;
pub use binance::{AccountSnapshot, Binance, OpenOrder, PositionSnapshot, Trade, VenueError};
pub use creds::Credentials;
pub use exec::{
    Endpoint, Execution, NewOrder, OrderAck, OrderUpdate, Placed, PositionSide, Reject, Unresolved,
    UserEvent, UserStream,
};
pub use reconcile::{Divergence, Expectation, ExpectedLeg, Reconciliation, Tolerance, reconcile};
pub use snapshot::{Part, Snapshot, SnapshotBuilder};
pub use stream::{
    Health, KEY_LIFETIME, KEY_RENEWAL, StreamHealth, StreamOutcome, UserStreamReader,
};
pub use watch::{Change, Tally, Watcher};
