//! Grid trading — the default retail strategy in crypto.
//!
//! **A teaching reference, not a recommendation.** This is the one in
//! the catalogue most worth running with the margin overlay on, and the
//! reason is below.
//!
//! Buy a rung every time price falls a step below the last entry; sell a
//! rung every time it rises a step above. No author, no publication —
//! it is what every exchange's built-in bot does.
//!
//! # Where it breaks, and why it matters here
//!
//! It is short volatility with no stop. Every rung is profitable until
//! the range breaks, and then the accumulated position is on the wrong
//! side of a trend with more size than any single decision ever
//! approved. Its equity curve is a staircase up and one cliff.
//!
//! That shape is exactly what a margin-free backtest reports as a
//! success: the position rides through the hole and books the recovery.
//! `examples/margin_fidelity` makes the same argument with a martingale
//! ladder; this one is the version people actually run.

use oq_types::{Fill, QtyLots, Side};

use crate::classics::helpers::Trader;

/// A ladder of rungs, a step apart.
#[derive(Debug, Clone)]
pub struct GridTrader {
    /// Fraction between rungs.
    step: f64,
    /// Rungs the strategy will hold at once.
    max_rungs: u32,
    /// Where the last rung was filled, in ticks.
    last_fill: Option<f64>,
    rungs: u32,
    trader: Trader,
}

impl Default for GridTrader {
    fn default() -> Self {
        Self::new()
    }
}

impl GridTrader {
    /// A half-percent grid, capped at eight rungs.
    ///
    /// The cap is not risk management and should not be mistaken for it.
    /// It bounds how fast the position grows and does nothing about the
    /// direction — which is the part that ends the account.
    #[must_use]
    pub fn new() -> Self {
        Self {
            step: 0.005,
            max_rungs: 8,
            last_fill: None,
            rungs: 0,
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for GridTrader {
    fn name(&self) -> &str {
        "grid"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let price = ctx.tick.last.0 as f64;
        let Some(anchor) = self.last_fill else {
            // The first rung anchors the grid. Placing it on the first
            // observation means the grid is centred wherever the run
            // happened to start, which is true of every grid anybody
            // runs and is worth knowing about the results.
            self.trader.open(out, Side::Buy, QtyLots(1));
            self.last_fill = Some(price);
            self.rungs = 1;
            return;
        };

        if price <= anchor * (1.0 - self.step) && self.rungs < self.max_rungs {
            self.trader.open(out, Side::Buy, QtyLots(1));
            self.last_fill = Some(price);
            self.rungs += 1;
        } else if price >= anchor * (1.0 + self.step) && ctx.position.0 > 0 {
            self.trader.close(out, Side::Sell, QtyLots(1));
            self.last_fill = Some(price);
            self.rungs = self.rungs.saturating_sub(1);
        }
    }

    fn on_fill(&mut self, _f: &Fill, _c: &oq_backtest::Context, _o: &mut Vec<oq_backtest::Intent>) {
    }
}
