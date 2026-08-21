//! Bollinger's bands, traded as a reversion.
//!
//! **A teaching reference, not a recommendation.**
//!
//! Buy when price closes below the lower band, close when it returns to
//! the middle. Twenty periods and two standard deviations — Bollinger's
//! published defaults, untuned here.
//!
//! # Where it breaks
//!
//! The bands widen *after* volatility arrives, because the standard
//! deviation is backward-looking. So the entry that matters most — the
//! one taken as a move begins — is taken against a band that has not
//! adjusted yet, and is therefore taken too early by exactly as much as
//! the move is large.

use oq_backtest::strategy::indicator::Window;
use oq_types::{QtyLots, Side};

use crate::classics::helpers::Trader;

/// Buy below the lower band, close at the middle.
#[derive(Debug, Clone)]
pub struct BollingerReversion {
    window: Window,
    /// Band width in standard deviations.
    width: f64,
    trader: Trader,
}

impl Default for BollingerReversion {
    fn default() -> Self {
        Self::new()
    }
}

impl BollingerReversion {
    /// Bollinger's defaults: twenty periods, two deviations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: Window::new(20),
            width: 2.0,
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for BollingerReversion {
    fn name(&self) -> &str {
        "bollinger-reversion"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let price = ctx.tick.last.0 as f64;
        self.window.push(price);
        if !self.window.is_full() {
            return;
        }
        // Both or neither. A band built from a mean without its
        // deviation would be the mean wearing a band's name.
        let (Some(mean), Some(sd)) = (self.window.mean(), self.window.std_dev()) else {
            return;
        };
        let lower = mean - self.width * sd;

        if ctx.position.0 == 0 {
            if price < lower {
                self.trader.open(ctx, out, Side::Buy, QtyLots(1));
            }
        } else if price >= mean {
            self.trader.close(ctx, out, Side::Sell, ctx.position);
        }
    }
}
