//! Donchian channels — the rule the Turtles were taught.
//!
//! **A teaching reference, not a recommendation.**
//!
//! Enter long on a new twenty-period high, exit on a ten-period low.
//! The 1983 Turtle rules used 20 in and 10 out for the faster system,
//! and those are the numbers here.
//!
//! # Where it breaks
//!
//! Most breakouts fail. The system depends entirely on the few that do
//! not, which makes its result a property of a handful of trades and
//! almost nothing else — and that is exactly the shape a deflated Sharpe
//! ratio is designed to deflate. Run it through a sweep and read the
//! statistic rather than the equity curve.

use oq_backtest::strategy::indicator::Window;
use oq_types::{QtyLots, Side};

use crate::classics::helpers::Trader;

/// Enter on a new high, leave on a lower low.
#[derive(Debug, Clone)]
pub struct DonchianBreakout {
    entry: Window,
    exit: Window,
    trader: Trader,
}

impl Default for DonchianBreakout {
    fn default() -> Self {
        Self::new()
    }
}

impl DonchianBreakout {
    /// The Turtles' faster system.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entry: Window::new(20),
            exit: Window::new(10),
            trader: Trader::new(),
        }
    }
}

impl oq_backtest::Strategy for DonchianBreakout {
    fn name(&self) -> &str {
        "donchian-breakout"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &oq_backtest::Context, out: &mut Vec<oq_backtest::Intent>) {
        let price = ctx.tick.last.0 as f64;
        // Read the channel *before* this observation joins it. A
        // breakout compared against a window that already contains the
        // breaking price can never trigger — the new high is its own
        // high — and the strategy would simply never trade.
        let channel = self.entry.extremes();
        let stop = self.exit.extremes();
        self.entry.push(price);
        self.exit.push(price);
        if !self.entry.is_full() {
            return;
        }

        match (ctx.position.0, channel, stop) {
            (0, Some((_, high)), _) if price > high => {
                self.trader.open(out, Side::Buy, QtyLots(1));
            }
            (held, _, Some((low, _))) if held > 0 && price < low => {
                self.trader.close(out, Side::Sell, ctx.position);
            }
            _ => {}
        }
    }
}
