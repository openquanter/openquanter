//! The strategy whose backtest is systematically fake without a margin
//! model.
//!
//! It lives in the library rather than in one example because two
//! examples need it: `martingale_ladder` shows what it does in a single
//! window, and `margin_fidelity` runs it over forty. A strategy copied
//! into both would eventually stop being the same strategy, and the
//! second example's whole claim is that it is running the first one.

use oq_backtest::{Context, Intent, Strategy};
use oq_types::{Fill, OrderId, QtyLots, Side};

/// Buys a first lot, then doubles down every time price falls another
/// step below the average entry.
pub struct MartingaleLadder {
    /// Fraction below the average entry at which the next rung sits.
    step: f64,
    /// Lots for the first rung; each subsequent rung doubles.
    base_qty: i64,
    /// Rungs already filled.
    rungs: u32,
    /// Cap, because a ladder without one is a countdown rather than a
    /// strategy. It still is not enough, which is the point.
    max_rungs: u32,
    next_id: u64,
}

impl Default for MartingaleLadder {
    fn default() -> Self {
        Self::new()
    }
}

impl MartingaleLadder {
    pub fn new() -> Self {
        Self {
            step: 0.04,
            base_qty: 4,
            rungs: 0,
            max_rungs: 6,
            next_id: 1,
        }
    }

    pub fn id(&mut self) -> OrderId {
        let id = OrderId::new(self.next_id);
        self.next_id += 1;
        id
    }
}

impl Strategy for MartingaleLadder {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if self.rungs == 0 {
            self.rungs = 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(self.base_qty),
                offset: oq_types::Offset::Open,
            });
            return;
        }

        if self.rungs >= self.max_rungs || ctx.position.0 <= 0 {
            return;
        }

        // Next rung: another `step` below the current average entry.
        #[allow(clippy::cast_possible_truncation)]
        let trigger = (f64::from(i32::try_from(ctx.entry.0).unwrap_or(i32::MAX))
            * (1.0 - self.step * f64::from(self.rungs))) as i64;

        if ctx.tick.low.0 <= trigger {
            let qty = self.base_qty * (1 << self.rungs);
            self.rungs += 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(qty),
                offset: oq_types::Offset::Open,
            });
        }
    }

    fn on_fill(&mut self, _fill: &Fill, _ctx: &Context, _out: &mut Vec<Intent>) {
        // Position management would live here. This ladder deliberately
        // has no exit: the lesson is about what happens when the market
        // does not come back in time, not about exit design.
    }

    fn name(&self) -> &str {
        "martingale-ladder"
    }
}
