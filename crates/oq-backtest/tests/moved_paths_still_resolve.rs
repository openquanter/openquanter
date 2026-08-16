//! The strategy contract moved out of this crate, and nothing that
//! referred to it had to change.
//!
//! `Context`, `Intent` and `Strategy` are defined in `oq-strategy` now.
//! Every path that reached them through this crate before the move
//! still reaches them, because a move that breaks its callers is a
//! rename with extra steps — and the reason for moving was to make the
//! same strategy runnable by a second host, which is not served by
//! making every existing strategy edit its imports first.
//!
//! This file is mostly a compile-time assertion. If a re-export is
//! dropped, it stops building rather than stops passing.

use oq_backtest::strategy::{Context, Intent, Strategy};
use oq_backtest::{Context as ShortContext, Intent as ShortIntent, Strategy as ShortStrategy};
use oq_types::{OrderId, QtyLots, Side};

/// A strategy written against the pre-move paths.
struct Legacy;

impl Strategy for Legacy {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        out.push(Intent::market(OrderId(1), Side::Buy, QtyLots(1)));
    }

    fn name(&self) -> &str {
        "legacy"
    }
}

/// The short paths and the module paths are the same types, not two
/// parallel definitions that happen to look alike.
fn _same_type(long: Intent, short: ShortIntent) -> [Intent; 2] {
    [long, short]
}

fn _short_context(_: &ShortContext) {}
fn _short_trait<S: ShortStrategy>() {}

#[test]
fn a_strategy_written_before_the_move_still_compiles_and_runs() {
    let mut s = Legacy;
    let mut out = Vec::new();
    let ctx = Context {
        tick: oq_engine::Tick::default(),
        position: QtyLots(0),
        entry: oq_types::PriceTicks(0),
        short_position: QtyLots(0),
        short_entry: oq_types::PriceTicks(0),
        equity: oq_types::Cash(0),
        working: 0,
    };
    s.on_tick(&ctx, &mut out);
    assert_eq!(out.len(), 1, "the old paths still drive the old strategy");
}
