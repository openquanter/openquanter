//! Two clocks, always.
//!
//! A market data record carries the exchange's timestamp and the local
//! receipt timestamp. Their difference is feed latency, and feed latency
//! is not recoverable after capture: a historical file that kept only
//! the exchange timestamp can never support a latency-aware simulation,
//! no matter what is done to it later. This is the single most common
//! reason a public historical dataset cannot be used for high-fidelity
//! backtesting.
//!
//! Nanoseconds since the Unix epoch, as `i64`. That range covers years
//! 1678–2262, which outlives any use of this software, and the signed
//! type keeps differences well-defined without a cast.
//!
//! There is no "now" here. Wall-clock time enters the system as an
//! event through the sequencer; a type that could read the clock would
//! make the determinism rule unenforceable at exactly the layer that
//! most wants to break it.

/// Nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Nanos(pub i64);

impl Nanos {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    #[must_use]
    pub const fn from_secs(s: i64) -> Self {
        Self(s.saturating_mul(1_000_000_000))
    }

    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0 / 1_000_000
    }

    #[must_use]
    pub const fn as_secs(self) -> i64 {
        self.0 / 1_000_000_000
    }

    /// Signed difference `self - earlier`, in nanoseconds.
    #[must_use]
    pub const fn since(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    /// The UTC day this instant falls in, as days since the epoch.
    ///
    /// Floor division, so instants before 1970 land on the day that
    /// contains them rather than the day after. Daily accounting
    /// buckets and funding windows both key on this.
    #[must_use]
    pub const fn utc_day(self) -> i64 {
        const NANOS_PER_DAY: i64 = 86_400_000_000_000;
        if self.0 >= 0 {
            self.0 / NANOS_PER_DAY
        } else {
            -((-self.0 + NANOS_PER_DAY - 1) / NANOS_PER_DAY)
        }
    }
}

/// An exchange timestamp paired with the local receipt timestamp.
///
/// Both are required. A constructor that let one default would be used,
/// and the resulting data would be indistinguishable from correct data
/// until someone tried to model latency with it years later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Stamp {
    /// When the venue says the event happened.
    pub exch: Nanos,
    /// When this process observed it.
    pub local: Nanos,
}

impl Stamp {
    #[must_use]
    pub const fn new(exch_ns: i64, local_ns: i64) -> Self {
        Self {
            exch: Nanos(exch_ns),
            local: Nanos(local_ns),
        }
    }

    /// Feed latency: local receipt minus exchange timestamp.
    ///
    /// Can be negative when the local clock is behind the venue's. That
    /// is a fact about the capture, not an error to hide — a negative
    /// feed latency in a dataset is the signal that the capture host's
    /// clock discipline failed, and callers need to see it.
    #[must_use]
    pub const fn feed_latency(&self) -> i64 {
        self.local.since(self.exch)
    }

    /// A stamp for simulated events, where both clocks are the same.
    ///
    /// Backtests generate events from historical records that already
    /// carry both timestamps; this is for synthetic events (timers,
    /// injected scenarios) that have no separate arrival.
    #[must_use]
    pub const fn synthetic(ns: i64) -> Self {
        Self {
            exch: Nanos(ns),
            local: Nanos(ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_latency_can_be_negative() {
        let skewed = Stamp::new(1_000, 900);
        assert_eq!(skewed.feed_latency(), -100);
    }

    #[test]
    fn utc_day_floors_across_the_epoch() {
        assert_eq!(Nanos::from_secs(0).utc_day(), 0);
        assert_eq!(Nanos::from_secs(86_399).utc_day(), 0);
        assert_eq!(Nanos::from_secs(86_400).utc_day(), 1);
        assert_eq!(Nanos::from_secs(-1).utc_day(), -1);
        assert_eq!(Nanos::from_secs(-86_400).utc_day(), -1);
        assert_eq!(Nanos::from_secs(-86_401).utc_day(), -2);
    }

    #[test]
    fn ordering_is_by_exchange_time_first() {
        let early_exch_late_local = Stamp::new(1, 100);
        let late_exch_early_local = Stamp::new(2, 3);
        assert!(early_exch_late_local < late_exch_early_local);
    }
}
