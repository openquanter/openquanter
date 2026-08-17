//! Where the time goes on the path that sends an order.
//!
//! # What G6 asks for, and what is observable
//!
//! The gate is stated as p99 ≤ 100 µs **from journal write to socket
//! write**. The second boundary is not observable from here: the HTTP
//! client performs connect, write and read inside one call and does not
//! report the instant it put bytes on the wire. Measuring the whole call
//! instead would produce a number dominated by the venue's round trip —
//! tens of milliseconds against a budget of a hundred microseconds — and
//! reporting that as G6 would be reporting a different quantity under the
//! gate's name.
//!
//! So what is measured is the **in-process segment**: from the journal
//! flush returning to the instant before the client is called. That is
//! the part a hundred microseconds is a plausible budget for, and it is
//! the part this project controls. The remaining hop is named here rather
//! than folded in, and G6 stays uncertified until the client boundary is
//! instrumented.
//!
//! # Why a histogram and not an average
//!
//! An average latency answers a question nobody has. What matters is the
//! tail: the order that took a hundred times longer than the median is
//! the one that missed its price, and it is invisible in a mean. So this
//! records a distribution and reports percentiles, and the buckets are
//! log-linear so the resolution is relative — 1% at every magnitude
//! rather than fine at the bottom and useless at the top.
//!
//! # Zero dependencies, on purpose
//!
//! A histogram is two hundred lines and a dependency is a supply chain.
//! More importantly the bucket layout is part of what a recorded
//! percentile means: a crate that changed its layout in a patch release
//! would silently move every number this reports.

/// A log-linear latency histogram over nanoseconds.
///
/// Values are bucketed by magnitude and then linearly within it, giving
/// constant *relative* resolution. With `SUB_BUCKETS` of 128 the error is
/// under one percent anywhere in the range, which is finer than any
/// conclusion anyone draws from a latency percentile.
#[derive(Debug, Clone)]
pub struct Latency {
    /// One row per power of two, each divided linearly.
    buckets: Vec<u64>,
    count: u64,
    total: u128,
    min: u64,
    max: u64,
}

/// Linear divisions inside each magnitude. A power of two, because the
/// index arithmetic is a shift and a mask.
const SUB_BUCKETS: usize = 128;
/// `log2(SUB_BUCKETS)`.
const SUB_BITS: usize = 7;
/// Rows: one linear row for values below `SUB_BUCKETS`, then one per
/// magnitude up to `u64`. Sized so nothing has to be clamped away.
const ROWS: usize = 64 - SUB_BITS + 1;

impl Default for Latency {
    fn default() -> Self {
        Self::new()
    }
}

impl Latency {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: vec![0; SUB_BUCKETS * ROWS],
            count: 0,
            total: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Which bucket a value falls in.
    ///
    /// Values above the range land in the last bucket rather than being
    /// dropped: a measurement that fell off the end is still evidence,
    /// and discarding it would make the tail look better than it is —
    /// which is the one direction a latency report must not be wrong in.
    fn index(ns: u64) -> usize {
        // Row 0 is exact: every value below the sub-bucket count gets its
        // own slot, so nanosecond-scale measurements are not rounded at
        // all.
        if ns < SUB_BUCKETS as u64 {
            return ns as usize;
        }
        let magnitude = 63 - ns.leading_zeros() as usize;
        let shift = magnitude - SUB_BITS;
        // Shifted into [SUB_BUCKETS, 2*SUB_BUCKETS), so subtracting the
        // base gives the position within the row.
        let sub = (ns >> shift) as usize - SUB_BUCKETS;
        let row = magnitude - SUB_BITS + 1;
        (row * SUB_BUCKETS + sub).min(SUB_BUCKETS * ROWS - 1)
    }

    /// The lowest value a bucket represents.
    fn value_at(index: usize) -> u64 {
        if index < SUB_BUCKETS {
            return index as u64;
        }
        let row = index / SUB_BUCKETS;
        let sub = index % SUB_BUCKETS;
        // The inverse of `index`: row 1 is unshifted, and each row above
        // doubles. Rounding is therefore always downward, which is the
        // direction that matters — a report that overstates the tail gets
        // ignored, one that understates it gets believed.
        ((SUB_BUCKETS + sub) as u64) << (row - 1)
    }

    /// Record one measurement.
    pub fn record(&mut self, ns: u64) {
        self.buckets[Self::index(ns)] += 1;
        self.count += 1;
        self.total += u128::from(ns);
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
    }

    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    #[must_use]
    pub const fn max(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.max)
        }
    }

    #[must_use]
    pub const fn min(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.min)
        }
    }

    /// Mean, reported alongside percentiles rather than instead of them.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.total as f64 / self.count as f64)
    }

    /// The value at `quantile`, in nanoseconds.
    ///
    /// `None` when nothing has been recorded — a percentile of no
    /// observations is not zero, and returning zero would report a
    /// latency budget as comfortably met by a process that never ran.
    #[must_use]
    pub fn quantile(&self, quantile: f64) -> Option<u64> {
        if self.count == 0 || !(0.0..=1.0).contains(&quantile) {
            return None;
        }
        let target = (quantile * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= target {
                return Some(Self::value_at(i));
            }
        }
        Some(self.max)
    }

    /// One line, for a process that is stopping.
    #[must_use]
    pub fn summary(&self) -> String {
        let Some(p50) = self.quantile(0.50) else {
            return "no measurements".to_string();
        };
        format!(
            "n={} p50={}µs p99={}µs p999={}µs max={}µs",
            self.count,
            p50 / 1000,
            self.quantile(0.99).unwrap_or(0) / 1000,
            self.quantile(0.999).unwrap_or(0) / 1000,
            self.max / 1000
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_recorded_reports_nothing_rather_than_zero() {
        // A percentile of no observations is not zero. Reporting zero
        // would show a latency budget comfortably met by a process that
        // never ran, which is the most flattering possible lie.
        let h = Latency::new();
        assert_eq!(h.quantile(0.99), None);
        assert_eq!(h.mean(), None);
        assert_eq!(h.max(), None);
        assert_eq!(h.summary(), "no measurements");
    }

    #[test]
    fn a_single_value_is_every_percentile() {
        let mut h = Latency::new();
        h.record(1234);
        for q in [0.0, 0.5, 0.99, 1.0] {
            let got = h.quantile(q).expect("recorded");
            assert!(
                got <= 1234 && got as f64 >= 1234.0 * 0.99,
                "q{q} gave {got}"
            );
        }
    }

    #[test]
    fn the_resolution_is_relative_and_within_one_percent() {
        // The property that makes log-linear the right layout: the error
        // is a percentage at every magnitude, rather than absolute and
        // therefore useless at one end.
        let mut h = Latency::new();
        for v in [
            1_u64, 7, 63, 100, 999, 1_000, 12_345, 100_000, 1_000_000, 50_000_000,
        ] {
            let mut one = Latency::new();
            one.record(v);
            let got = one.quantile(0.5).expect("recorded");
            let error = (v as f64 - got as f64).abs() / v as f64;
            assert!(error < 0.01, "{v} bucketed to {got}, error {error}");
            h.record(v);
        }
        assert_eq!(h.count(), 10);
    }

    #[test]
    fn percentiles_land_where_the_distribution_puts_them() {
        // Ninety-nine fast and one slow: the median must be fast and the
        // p99 must see the slow one. A histogram that smeared them would
        // hide exactly the observation worth having.
        let mut h = Latency::new();
        for _ in 0..99 {
            h.record(10_000);
        }
        h.record(5_000_000);
        let p50 = h.quantile(0.50).expect("recorded");
        let p99 = h.quantile(0.99).expect("recorded");
        assert!(p50 < 20_000, "p50 {p50}");
        assert!(p99 < 20_000, "p99 is still the fast group: {p99}");
        assert_eq!(h.max(), Some(5_000_000), "and the outlier is not lost");
        assert!(
            h.quantile(1.0).expect("recorded") > 1_000_000,
            "p100 sees it"
        );
    }

    #[test]
    fn a_value_past_the_range_lands_in_the_last_bucket_and_is_not_dropped() {
        // Discarding it would make the tail look better than it is, which
        // is the one direction a latency report must not be wrong in.
        let mut h = Latency::new();
        h.record(10_000);
        h.record(u64::MAX);
        assert_eq!(h.count(), 2);
        assert_eq!(h.max(), Some(u64::MAX));
        assert!(h.quantile(1.0).expect("recorded") > 10_000);
    }

    #[test]
    fn the_mean_is_reported_beside_percentiles_and_not_instead_of_them() {
        // The mean of this distribution is nowhere near either group,
        // which is the reason it is not the headline number.
        let mut h = Latency::new();
        for _ in 0..99 {
            h.record(1_000);
        }
        h.record(1_000_000);
        let mean = h.mean().expect("recorded");
        let p50 = h.quantile(0.5).expect("recorded") as f64;
        assert!(mean > p50 * 5.0, "mean {mean} vs p50 {p50}");
    }

    #[test]
    fn buckets_are_monotonic_so_a_percentile_cannot_go_backwards() {
        // A layout error here shows as a p99 below the p50, which reads as
        // a bug in the caller rather than in the histogram.
        for i in 1..(SUB_BUCKETS * ROWS) {
            assert!(
                Latency::value_at(i) >= Latency::value_at(i - 1),
                "bucket {i} is below {}",
                i - 1
            );
        }
    }

    #[test]
    fn a_recorded_value_never_reads_back_higher_than_itself() {
        // Rounding must be downward. Reading back higher would report a
        // latency nobody measured, and the direction matters: a report
        // that overstates the tail gets ignored, one that understates it
        // gets believed.
        let mut ok = 0;
        for v in (0..2_000_000).step_by(9_973) {
            let mut h = Latency::new();
            h.record(v);
            let back = h.quantile(0.5).expect("recorded");
            assert!(back <= v, "{v} read back as {back}");
            ok += 1;
        }
        assert!(ok > 100);
    }
}
