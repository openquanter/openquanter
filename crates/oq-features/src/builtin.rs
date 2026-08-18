//! The one feature definition the skeleton ships with.
//!
//! One, not a library of them. The point of this crate at M2 is the
//! shape — a definition, two paths derived from it, and a metric — and a
//! second feature would demonstrate nothing the first does not. The
//! feature layer proper is M5.
//!
//! `MidReturn` is chosen because it is the smallest feature that has
//! every property worth testing: it warms up (there is no return on the
//! first tick), it depends on order (shuffle the ticks and it changes),
//! it is undefined when the book is empty, and its offline form is the
//! one people vectorise first.

use oq_engine::Tick;
use oq_strategy::indicator::Ema;

use crate::Feature;

/// The log return of the mid price, smoothed over `period` ticks.
///
/// Undefined until there are two mids to take a return between, and
/// undefined for any tick with no book: a one-sided or empty book has no
/// mid, and inventing one from `last` would make the feature quietly
/// different in exactly the conditions — thin markets — where it matters
/// most.
#[derive(Debug, Clone)]
pub struct MidReturn {
    ema: Ema,
    previous: Option<f64>,
    name: String,
}

impl MidReturn {
    /// A smoothed mid return over `period` ticks.
    ///
    /// # Panics
    ///
    /// If `period` is zero, via [`Ema::new`].
    #[must_use]
    pub fn new(period: usize) -> Self {
        Self {
            ema: Ema::new(period, oq_strategy::indicator::Warmup::SimpleAverage),
            previous: None,
            name: format!("mid_return_{period}"),
        }
    }

    /// The mid price of a tick, or `None` when the book cannot give one.
    #[must_use]
    pub fn mid(tick: &Tick) -> Option<f64> {
        if tick.bid.0 <= 0 || tick.ask.0 <= 0 {
            return None;
        }
        Some((tick.bid.0 as f64 + tick.ask.0 as f64) / 2.0)
    }
}

impl Feature for MidReturn {
    fn name(&self) -> &str {
        &self.name
    }

    fn update(&mut self, tick: &Tick) -> Option<f64> {
        let mid = Self::mid(tick)?;
        let previous = self.previous.replace(mid)?;
        if previous <= 0.0 {
            return None;
        }
        self.ema.update((mid / previous).ln())
    }
}
