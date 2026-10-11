//! Remaining quote cost, independent of the displayed entry tick.
//!
//! Partial-close P&L is truncated only at cash precision. The corresponding
//! cost allocation retains the residual in the remaining position so that
//! final flattening conserves the entire entry cost.

use oq_margin::Contract;
use oq_types::{Cash, PriceTicks, QtyLots};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct EntryCost {
    qty: QtyLots,
    entry: PriceTicks,
    value: i128,
}

impl EntryCost {
    /// Public holding fields may be seeded by adoption or older callers.
    /// Such a seed has only its reported entry precision; do not pretend it
    /// carries more evidence. Otherwise retain the cost learned from fills.
    pub fn value(self, contract: Contract, qty: QtyLots, entry: PriceTicks) -> i128 {
        if qty.is_zero() {
            0
        } else if qty == self.qty && entry == self.entry {
            self.value
        } else {
            contract.notional_wide(entry, qty)
        }
    }

    pub fn set(&mut self, qty: QtyLots, entry: PriceTicks, value: i128) {
        *self = Self { qty, entry, value };
    }
}

/// Allocate a close without rounding average entry to price ticks.
///
/// Divide before multiplying: a cost already uses the wide range, whereas
/// the remainder times quantity fits in i128 for i64 quantities.
pub(crate) fn close(cost: i128, held: QtyLots, closing: i64, exit: i128) -> (i128, Cash) {
    let size = i128::from(held.0.saturating_abs());
    let qty = i128::from(closing);
    let remainder = (cost % size) * qty;
    let allocated = (cost / size)
        .saturating_mul(qty)
        .saturating_add(remainder / size);
    let sign = if held.0 > 0 { 1_i128 } else { -1_i128 };
    let mut pnl = exit.saturating_sub(allocated).saturating_mul(sign);
    // The omitted positive fraction of cost shifts a positive long profit
    // down, or a negative short loss up, when truncated toward zero.
    if remainder % size != 0 && (pnl > 0 && sign > 0 || pnl < 0 && sign < 0) {
        pnl -= sign;
    }
    let consumed = exit.saturating_sub(pnl.saturating_mul(sign));
    let left = if closing == held.0.saturating_abs() {
        0
    } else {
        cost.saturating_sub(consumed)
    };
    let cash = Cash(pnl.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64);
    (left, cash)
}
