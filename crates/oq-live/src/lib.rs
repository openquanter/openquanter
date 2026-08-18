//! The process that trades.
//!
//! Everything else in this workspace is a part. The gate decides whether
//! an order may be sent, the gateway knows how to send it, the stream
//! says what happened, and none of them know about each other. This
//! crate is where they meet, and the meeting is the whole point: an
//! order path that is only correct when the caller remembers to check
//! first is not a safe order path, it is a set of tools next to a
//! warning label.
//!
//! # The gate becomes unbypassable here
//!
//! [`Session::submit`] is the only way to send an order through this
//! crate, and it always consults the gate. The gate returns a permit
//! carrying the order it approved, and that permit is what reaches the
//! venue — so a check cannot validate one order while a different one
//! goes out, and there is no second path that skips the check. The
//! permit type was shaped for this before this existed, because
//! retrofitting it once callers held the alternative would have been
//! the expensive order.
//!
//! # Judgement here, input and output outside
//!
//! The supervisor decides *what should happen* and returns it. Doing it
//! is [`Session`]'s job. Time arrives as an argument and never from a
//! clock, so every decision this crate makes can be replayed exactly —
//! which is the difference between an incident that can be
//! reconstructed and one that can only be regretted.
//!
//! # Startup is the strict one
//!
//! A process that begins trading beside a position it does not know
//! about is a process whose risk limits mean nothing: the limits are
//! computed against a picture that is already wrong. So an unrecognised
//! position at startup stops the process rather than warning about it.
//! That is a deliberate choice to fail loudly at the only moment when
//! failing is cheap.

#![forbid(unsafe_code)]

pub mod book;
pub mod books;
pub mod feed;
pub mod latency;
pub mod metrics;
pub mod record;
pub mod recovery;
pub mod run;
pub mod session;
pub mod shadow;
pub mod supervisor;
pub mod trader;

pub use book::{Book, Position};
pub use feed::{MarketData, Stream};
pub use latency::Latency;
pub use record::{OutcomeTag, Record};
pub use recovery::{InFlight, Recovered, Unaccounted, recover};
pub use session::{Session, SessionConfig, StartupRefusal, Submission};
pub use supervisor::{Action, Supervisor, Timings};
pub use trader::{Outcome, Trader};
