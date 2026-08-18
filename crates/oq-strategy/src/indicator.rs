//! Indicators, and the convention that decides whether two of them agree.
//!
//! # The warm-up is the whole problem
//!
//! Every moving average needs a first value, and implementations disagree
//! about where it comes from. Two common answers:
//!
//! - Seed with the **first sample**. Simple, and the average starts
//!   biased toward one observation.
//! - Seed with a **simple average of the first `period` samples**, then
//!   switch to the exponential recurrence. This is what TA-Lib does, and
//!   therefore what most published parameter sets were tuned against.
//!
//! The two never converge to the same number — they converge to the same
//! *neighbourhood*, which is worse, because a difference that shrinks
//! looks like a rounding error and is not one. A strategy tuned on one
//! and run on the other has parameters that were fitted to a different
//! series.
//!
//! So the seeding is a named value, not a default nobody reads, and
//! nothing here has an implicit one. Ported strategies pick
//! [`Warmup::SimpleAverage`] because that is what they were fitted
//! against; something new can pick either, having been asked.
//!
//! # Values are `f64`, prices are integers
//!
//! The engine carries prices as fixed-point integers because money is
//! not a float. Indicators are ratios and exponentials of prices, which
//! are not money, and doing them in integers means choosing a scale for
//! every intermediate. The caller converts once, at the boundary, and
//! the conversion is exact for every price magnitude these venues quote.
//!
//! # Nothing here allocates per update
//!
//! An indicator asked for a value on every tick is on the hot path. The
//! rolling window is a fixed-size ring; there is no `Vec` growing behind
//! any of these.

/// Where a moving average's first value comes from.
///
/// Named rather than defaulted, because this is the choice that decides
/// whether an implementation agrees with the one a parameter set was
/// fitted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmup {
    /// Seed with a simple average of the first `period` samples, then
    /// switch to the exponential recurrence. TA-Lib's convention, and so
    /// the one most published parameters assume.
    SimpleAverage,
    /// Seed with the first sample. Reports a value immediately, biased
    /// toward one observation for a while.
    FirstSample,
}

/// A fixed-size ring of the last `n` samples.
///
/// Not a `VecDeque`: the capacity never changes, and a ring makes that
/// visible in the type rather than in a comment nobody has to obey.
#[derive(Debug, Clone)]
pub struct Window {
    buf: Vec<f64>,
    next: usize,
    filled: usize,
}

impl Window {
    /// # Panics
    /// A window of zero samples has no meaning to fall back on.
    #[must_use]
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "a window needs at least one sample");
        Self {
            buf: vec![0.0; period],
            next: 0,
            filled: 0,
        }
    }

    /// Add a sample, returning the one it displaced once full.
    pub fn push(&mut self, v: f64) -> Option<f64> {
        let displaced = if self.filled == self.buf.len() {
            Some(self.buf[self.next])
        } else {
            self.filled += 1;
            None
        };
        self.buf[self.next] = v;
        self.next = (self.next + 1) % self.buf.len();
        displaced
    }

    #[must_use]
    pub fn is_full(&self) -> bool {
        self.filled == self.buf.len()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.filled
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.filled == 0
    }

    #[must_use]
    pub fn period(&self) -> usize {
        self.buf.len()
    }

    /// Mean of what is present, or `None` while empty.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        if self.filled == 0 {
            return None;
        }
        let sum: f64 = self.buf[..self.filled].iter().sum();
        Some(sum / self.filled as f64)
    }

    /// Population standard deviation of what is present.
    ///
    /// `None` while empty, and **`None` with one sample** rather than
    /// zero: one observation has no dispersion to report, and a band
    /// built on a zero would sit exactly on the price and signal on
    /// every tick. The population form, not the sample form, because
    /// this window *is* the population being described — there is no
    /// wider set it was drawn from.
    #[must_use]
    pub fn std_dev(&self) -> Option<f64> {
        if self.filled < 2 {
            return None;
        }
        let mean = self.mean()?;
        let n = self.filled as f64;
        let variance: f64 = self.buf[..self.filled]
            .iter()
            .map(|v| (v - mean) * (v - mean))
            .sum::<f64>()
            / n;
        Some(variance.sqrt())
    }

    /// Lowest and highest of what is present, or `None` while empty.
    ///
    /// The pair rather than two calls, because a breakout compares
    /// against both and reading them separately invites reading them
    /// from different states.
    #[must_use]
    pub fn extremes(&self) -> Option<(f64, f64)> {
        if self.filled == 0 {
            return None;
        }
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for v in &self.buf[..self.filled] {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        Some((lo, hi))
    }
}

/// A simple moving average.
///
/// Reports nothing until it has `period` samples. Reporting a partial
/// average earlier would be an average of a different period wearing this
/// one's name, and a strategy comparing it against a threshold cannot
/// tell the difference.
#[derive(Debug, Clone)]
pub struct Sma {
    window: Window,
    sum: f64,
}

impl Sma {
    #[must_use]
    pub fn new(period: usize) -> Self {
        Self {
            window: Window::new(period),
            sum: 0.0,
        }
    }

    /// Add a sample and return the average, once there are enough.
    pub fn update(&mut self, v: f64) -> Option<f64> {
        // Running sum, corrected by the displaced sample rather than
        // recomputed: an O(period) sum per tick is the shape that makes a
        // long period quietly expensive.
        self.sum += v;
        if let Some(old) = self.window.push(v) {
            self.sum -= old;
        }
        self.window
            .is_full()
            .then(|| self.sum / self.window.period() as f64)
    }

    #[must_use]
    pub fn value(&self) -> Option<f64> {
        self.window
            .is_full()
            .then(|| self.sum / self.window.period() as f64)
    }
}

/// An exponential moving average with a stated warm-up.
#[derive(Debug, Clone)]
pub struct Ema {
    alpha: f64,
    warmup: Warmup,
    seed: Option<Sma>,
    value: Option<f64>,
}

impl Ema {
    /// # Panics
    /// A period of zero has no smoothing factor.
    #[must_use]
    pub fn new(period: usize, warmup: Warmup) -> Self {
        assert!(period > 0, "an EMA needs a period");
        Self {
            alpha: 2.0 / (period as f64 + 1.0),
            warmup,
            seed: matches!(warmup, Warmup::SimpleAverage).then(|| Sma::new(period)),
            value: None,
        }
    }

    pub fn update(&mut self, v: f64) -> Option<f64> {
        match (&mut self.seed, self.value) {
            // Still seeding: the simple average is the value, and the
            // exponential recurrence starts from it.
            (Some(sma), None) => {
                self.value = sma.update(v);
                self.value
            }
            (_, Some(prev)) => {
                let next = prev + self.alpha * (v - prev);
                self.value = Some(next);
                Some(next)
            }
            (None, None) => {
                debug_assert!(matches!(self.warmup, Warmup::FirstSample));
                self.value = Some(v);
                Some(v)
            }
        }
    }

    #[must_use]
    pub const fn value(&self) -> Option<f64> {
        self.value
    }
}

/// Moving average convergence/divergence.
///
/// Three numbers, and the signal line is an EMA **of the MACD line**, not
/// of the price — an implementation that smooths the price instead
/// produces a plausible curve that crosses at different times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacdValue {
    /// Fast EMA minus slow EMA.
    pub macd: f64,
    /// EMA of `macd`.
    pub signal: f64,
    /// `macd - signal`.
    pub histogram: f64,
}

/// The MACD indicator.
#[derive(Debug, Clone)]
pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
}

impl Macd {
    /// The conventional 12/26/9, with the warm-up stated.
    #[must_use]
    pub fn new(fast: usize, slow: usize, signal: usize, warmup: Warmup) -> Self {
        Self {
            fast: Ema::new(fast, warmup),
            slow: Ema::new(slow, warmup),
            signal: Ema::new(signal, warmup),
        }
    }

    /// Add a price and return the three values, once all three are warm.
    pub fn update(&mut self, price: f64) -> Option<MacdValue> {
        let fast = self.fast.update(price);
        let slow = self.slow.update(price);
        let (Some(fast), Some(slow)) = (fast, slow) else {
            return None;
        };
        let macd = fast - slow;
        // The signal line smooths the MACD line. Feeding it the price
        // instead is the error that produces a curve which looks right
        // and crosses at the wrong moments.
        let signal = self.signal.update(macd)?;
        Some(MacdValue {
            macd,
            signal,
            histogram: macd - signal,
        })
    }
}

/// Relative strength index, with Wilder's smoothing.
///
/// The other convention trap. Wilder's original smoothing is an EMA with
/// `alpha = 1/period`, not `2/(period+1)`; using the latter gives a
/// faster index that shares the name and crosses its thresholds earlier.
/// This is Wilder's, because that is what "RSI(14)" means in every
/// parameter set anyone has published.
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    previous: Option<f64>,
    avg_gain: Option<f64>,
    avg_loss: Option<f64>,
    seen: usize,
    gain_sum: f64,
    loss_sum: f64,
}

impl Rsi {
    /// # Panics
    /// A period of zero has no average to take.
    #[must_use]
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "an RSI needs a period");
        Self {
            period,
            previous: None,
            avg_gain: None,
            avg_loss: None,
            seen: 0,
            gain_sum: 0.0,
            loss_sum: 0.0,
        }
    }

    pub fn update(&mut self, price: f64) -> Option<f64> {
        let prev = self.previous.replace(price)?;
        let change = price - prev;
        let gain = change.max(0.0);
        let loss = (-change).max(0.0);

        match (self.avg_gain, self.avg_loss) {
            (Some(g), Some(l)) => {
                let n = self.period as f64;
                self.avg_gain = Some((g * (n - 1.0) + gain) / n);
                self.avg_loss = Some((l * (n - 1.0) + loss) / n);
            }
            _ => {
                self.gain_sum += gain;
                self.loss_sum += loss;
                self.seen += 1;
                if self.seen < self.period {
                    return None;
                }
                let n = self.period as f64;
                self.avg_gain = Some(self.gain_sum / n);
                self.avg_loss = Some(self.loss_sum / n);
            }
        }

        let g = self.avg_gain?;
        let l = self.avg_loss?;
        // A period with no losses is 100 by definition rather than a
        // division by zero, and the definition is the one worth encoding:
        // the index is bounded and its bound is reachable.
        if l == 0.0 {
            return Some(if g == 0.0 { 50.0 } else { 100.0 });
        }
        let rs = g / l;
        Some(100.0 - 100.0 / (1.0 + rs))
    }
}

#[cfg(test)]
mod window_stats {
    use super::Window;

    /// One sample has no dispersion, and a zero would put a band exactly
    /// on the price — signalling on every observation.
    #[test]
    fn one_sample_has_no_standard_deviation() {
        let mut w = Window::new(4);
        assert_eq!(w.std_dev(), None, "empty");
        w.push(10.0);
        assert_eq!(w.std_dev(), None, "one sample");
        w.push(10.0);
        assert_eq!(
            w.std_dev(),
            Some(0.0),
            "two identical samples do have one, and it is zero"
        );
    }

    /// The population form, checked against arithmetic done by hand.
    #[test]
    fn the_standard_deviation_is_the_population_one() {
        let mut w = Window::new(4);
        for v in [2.0, 4.0, 4.0, 4.0] {
            w.push(v);
        }
        // mean 3.5; deviations -1.5, .5, .5, .5; variance 0.75
        let sd = w.std_dev().expect("four samples");
        assert!((sd - 0.75f64.sqrt()).abs() < 1e-12, "{sd}");
    }

    /// A window that has rolled reports the window, not the history.
    #[test]
    fn extremes_follow_the_window_rather_than_everything_seen() {
        let mut w = Window::new(3);
        for v in [100.0, 1.0, 50.0] {
            w.push(v);
        }
        assert_eq!(w.extremes(), Some((1.0, 100.0)));
        // 100 rolls out.
        w.push(60.0);
        assert_eq!(w.extremes(), Some((1.0, 60.0)));
    }

    #[test]
    fn an_empty_window_has_no_extremes() {
        assert_eq!(Window::new(3).extremes(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_two_warm_ups_disagree_and_keep_disagreeing() {
        // The reason the choice is a named value. Both are called an EMA
        // and neither is wrong; a strategy fitted against one and run on
        // the other has parameters fitted to a different series. The gap
        // narrows, which is worse than staying wide: a shrinking
        // difference reads as rounding.
        let mut simple = Ema::new(10, Warmup::SimpleAverage);
        let mut first = Ema::new(10, Warmup::FirstSample);
        let series: Vec<f64> = (1..=60).map(f64::from).collect();

        let mut both = Vec::new();
        for v in &series {
            both.push((simple.update(*v), first.update(*v)));
        }
        // Early on only one of them reports at all.
        assert!(both[0].0.is_none(), "the seeded one waits for its period");
        assert!(both[0].1.is_some(), "the other reports immediately");

        let (a, b) = both[59];
        let (a, b) = (a.expect("warm"), b.expect("warm"));
        assert!(
            !close(a, b),
            "after 60 samples they still differ: {a} vs {b}"
        );
        assert!(
            (a - b).abs() < 1.0,
            "and the difference is small: {a} vs {b}"
        );
    }

    #[test]
    fn an_average_of_a_constant_is_that_constant() {
        // The property that catches an alpha applied the wrong way round,
        // which otherwise produces a curve that looks like smoothing.
        for warmup in [Warmup::SimpleAverage, Warmup::FirstSample] {
            let mut ema = Ema::new(7, warmup);
            let mut last = None;
            for _ in 0..50 {
                last = ema.update(42.0);
            }
            assert!(close(last.expect("warm"), 42.0), "{warmup:?}");
        }
        let mut sma = Sma::new(7);
        let mut last = None;
        for _ in 0..50 {
            last = sma.update(42.0);
        }
        assert!(close(last.expect("warm"), 42.0));
    }

    #[test]
    fn a_simple_average_reports_nothing_until_it_has_its_period() {
        // A partial average is an average of a different period wearing
        // this one's name, and a threshold comparison cannot tell.
        let mut sma = Sma::new(3);
        assert_eq!(sma.update(1.0), None);
        assert_eq!(sma.update(2.0), None);
        assert_eq!(sma.update(3.0), Some(2.0));
        assert_eq!(sma.update(4.0), Some(3.0));
    }

    #[test]
    fn the_running_sum_matches_a_recomputed_one() {
        // The optimisation that pays for itself and could silently drift:
        // a displaced sample subtracted wrongly accumulates error that no
        // single value looks wrong for.
        let mut sma = Sma::new(5);
        let series = [3.0, -1.0, 7.5, 2.25, 0.0, 9.0, -4.5, 6.0, 1.0, 8.0];
        for (i, v) in series.iter().enumerate() {
            let got = sma.update(*v);
            if i + 1 >= 5 {
                let window = &series[i + 1 - 5..=i];
                let expected: f64 = window.iter().sum::<f64>() / 5.0;
                assert!(close(got.expect("warm"), expected), "at {i}");
            }
        }
    }

    #[test]
    fn the_macd_signal_smooths_the_macd_line_and_not_the_price() {
        // The error that produces a plausible curve crossing at the wrong
        // moments. On a constant price the MACD line is zero, so the
        // signal must be zero too — a signal fed the price would sit at
        // the price.
        let mut macd = Macd::new(12, 26, 9, Warmup::SimpleAverage);
        let mut last = None;
        for _ in 0..200 {
            last = macd.update(100.0);
        }
        let v = last.expect("warm");
        assert!(close(v.macd, 0.0), "macd {}", v.macd);
        assert!(close(v.signal, 0.0), "signal {}", v.signal);
        assert!(close(v.histogram, 0.0), "histogram {}", v.histogram);
    }

    #[test]
    fn the_macd_reports_nothing_until_all_three_averages_are_warm() {
        let mut macd = Macd::new(12, 26, 9, Warmup::SimpleAverage);
        let mut first_at = None;
        for i in 1..=100 {
            if macd.update(f64::from(i)).is_some() {
                first_at = Some(i);
                break;
            }
        }
        // The slow EMA needs 26 samples before the MACD line exists, and
        // the signal needs 9 MACD values after that.
        assert_eq!(first_at, Some(34), "26 + 9 - 1");
    }

    #[test]
    fn a_rising_series_takes_the_rsi_to_a_hundred_and_it_stays_bounded() {
        // A period with no losses is 100 by definition rather than a
        // division by zero, and the bound is reachable rather than
        // asymptotic.
        let mut rsi = Rsi::new(14);
        let mut last = None;
        for i in 1..=40 {
            last = rsi.update(f64::from(i));
        }
        assert!(close(last.expect("warm"), 100.0), "{last:?}");
    }

    #[test]
    fn a_falling_series_takes_the_rsi_to_zero() {
        let mut rsi = Rsi::new(14);
        let mut last = None;
        for i in (1..=40).rev() {
            last = rsi.update(f64::from(i));
        }
        assert!(close(last.expect("warm"), 0.0), "{last:?}");
    }

    #[test]
    fn an_unchanging_price_is_neither_overbought_nor_oversold() {
        // Zero gains and zero losses is the case that divides by zero if
        // the definition is not encoded.
        let mut rsi = Rsi::new(14);
        let mut last = None;
        for _ in 0..40 {
            last = rsi.update(50.0);
        }
        assert!(close(last.expect("warm"), 50.0), "{last:?}");
    }

    #[test]
    fn wilders_smoothing_is_slower_than_the_other_alpha() {
        // The RSI convention trap, made visible. Wilder's alpha is 1/n;
        // the EMA alpha 2/(n+1) gives a faster index that shares the name
        // and crosses its thresholds earlier. This asserts the one here is
        // the slower of the two.
        let mut wilder = Rsi::new(14);
        let mut value = None;
        // Twenty rises then one sharp fall: a faster index drops further.
        for i in 1..=20 {
            value = wilder.update(f64::from(i));
        }
        let before = value.expect("warm");
        let after = wilder.update(10.0).expect("warm");
        assert!(before > after, "a fall lowers it: {before} -> {after}");
        assert!(
            after > 50.0,
            "but Wilder's smoothing keeps it high after one fall: {after}"
        );
    }

    #[test]
    fn a_window_displaces_in_order_and_reports_when_full() {
        let mut w = Window::new(3);
        assert_eq!(w.push(1.0), None);
        assert_eq!(w.push(2.0), None);
        assert!(!w.is_full());
        assert_eq!(w.push(3.0), None);
        assert!(w.is_full());
        assert_eq!(w.push(4.0), Some(1.0), "oldest out first");
        assert_eq!(w.push(5.0), Some(2.0));
        assert!(close(w.mean().expect("full"), 4.0));
    }

    #[test]
    #[should_panic(expected = "at least one sample")]
    fn a_window_of_zero_is_refused_rather_than_silently_useless() {
        let _ = Window::new(0);
    }
}
