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

use oq_types::{Fill, OrderId, QtyLots, Side};

use crate::classics::helpers::Trader;

/// A ladder of rungs, a step apart.
#[derive(Debug, Clone)]
pub struct GridTrader {
    /// Fraction between rungs.
    step: f64,
    /// Rungs the strategy will hold at once.
    max_rungs: u32,
    /// Where the last rung was filled, in ticks.
    ///
    /// Set from a **fill**, never from the tick that produced the order.
    /// See the note on `pending`.
    last_fill: Option<f64>,
    rungs: u32,
    /// The rung that has been asked for and not yet answered.
    ///
    /// A grid is the one strategy in this catalogue that carries state
    /// derived from its own orders — where the ladder is anchored and
    /// how many rungs it believes it holds. That makes it the one where
    /// assuming a submission became a position is an actual defect
    /// rather than a stylistic one: refuse a rung (a risk limit, a
    /// margin shortfall, a venue `-2019`) and an optimistic grid still
    /// advances the ladder and re-anchors, so it stops placing rungs it
    /// thinks it already owns and quietly does nothing for the rest of
    /// the run. The other five strategies here re-derive their
    /// conditions from `ctx.position` every tick and simply retry.
    ///
    /// So the ladder moves on fills, and this holds the gap between
    /// asking and finding out. One at a time: without that, a grid
    /// ticking faster than the venue answers stacks a rung per tick.
    pending: Option<OrderId>,
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
            pending: None,
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
        // A rung is outstanding. Placing another would ladder on the
        // same signal, since the condition that produced the first is
        // still true until it fills.
        if self.pending.is_some() {
            return;
        }
        let price = ctx.tick.last.0 as f64;
        let Some(anchor) = self.last_fill else {
            // The first rung anchors the grid. Placing it on the first
            // observation means the grid is centred wherever the run
            // happened to start, which is true of every grid anybody
            // runs and is worth knowing about the results.
            self.trader.open(ctx, out, Side::Buy, QtyLots(1));
            self.pending = Some(self.trader.last_id());
            return;
        };

        if price <= anchor * (1.0 - self.step) && self.rungs < self.max_rungs {
            self.trader.open(ctx, out, Side::Buy, QtyLots(1));
            self.pending = Some(self.trader.last_id());
        } else if price >= anchor * (1.0 + self.step) && ctx.position.0 > 0 {
            self.trader.close(ctx, out, Side::Sell, QtyLots(1));
            self.pending = Some(self.trader.last_id());
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_fill(&mut self, f: &Fill, _c: &oq_backtest::Context, _o: &mut Vec<oq_backtest::Intent>) {
        // The ladder moves here, on the price that was actually paid.
        // Anchoring on the tick that produced the order would put the
        // next rung a step from a price nobody traded at, and every
        // rung's worth of slippage would compound into the grid's
        // geometry rather than showing up as a cost.
        self.last_fill = Some(f.price.0 as f64);
        match f.side {
            Side::Buy => self.rungs += 1,
            Side::Sell => self.rungs = self.rungs.saturating_sub(1),
        }
        if self.pending == Some(f.order) {
            self.pending = None;
        }
    }

    /// Note what this does *not* handle: a submission nobody got an
    /// answer to. Those never reach here — the host does not report an
    /// unresolved placement as a refusal — so `pending` stays set and
    /// the grid stops placing rungs until the fill arrives or the run
    /// ends.
    ///
    /// That stall is the intended outcome. A grid that stopped is a
    /// grid an operator can look at; a grid that assumed an unanswered
    /// rung was dead and placed another is holding twice what it thinks
    /// it holds, and it will keep compounding that on every rung.
    fn on_placed(&mut self, id: OrderId, accepted: bool) {
        // Refused. Leave the ladder exactly where it was, so the same
        // condition places the same rung on the next tick — which is
        // what the other strategies in this catalogue get for free by
        // holding no placement state at all.
        if !accepted && self.pending == Some(id) {
            self.pending = None;
        }
    }
}
