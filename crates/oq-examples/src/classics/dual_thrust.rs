//! Dual Thrust — Michael Chalek, and the default template in a great
//! many Chinese futures systems.
//!
//! **A teaching reference, not a recommendation.**
//!
//! Measure a range over the last N observations, then buy when price
//! exceeds the period's open by a fraction of that range and sell when
//! it falls below by the same fraction. `k` is the published 0.5.
//!
//! # Where it breaks
//!
//! The range it measures is the *previous* period's, so a regime change
//! is priced one period late — the strategy sizes its trigger from a
//! market that has already stopped existing. In a quiet period followed
//! by a violent one, the trigger is far too close and it enters on
//! noise; in the reverse, far too wide and it never enters at all.

use oq_backtest::strategy::indicator::Window;
use oq_types::{QtyLots, Side};

use crate::classics::helpers::Trader;

/// Break a fraction of the recent range, in either direction.
#[derive(Debug, Clone)]
pub struct DualThrust {
    window: Window,
    /// Fraction of the range that makes a trigger.
    k: f64,
    /// Where the current period opened.
    open: Option<f64>,
    /// Observations into the current period.
    seen: usize,
    /// Observations per period.
    period: usize,
    trader: Trader,
}

impl Default for DualThrust {
    fn default() -> Self {
        Self::new()
    }
}

impl DualThrust {
    /// Chalek's `k`, over a period of sixty observations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: Window::new(60),
            k: 0.5,
            open: None,
            seen: 0,
            period: 60,
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for DualThrust {
    fn name(&self) -> &str {
        "dual-thrust"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let price = ctx.tick.last.0 as f64;

        // The range from the period that has *finished*, captured before
        // this observation joins the window. Reading it afterwards would
        // let the current move set its own trigger, which is the mistake
        // that makes any breakout system untradeable and profitable in a
        // backtest at the same time.
        let previous = self.window.extremes();
        self.window.push(price);

        self.seen += 1;
        if self.seen > self.period || self.open.is_none() {
            self.open = Some(price);
            self.seen = 1;
        }

        let (Some(open), Some((low, high))) = (self.open, previous) else {
            return;
        };
        if !self.window.is_full() {
            return;
        }
        let trigger = self.k * (high - low);
        if trigger <= 0.0 {
            // A period with no range gives no trigger. Entering on zero
            // would enter on the first observation of every flat
            // stretch.
            return;
        }

        if ctx.position.0 == 0 {
            if price > open + trigger {
                self.trader.open(ctx, out, Side::Buy, QtyLots(1));
            }
        } else if price < open - trigger {
            self.trader.close(ctx, out, Side::Sell, ctx.position);
        }
    }
}
