//! Teaching examples, and the synthetic market they run against.
//!
//! **Nothing here is a trading strategy you should run.** Every example
//! is written to demonstrate one property of the framework, and each is
//! expected to lose money. An example tuned to show a pleasant equity
//! curve would teach the wrong lesson from a project whose entire claim
//! is that backtests flatter you.
//!
//! ## Why the data is synthetic
//!
//! The sample market is generated from a seed rather than captured from
//! a venue, for three reasons:
//!
//! 1. **Redistribution.** Exchange market data comes with terms; a
//!    public repository that ships it cannot take it back.
//! 2. **Determinism.** A generated series is identical on every machine
//!    forever, which is what golden tests need and what makes an
//!    example's printed numbers quotable in documentation.
//! 3. **Control.** A crash of exactly the depth needed can be scripted.
//!    Waiting for real data to contain the case you want to teach is a
//!    bad way to build a lesson.
//!
//! Real captures are the point of `oq-l2feed`; see
//! `docs/CAPTURE-FORMAT.md` for pointing the engine at one.

use oq_engine::Tick;
use oq_types::Stamp;

/// Deterministic generator.
///
/// SplitMix64, not a bare linear congruential generator: an LCG
/// consumed at a fixed stride leaks lattice structure into whatever is
/// generated in a loop, which is how synthetic "noise" acquires
/// structure nobody intended.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn uniform(&mut self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Standard normal, by Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(f64::MIN_POSITIVE);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
    }
}

/// Milliseconds between generated observations.
pub const INTERVAL_MS: i64 = 250;

/// Format cash for a human.
///
/// The ledger is fixed-point integers, which is right for arithmetic
/// and unreadable in output. An example whose numbers cannot be read is
/// not an example.
#[must_use]
pub fn money(cash: oq_types::Cash) -> String {
    #[allow(clippy::cast_precision_loss)]
    {
        format!("{:>12.2}", cash.0 as f64 / oq_types::CASH_SCALE as f64)
    }
}

/// Format a price in ticks for a human.
#[must_use]
pub fn price(ticks: oq_types::PriceTicks) -> String {
    #[allow(clippy::cast_precision_loss)]
    {
        format!("{:.2}", ticks.0 as f64 / 100.0)
    }
}

/// How a generated series behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarketShape {
    /// Starting price in ticks.
    pub start: i64,
    /// Per-observation drift, as a fraction.
    pub drift: f64,
    /// Per-observation volatility, as a fraction.
    pub volatility: f64,
    /// Observations to generate.
    pub ticks: usize,
}

impl MarketShape {
    /// A market that goes sideways: what most of history looks like.
    #[must_use]
    pub const fn calm(ticks: usize) -> Self {
        Self {
            start: 6_000_000,
            drift: 0.0,
            volatility: 0.000_35,
            ticks,
        }
    }

    /// A market that grinds upward, so a trend follower has something
    /// to follow.
    #[must_use]
    pub const fn trending(ticks: usize) -> Self {
        Self {
            start: 6_000_000,
            drift: 0.000_06,
            volatility: 0.000_30,
            ticks,
        }
    }
}

/// Generate a price series.
#[must_use]
pub fn series(shape: MarketShape, seed: u64) -> Vec<Tick> {
    let mut rng = Rng::new(seed);
    let mut price = shape.start as f64;
    let mut out = Vec::with_capacity(shape.ticks);
    let mut volume = 0i64;

    for i in 0..shape.ticks {
        let step = shape.drift + shape.volatility * rng.normal();
        let open = price;
        price *= 1.0 + step;
        volume += 10 + (rng.uniform() * 90.0) as i64;
        out.push(observation(i, open, price, &mut rng, volume));
    }

    out
}

/// A calm market interrupted by a crash, then a partial recovery.
///
/// The shape that separates an honest backtest from a flattering one.
/// A strategy that adds to a losing position survives the first two
/// phases and is decided by the third, which is exactly the case a
/// margin-free simulation gets wrong.
///
/// `depth` is the fraction the price falls during the crash, e.g. `0.5`.
#[must_use]
pub fn crash_series(seed: u64, calm_ticks: usize, crash_ticks: usize, depth: f64) -> Vec<Tick> {
    let mut rng = Rng::new(seed);
    let shape = MarketShape::calm(calm_ticks);
    let mut price = shape.start as f64;
    let mut out = Vec::with_capacity(calm_ticks + crash_ticks * 2);
    let mut volume = 0i64;
    let mut index = 0usize;

    for _ in 0..calm_ticks {
        let open = price;
        price *= 1.0 + shape.volatility * rng.normal();
        volume += 10 + (rng.uniform() * 90.0) as i64;
        out.push(observation(index, open, price, &mut rng, volume));
        index += 1;
    }

    // The crash: a steady decline, not a single gap, so the ladder gets
    // filled on the way down exactly as it would in a real one.
    let per_step = (1.0 - depth).powf(1.0 / crash_ticks as f64);
    for _ in 0..crash_ticks {
        let open = price;
        price *= per_step * (1.0 + shape.volatility * rng.normal());
        volume += 50 + (rng.uniform() * 200.0) as i64;
        out.push(observation(index, open, price, &mut rng, volume));
        index += 1;
    }

    // The recovery: enough to make a margin-free run look like it was
    // merely a bad afternoon.
    let per_step = (1.0 / (1.0 - depth * 0.8)).powf(1.0 / crash_ticks as f64);
    for _ in 0..crash_ticks {
        let open = price;
        price *= per_step * (1.0 + shape.volatility * rng.normal());
        volume += 30 + (rng.uniform() * 120.0) as i64;
        out.push(observation(index, open, price, &mut rng, volume));
        index += 1;
    }

    out
}

fn observation(index: usize, open: f64, close: f64, rng: &mut Rng, volume: i64) -> Tick {
    let wick = (close - open).abs().mul_add(0.5, close * 0.000_1);
    #[allow(clippy::cast_possible_truncation)]
    let high = (open.max(close) + wick * rng.uniform()) as i64;
    #[allow(clippy::cast_possible_truncation)]
    let low = (open.min(close) - wick * rng.uniform()) as i64;
    #[allow(clippy::cast_possible_truncation)]
    let last = close as i64;

    let at = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1) * INTERVAL_MS * 1_000_000;

    Tick::trades_only(Stamp::synthetic(at), last, high.max(last), low.min(last)).with_volume(volume)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_is_identical_on_every_run() {
        // The property the examples' documented numbers depend on.
        let a = series(MarketShape::calm(500), 42);
        let b = series(MarketShape::calm(500), 42);
        assert_eq!(a, b);
        assert_ne!(a, series(MarketShape::calm(500), 43));
    }

    #[test]
    fn observations_are_internally_consistent() {
        for tick in series(MarketShape::trending(2_000), 7) {
            assert!(tick.high.0 >= tick.last.0, "high must cover last");
            assert!(tick.low.0 <= tick.last.0, "low must cover last");
            assert!(tick.last.0 > 0, "price must stay positive");
        }
    }

    #[test]
    fn volume_only_ever_increases() {
        // A venue's traded-volume field is an accumulator; a generator
        // that let it fall would let a strategy see a volume delta that
        // cannot happen.
        let ticks = series(MarketShape::calm(1_000), 3);
        for pair in ticks.windows(2) {
            assert!(pair[1].volume_since(&pair[0]).0 >= 0);
        }
    }

    #[test]
    fn the_crash_series_actually_crashes_and_recovers() {
        let ticks = crash_series(11, 400, 200, 0.5);
        let start = ticks[0].last.0;
        let bottom = ticks.iter().map(|t| t.last.0).min().expect("non-empty");
        let end = ticks.last().expect("non-empty").last.0;

        assert!(
            (bottom as f64) < start as f64 * 0.6,
            "the crash must be deep enough to matter: {start} -> {bottom}"
        );
        assert!(
            end > bottom * 3 / 2,
            "the recovery must be enough to flatter a margin-free run"
        );
    }
}
