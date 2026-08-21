//! Rebuilding a venue's order book from incremental depth.
//!
//! Extracted from `oq-l2feed` so that both halves of the problem can use
//! it. The capture side replays an archive into a book to prove the
//! archive is usable; the matching side needs the same book to decide
//! where an order actually sits in a queue. One implementation, because
//! two would agree until the day they did not — and the day they did not
//! would be the day a backtest reported a fill the venue never gave.
//!
//! Zero dependencies, which is why it can be a crate of its own rather
//! than living where it was. `oq-l2feed` carries a TLS stack because it
//! talks to venues; the matcher must not inherit one to look at a price
//! level.

pub mod book;
pub mod depth;

pub use book::{Applied, Book, SequenceError, Side, Sweep};
pub use depth::{DepthUpdate, Level};
