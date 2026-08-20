//! Observing an account over time, rather than gating on it once.
//!
//! [`crate::reconcile`] answers "does this match?", which is the right
//! question for a startup gate: one answer, and a wrong one stops the
//! system. It is the wrong question for observation. A shadow run exists
//! to collect evidence about the failure modes a backtest cannot reach —
//! a fill delivered twice, a cancel that raced an execution, a position
//! moved by something nobody sent — and those are visible only as
//! *changes over time*. A tool that stops at the first difference sees
//! one of them and then nothing.
//!
//! So this watches. Each read is compared against the one before it, and
//! what comes out is a record of what happened to the account as seen
//! from outside it.
//!
//! ## Why a change log rather than a running diff against a baseline
//!
//! A fixed baseline goes stale the moment the account moves, and every
//! read afterwards reports the same difference again. That is a level
//! where an edge is wanted: the useful signal is *the account changed*,
//! not *the account is still not what it was an hour ago*.
//!
//! Roq makes the same distinction for stale orders and emits them
//! exactly once for exactly this reason.

use crate::binance::{OpenOrder, PositionSnapshot};
use crate::snapshot::Snapshot;

/// Something that happened between two reads.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    PositionOpened {
        side: String,
        amount: f64,
        entry: f64,
    },
    PositionClosed {
        side: String,
        was: f64,
    },
    PositionResized {
        side: String,
        from: f64,
        to: f64,
        /// The entry moved too, which for a martingale means the ladder
        /// anchor moved and every rung after it is priced differently.
        entry_from: f64,
        entry_to: f64,
    },
    OrderAppeared {
        client_order_id: String,
        side: String,
        position_side: String,
        price: f64,
        qty: f64,
    },
    /// Gone from the book. Filled, cancelled or expired — which of the
    /// three is not knowable from a snapshot, and this does not guess.
    /// Roq's documentation is explicit that a client cannot know, and
    /// the projects that guess are the ones with the duplicate-order
    /// bugs.
    OrderGone {
        client_order_id: String,
        filled: f64,
        of: f64,
    },
    /// Partially filled between two reads, and still resting.
    OrderPartiallyFilled {
        client_order_id: String,
        from: f64,
        to: f64,
        of: f64,
    },
    BalanceMoved {
        from: f64,
        to: f64,
    },
}

impl Change {
    /// Whether this is the kind of change that moves a ladder's anchor.
    ///
    /// Not a severity: a resize is normal when a cover fills. It marks
    /// the changes worth reading twice, because they are the ones whose
    /// absence from the local model would matter most.
    #[must_use]
    pub const fn touches_position(&self) -> bool {
        matches!(
            self,
            Self::PositionOpened { .. }
                | Self::PositionClosed { .. }
                | Self::PositionResized { .. }
        )
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::PositionOpened {
                side,
                amount,
                entry,
            } => {
                format!("{side} opened {amount} @ {entry}")
            }
            Self::PositionClosed { side, was } => format!("{side} closed (was {was})"),
            Self::PositionResized {
                side,
                from,
                to,
                entry_from,
                entry_to,
            } => {
                if (entry_from - entry_to).abs() < 1e-9 {
                    format!("{side} {from} -> {to}")
                } else {
                    format!("{side} {from} -> {to}, entry {entry_from} -> {entry_to}")
                }
            }
            Self::OrderAppeared {
                client_order_id,
                side,
                position_side,
                price,
                qty,
            } => format!("+ {client_order_id} {side} {position_side} {qty} @ {price}"),
            Self::OrderGone {
                client_order_id,
                filled,
                of,
            } => format!("- {client_order_id} left the book at {filled}/{of}"),
            Self::OrderPartiallyFilled {
                client_order_id,
                from,
                to,
                of,
            } => format!("~ {client_order_id} filled {from} -> {to} of {of}"),
            Self::BalanceMoved { from, to } => format!("wallet {from:.2} -> {to:.2}"),
        }
    }
}

/// What has been seen so far.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    pub reads: u64,
    pub incomplete_reads: u64,
    pub position_changes: u64,
    pub orders_appeared: u64,
    pub orders_gone: u64,
    pub partial_fills: u64,
    /// Reads where nothing at all changed. Reported because a watch that
    /// never sees a quiet read is watching something else, and a watch
    /// that only ever sees quiet reads has not yet earned any confidence.
    pub quiet_reads: u64,
}

impl Tally {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} reads ({} quiet, {} incomplete) | positions {} | orders +{} -{} ~{}",
            self.reads,
            self.quiet_reads,
            self.incomplete_reads,
            self.position_changes,
            self.orders_appeared,
            self.orders_gone,
            self.partial_fills,
        )
    }
}

/// Compares each read against the one before it.
#[derive(Debug, Default)]
pub struct Watcher {
    previous: Option<Snapshot>,
    pub tally: Tally,
}

impl Watcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a read could not be completed.
    ///
    /// Counted rather than ignored: a watch reporting no changes because
    /// every read failed looks exactly like a quiet account, and the two
    /// need to be distinguishable in the same output.
    pub const fn incomplete(&mut self) {
        self.tally.incomplete_reads += 1;
    }

    /// Feed the next read and get what changed.
    ///
    /// The first read establishes the baseline and reports nothing —
    /// everything would otherwise appear as having just been opened.
    pub fn observe(&mut self, now: &Snapshot) -> Vec<Change> {
        self.tally.reads += 1;
        let Some(before) = self.previous.replace(now.clone()) else {
            return Vec::new();
        };

        let mut changes = Vec::new();
        diff_positions(&before.positions, &now.positions, &mut changes);
        diff_orders(&before.open_orders, &now.open_orders, &mut changes);

        if (before.account.wallet_balance - now.account.wallet_balance).abs() > 1e-8 {
            changes.push(Change::BalanceMoved {
                from: before.account.wallet_balance,
                to: now.account.wallet_balance,
            });
        }

        for c in &changes {
            match c {
                Change::PositionOpened { .. }
                | Change::PositionClosed { .. }
                | Change::PositionResized { .. } => self.tally.position_changes += 1,
                Change::OrderAppeared { .. } => self.tally.orders_appeared += 1,
                Change::OrderGone { .. } => self.tally.orders_gone += 1,
                Change::OrderPartiallyFilled { .. } => self.tally.partial_fills += 1,
                Change::BalanceMoved { .. } => {}
            }
        }
        if changes.is_empty() {
            self.tally.quiet_reads += 1;
        }
        changes
    }
}

fn diff_positions(before: &[PositionSnapshot], now: &[PositionSnapshot], out: &mut Vec<Change>) {
    for b in before {
        match now.iter().find(|n| n.position_side == b.position_side) {
            None => out.push(Change::PositionClosed {
                side: b.position_side.clone(),
                was: b.amount,
            }),
            Some(n) => {
                if (b.amount - n.amount).abs() > 1e-12
                    || (b.entry_price - n.entry_price).abs() > 1e-9
                {
                    out.push(Change::PositionResized {
                        side: b.position_side.clone(),
                        from: b.amount,
                        to: n.amount,
                        entry_from: b.entry_price,
                        entry_to: n.entry_price,
                    });
                }
            }
        }
    }
    for n in now {
        if !before.iter().any(|b| b.position_side == n.position_side) {
            out.push(Change::PositionOpened {
                side: n.position_side.clone(),
                amount: n.amount,
                entry: n.entry_price,
            });
        }
    }
}

fn diff_orders(before: &[OpenOrder], now: &[OpenOrder], out: &mut Vec<Change>) {
    for b in before {
        match now.iter().find(|n| n.client_order_id == b.client_order_id) {
            None => out.push(Change::OrderGone {
                client_order_id: b.client_order_id.clone(),
                filled: b.executed_qty,
                of: b.orig_qty,
            }),
            Some(n) => {
                if (b.executed_qty - n.executed_qty).abs() > 1e-12 {
                    out.push(Change::OrderPartiallyFilled {
                        client_order_id: b.client_order_id.clone(),
                        from: b.executed_qty,
                        to: n.executed_qty,
                        of: b.orig_qty,
                    });
                }
            }
        }
    }
    for n in now {
        if !before
            .iter()
            .any(|b| b.client_order_id == n.client_order_id)
        {
            out.push(Change::OrderAppeared {
                client_order_id: n.client_order_id.clone(),
                side: n.side.clone(),
                position_side: n.position_side.clone(),
                price: n.price,
                qty: n.orig_qty,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::AccountSnapshot;
    use crate::snapshot::SnapshotBuilder;

    fn snap(positions: Vec<PositionSnapshot>, orders: Vec<OpenOrder>, wallet: f64) -> Snapshot {
        SnapshotBuilder::new("BTCUSDT")
            .account(AccountSnapshot {
                wallet_balance: wallet,
                unrealized: 0.0,
                margin_balance: wallet,
                read_at_ms: 1,
            })
            .positions(positions)
            .open_orders(orders)
            .seal()
            .expect("seals")
    }

    fn leg(side: &str, amount: f64, entry: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: side.into(),
            amount_text: String::new(),
            entry_text: String::new(),
            amount,
            entry_price: entry,
            unrealized: 0.0,
        }
    }

    fn order(cid: &str, filled: f64, of: f64) -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".into(),
            order_id: 1,
            client_order_id: cid.into(),
            side: "BUY".into(),
            position_side: "LONG".into(),
            price: 60_000.0,
            orig_qty: of,
            executed_qty: filled,
            status: "NEW".into(),
        }
    }

    /// The first read is the baseline. Reporting it as change would make
    /// every start look like the account had just been opened.
    #[test]
    fn the_first_read_reports_nothing() {
        let mut w = Watcher::new();
        let changes = w.observe(&snap(vec![leg("LONG", 0.400, 30_000.0)], vec![], 5_000.0));
        assert!(changes.is_empty());
        assert_eq!(w.tally.reads, 1);
    }

    #[test]
    fn a_cover_filling_shows_as_a_resize_and_names_the_moved_anchor() {
        let mut w = Watcher::new();
        w.observe(&snap(vec![leg("LONG", 0.400, 30_000.0)], vec![], 5_000.0));
        let changes = w.observe(&snap(vec![leg("LONG", 1.200, 62_000.0)], vec![], 5_000.0));

        assert_eq!(changes.len(), 1);
        let text = changes[0].describe();
        assert!(text.contains("0.4 -> 1.2"), "{text}");
        assert!(
            text.contains("entry"),
            "the anchor moved and the line says so: {text}"
        );
        assert!(changes[0].touches_position());
        assert_eq!(w.tally.position_changes, 1);
    }

    #[test]
    fn a_position_appearing_and_disappearing_are_distinct_changes() {
        let mut w = Watcher::new();
        w.observe(&snap(vec![leg("LONG", 0.400, 30_000.0)], vec![], 5_000.0));

        let opened = w.observe(&snap(
            vec![
                leg("LONG", 0.400, 30_000.0),
                leg("SHORT", -0.002, 62_808.82),
            ],
            vec![],
            5_000.0,
        ));
        assert!(matches!(opened[0], Change::PositionOpened { .. }));

        let closed = w.observe(&snap(vec![leg("LONG", 0.400, 30_000.0)], vec![], 5_000.0));
        assert!(matches!(closed[0], Change::PositionClosed { .. }));
    }

    /// An order leaving the book was filled, cancelled or expired, and a
    /// snapshot cannot say which. The projects that guess are the ones
    /// with the duplicate-order bugs, so this reports the fact and the
    /// fill it had reached, and stops there.
    #[test]
    fn an_order_leaving_the_book_is_reported_without_a_cause() {
        let mut w = Watcher::new();
        w.observe(&snap(vec![], vec![order("c-1", 0.0, 0.004)], 5_000.0));
        let changes = w.observe(&snap(vec![], vec![], 5_000.0));

        assert_eq!(changes.len(), 1);
        let text = changes[0].describe();
        assert!(text.contains("left the book"), "{text}");
        // It may state how far the order had got — that is observed. It
        // may not say why it left, which is not.
        for claim in ["cancel", "expired", "was filled", "completed"] {
            assert!(
                !text.to_lowercase().contains(claim),
                "must not claim a cause it cannot know ({claim}): {text}"
            );
        }
    }

    #[test]
    fn a_partial_fill_on_a_resting_order_is_seen() {
        let mut w = Watcher::new();
        w.observe(&snap(vec![], vec![order("c-1", 0.0, 0.004)], 5_000.0));
        let changes = w.observe(&snap(vec![], vec![order("c-1", 0.002, 0.004)], 5_000.0));
        assert!(matches!(changes[0], Change::OrderPartiallyFilled { .. }));
        assert_eq!(w.tally.partial_fills, 1);
    }

    /// A quiet account and an account nobody could read produce the same
    /// silence. The tally keeps them apart.
    #[test]
    fn quiet_reads_and_failed_reads_are_counted_separately() {
        let mut w = Watcher::new();
        let s = snap(vec![leg("LONG", 0.400, 30_000.0)], vec![], 5_000.0);
        w.observe(&s);
        w.observe(&s);
        w.incomplete();

        assert_eq!(w.tally.quiet_reads, 1);
        assert_eq!(w.tally.incomplete_reads, 1);
        let text = w.tally.render();
        assert!(text.contains("1 quiet"), "{text}");
        assert!(text.contains("1 incomplete"), "{text}");
    }

    #[test]
    fn a_balance_move_is_reported() {
        let mut w = Watcher::new();
        w.observe(&snap(vec![], vec![], 5_000.0));
        let changes = w.observe(&snap(vec![], vec![], 5_340.99));
        assert!(matches!(changes[0], Change::BalanceMoved { .. }));
    }
}
