//! Time, as the live loop is allowed to read it.
//!
//! The live loop decides things by the clock — when a deadline passes,
//! when a reconnect may be tried again, when a silent stream is presumed
//! dead, what time to stamp on a record. Read straight from the system,
//! none of those can be tested except by waiting for them, and a test
//! that waits is a test that is either slow or flaky. Read through this,
//! a test hands the loop a clock it moves by hand, and a run with the
//! same inputs makes the same decisions at the same instants — which is
//! what the deterministic simulation of the whole process needs first.
//!
//! Two readings, because they answer different questions. `wall` is the
//! time a venue would recognise, for stamps and for anything compared
//! against the venue's own clock. `elapsed` only moves forward and is
//! for intervals: a wall clock stepped back by NTP must not make a
//! backoff wait twice as long or a deadline never arrive.
//!
//! Not everything goes through it. How long this process itself took to
//! do something — the submit latency the session records — is a
//! measurement of the machine, not a decision, and a virtual clock would
//! only ever report zero for it.

use std::cell::Cell;
use std::time::{Duration, Instant};

use oq_types::Nanos;

/// A source of time for the live loop.
pub trait Clock {
    /// Nanoseconds since the Unix epoch.
    fn wall(&self) -> Nanos;
    /// Time since this clock started. Never goes backwards.
    fn elapsed(&self) -> Duration;
    /// Wait for `d`.
    fn sleep(&self, d: Duration);
}

/// The system's clocks.
#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn wall(&self) -> Nanos {
        Nanos(oq_l2feed::session::now_ns())
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that moves only when told to.
///
/// Sleeping advances it by the requested amount, so code that waits
/// finishes its wait at once and the time it waited is still accounted
/// for.
#[derive(Debug)]
pub struct ManualClock {
    wall_at_start: Nanos,
    elapsed: Cell<Duration>,
}

impl ManualClock {
    /// A clock whose wall reading starts at `wall_at_start`.
    #[must_use]
    pub const fn starting_at(wall_at_start: Nanos) -> Self {
        Self {
            wall_at_start,
            elapsed: Cell::new(Duration::ZERO),
        }
    }

    /// Move time forward.
    pub fn advance(&self, d: Duration) {
        self.elapsed.set(self.elapsed.get() + d);
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> Nanos {
        let since = i64::try_from(self.elapsed.get().as_nanos()).unwrap_or(i64::MAX);
        Nanos(self.wall_at_start.0.saturating_add(since))
    }

    fn elapsed(&self) -> Duration {
        self.elapsed.get()
    }

    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_told_and_sleeping_moves_it() {
        let c = ManualClock::starting_at(Nanos(1_000));
        assert_eq!(c.wall(), Nanos(1_000));
        assert_eq!(c.elapsed(), Duration::ZERO);
        c.advance(Duration::from_nanos(500));
        c.sleep(Duration::from_nanos(250));
        assert_eq!(c.elapsed(), Duration::from_nanos(750));
        assert_eq!(c.wall(), Nanos(1_750));
    }

    #[test]
    fn the_system_clock_reads_the_epoch_and_moves_forward() {
        let c = SystemClock::new();
        assert!(c.wall().0 > 1_700_000_000_000_000_000);
        let a = c.elapsed();
        c.sleep(Duration::from_millis(1));
        assert!(c.elapsed() > a);
    }
}
