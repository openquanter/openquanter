//! Wilder's RSI, used the way every platform's default template uses it.
//!
//! **A teaching reference, not a recommendation.** See the module
//! documentation in `classics/mod.rs`.
//!
//! Buy when the oscillator falls below 30, close when it rises back
//! above 50. The parameters are Wilder's published ones — period 14,
//! bands at 30 and 70 — and are not tuned here, so nothing about the
//! result is a fit.
//!
//! # The assumption
//!
//! That an extreme reading reverts. It is the oldest bet in technical
//! analysis and it is a bet on the market being range-bound.
//!
//! # Where it breaks
//!
//! An oscillator reads "oversold" the entire way down a trend. The
//! reading that triggers the entry is the same reading a collapse
//! produces, and the strategy cannot tell them apart — which is why this
//! one is worth running with the margin overlay switched on and the
//! margin-free arm beside it.

use oq_backtest::strategy::indicator::Rsi;
use oq_types::{Fill, OrderId, QtyLots, Side};

use crate::classics::helpers::Trader;

/// Buy oversold, close when the oscillator recovers.
#[derive(Debug, Clone)]
pub struct RsiReversion {
    rsi: Rsi,
    /// Below this, the strategy calls the market oversold.
    enter_below: f64,
    /// Above this, it considers the reversion done.
    exit_above: f64,
    trader: Trader,
}

impl Default for RsiReversion {
    fn default() -> Self {
        Self::new()
    }
}

impl RsiReversion {
    /// Wilder's parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rsi: Rsi::new(14),
            enter_below: 30.0,
            exit_above: 50.0,
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for RsiReversion {
    fn name(&self) -> &str {
        "rsi-reversion"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let Some(value) = self.rsi.update(ctx.tick.last.0 as f64) else {
            // Warming up. Not a signal of "neutral" — the indicator has
            // no reading at all, and treating that as neutral is how a
            // strategy trades on the first observation it happens to see.
            return;
        };
        if ctx.position.0 == 0 {
            if value < self.enter_below {
                self.trader.open(ctx, out, Side::Buy, QtyLots(1));
            }
        } else if value > self.exit_above {
            self.trader.close(ctx, out, Side::Sell, ctx.position);
        }
    }

    fn on_fill(&mut self, _f: &Fill, _c: &oq_backtest::Context, _o: &mut Vec<oq_backtest::Intent>) {
    }
}

/// The ids this strategy has used, for a caller that wants them.
impl RsiReversion {
    /// Last order id issued.
    #[must_use]
    pub const fn last_id(&self) -> OrderId {
        self.trader.last_id()
    }
}
