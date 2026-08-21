//! Appel's MACD, traded on the signal-line crossing.
//!
//! **A teaching reference, not a recommendation.**
//!
//! Long while the MACD line is above its signal line, flat otherwise.
//! Parameters are the published 12/26/9 and are not tuned here.
//!
//! # Where it breaks
//!
//! It is two moving averages, so it is late by construction. In a
//! sideways market the lateness costs a round trip per oscillation, and
//! the fidelity report's maker/taker split will show every one of them
//! paying the spread.

use oq_backtest::strategy::indicator::{Macd, Warmup};
use oq_types::{QtyLots, Side};

use crate::classics::helpers::Trader;

/// Long while MACD is above its signal line.
#[derive(Debug, Clone)]
pub struct MacdTrend {
    macd: Macd,
    trader: Trader,
}

impl Default for MacdTrend {
    fn default() -> Self {
        Self::new()
    }
}

impl MacdTrend {
    /// Appel's parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            macd: Macd::new(12, 26, 9, Warmup::SimpleAverage),
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for MacdTrend {
    fn name(&self) -> &str {
        "macd-trend"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let Some(v) = self.macd.update(ctx.tick.last.0 as f64) else {
            return;
        };
        let bullish = v.macd > v.signal;
        if bullish && ctx.position.0 == 0 {
            self.trader.open(ctx, out, Side::Buy, QtyLots(1));
        } else if !bullish && ctx.position.0 > 0 {
            self.trader.close(ctx, out, Side::Sell, ctx.position);
        }
    }
}
