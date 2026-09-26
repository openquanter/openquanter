//! Everything the live loop reads the world through, other than the
//! venue's order and account API.
//!
//! The venue's API was already a trait — `Account` — and the rest was
//! not: market data opened its own websockets, the account stream its
//! own, a depth snapshot its own HTTP request, the state directory came
//! from the process environment and a stop request from a signal flag.
//! Each of those is a place a test of the whole loop could not reach.
//! Behind this, a simulation supplies all of them and the loop is the
//! same code it is in production.

use std::path::PathBuf;
use std::time::Duration;

use oq_gateway::{StreamOutcome, UserStream, UserStreamReader, VenueError};
use oq_l2feed::venue::{Deployment, Venue};

use crate::MarketData;
use crate::clock::{Clock, SystemClock};

/// The account's own stream: fills, order updates, and the connection's
/// state.
pub trait UserEvents {
    /// The next event, or why there is none.
    fn next(&mut self) -> StreamOutcome;
    /// Close the connection.
    ///
    /// # Errors
    /// Whatever the transport reports.
    fn close(self: Box<Self>) -> Result<(), VenueError>;
}

impl UserEvents for UserStreamReader {
    fn next(&mut self) -> StreamOutcome {
        Self::next(self)
    }

    fn close(self: Box<Self>) -> Result<(), VenueError> {
        Self::close(*self)
    }
}

/// What the loop reads the world through.
///
/// `&self` throughout: the loop holds the clock for the whole run and
/// asks for the rest along the way, and a simulation that needs to change
/// state behind a call keeps it in cells.
pub trait Environment {
    fn clock(&self) -> &dyn Clock;

    /// Depth and trades for `symbol`, and the adapter that reads them.
    ///
    /// # Errors
    /// The venue has no adapter on that deployment, or no such streams.
    fn market_data(
        &self,
        venue: &str,
        deployment: Deployment,
        symbol: &str,
    ) -> Result<(MarketData, Box<dyn Venue>), String>;

    /// Connect to the account stream `stream` names.
    ///
    /// # Errors
    /// The connection could not be made.
    fn user_events(&self, stream: &UserStream) -> Result<Box<dyn UserEvents>, VenueError>;

    /// A REST depth snapshot, within `timeout`.
    ///
    /// # Errors
    /// Transport or HTTP failure.
    fn depth_snapshot(&self, url: &str, timeout: Duration) -> std::io::Result<Vec<u8>>;

    /// Where durable process state — the reserved order ids — lives.
    fn state_root(&self) -> Option<PathBuf>;

    /// The operator's control port for the process named `name`, if this
    /// environment has one. `None` is a process without a port, which
    /// trades exactly as before.
    ///
    /// # Errors
    /// A port that should exist and could not be made; the caller reports
    /// it and trades on without one.
    fn control(&self, _name: &str) -> Result<Option<Box<dyn crate::control::Control>>, String> {
        Ok(None)
    }

    /// Open the journal at `path` for appending.
    ///
    /// `EveryRecord`, which is the policy the record-before-send
    /// ordering needs: what is written here is decisions — a placement,
    /// a withdrawal, a fill — not market data, so the cost is a device
    /// round trip per order rather than per tick. `EveryRecordNoFsync`
    /// survives a process crash and not a machine one, and the failure
    /// the ordering exists to rule out is a live order this journal has
    /// never heard of: a power loss between the write and the venue's
    /// answer loses exactly the record that would have let a restart ask
    /// about it. The simulator keeps the cheaper policy, because its
    /// inputs exist elsewhere and its journal is for replay.
    ///
    /// # Errors
    /// Whatever opening it reports.
    fn open_journal(&self, path: &std::path::Path) -> oq_journal::Result<oq_journal::Writer> {
        oq_journal::Writer::open(path, oq_journal::SyncPolicy::EveryRecord)
    }

    /// Whether an operator has asked the process to stop.
    fn shutdown_requested(&self) -> bool;

    /// Start listening for stop requests.
    fn listen_for_shutdown(&self) {}
}

/// The production environment: the system clock, the venue's sockets and
/// HTTP, the process's state directory, and its signals.
#[derive(Debug, Default)]
pub struct Production {
    clock: SystemClock,
}

impl Production {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// How long a read of the account stream may block before the loop goes
/// on to its other work.
///
/// Short because the loop has market data to drain and a key to renew;
/// a thread parked on a quiet account is a thread doing neither. Named
/// rather than repeated, because the first connection and every
/// reconnection have to agree about it — a reader replaced with a
/// blockier one would stall the same loop this bounds.
pub const USER_STREAM_READ_TIMEOUT: Duration = Duration::from_millis(200);

impl Environment for Production {
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn market_data(
        &self,
        venue: &str,
        deployment: Deployment,
        symbol: &str,
    ) -> Result<(MarketData, Box<dyn Venue>), String> {
        MarketData::open(venue, deployment, symbol, Duration::from_millis(200))
    }

    fn user_events(&self, stream: &UserStream) -> Result<Box<dyn UserEvents>, VenueError> {
        UserStreamReader::connect(stream, USER_STREAM_READ_TIMEOUT)
            .map(|r| Box::new(r) as Box<dyn UserEvents>)
    }

    fn depth_snapshot(&self, url: &str, timeout: Duration) -> std::io::Result<Vec<u8>> {
        oq_l2feed::ws::fetch_snapshot(url, timeout)
    }

    fn state_root(&self) -> Option<PathBuf> {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
    }

    fn shutdown_requested(&self) -> bool {
        oq_l2feed::session::shutdown_requested()
    }

    /// A socket in the service's runtime directory, when it has one.
    ///
    /// No runtime directory is not an error: a process started from a
    /// shell has none, and gets no port rather than one in `/tmp`.
    fn control(&self, name: &str) -> Result<Option<Box<dyn crate::control::Control>>, String> {
        #[cfg(unix)]
        {
            if std::env::var_os("RUNTIME_DIRECTORY").is_none() {
                return Ok(None);
            }
            crate::control::Socket::open(name)
                .map(|s| Some(Box::new(s) as Box<dyn crate::control::Control>))
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Ok(None)
        }
    }

    fn listen_for_shutdown(&self) {
        oq_l2feed::session::install_signal_handlers();
    }
}
