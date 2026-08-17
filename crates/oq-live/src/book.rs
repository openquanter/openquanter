//! What the process believes it holds.
//!
//! Built from the venue's own event stream, and never trusted on its
//! own — [`crate::Session`] compares it against the venue on a schedule,
//! because a belief assembled from messages is only as good as the
//! messages that arrived.
//!
//! # Fills are deduplicated by the venue's trade id
//!
//! A stream that reconnects can redeliver, and a fill counted twice is
//! a position that never existed. The venue's trade id is the only
//! identifier that is stable across a redelivery — a local counter
//! restarts, and a timestamp repeats. Events that are not fills carry
//! no trade id and are not deduplicated, because they change no
//! quantity.

use std::collections::HashSet;

use oq_gateway::OrderUpdate;

/// One leg of one contract.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub symbol: String,
    /// `BOTH` under one-way netting, `LONG` or `SHORT` under hedging.
    pub side: String,
    /// Signed, in the venue's own decimal quantities.
    pub amount: f64,
}

/// The positions and resting orders this process believes in.
#[derive(Debug, Default)]
pub struct Book {
    positions: Vec<Position>,
    working: Vec<String>,
    seen_trades: HashSet<i64>,
    /// Fills that arrived twice and were discarded.
    duplicates: u64,
}

impl Book {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one order update.
    ///
    /// Returns whether it changed anything. A redelivered fill returns
    /// `false` and is counted rather than silently dropped: a stream
    /// redelivering steadily is worth noticing even though each
    /// individual duplicate is handled correctly.
    pub fn apply(&mut self, u: &OrderUpdate) -> bool {
        match u.status.as_str() {
            "NEW" => {
                if self.working.iter().any(|w| w == &u.client_id) {
                    return false;
                }
                self.working.push(u.client_id.clone());
                true
            }
            "CANCELED" | "EXPIRED" | "REJECTED" => {
                let before = self.working.len();
                self.working.retain(|w| w != &u.client_id);
                before != self.working.len()
            }
            "PARTIALLY_FILLED" | "FILLED" => {
                let Some(trade_id) = u.trade_id else {
                    // A fill event with no trade id cannot be
                    // deduplicated, so it is not applied. Applying it
                    // would make a redelivery indistinguishable from a
                    // second fill, which is the error that compounds.
                    return false;
                };
                if !self.seen_trades.insert(trade_id) {
                    self.duplicates += 1;
                    return false;
                }
                if u.status == "FILLED" {
                    self.working.retain(|w| w != &u.client_id);
                }
                true
            }
            _ => false,
        }
    }

    /// Replace the believed positions with the venue's own.
    ///
    /// Used after a reconciliation. The stream's view is discarded
    /// rather than merged: if the two disagree the venue is right by
    /// definition, and merging would keep whatever made them disagree.
    pub fn adopt(&mut self, positions: Vec<Position>) {
        self.positions = positions;
    }

    #[must_use]
    pub fn positions(&self) -> &[Position] {
        &self.positions
    }

    /// How many orders are resting.
    #[must_use]
    pub fn working(&self) -> u32 {
        u32::try_from(self.working.len()).unwrap_or(u32::MAX)
    }

    /// Fills discarded as redeliveries.
    #[must_use]
    pub const fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Net signed quantity for a symbol across every leg.
    #[must_use]
    pub fn net(&self, symbol: &str) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.symbol == symbol)
            .map(|p| p.amount)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(client_id: &str, status: &str, trade_id: Option<i64>) -> OrderUpdate {
        OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: client_id.into(),
            venue_id: 1,
            status: status.into(),
            last_qty: "0.001".into(),
            cumulative_qty: "0.001".into(),
            last_price: "60000".into(),
            trade_id,
            event_ms: 0,
        }
    }

    #[test]
    fn a_redelivered_fill_is_counted_once() {
        // The failure this exists for: a reconnecting stream repeats
        // what it already said, and a fill counted twice is a position
        // that never existed.
        let mut b = Book::new();
        assert!(b.apply(&update("a", "FILLED", Some(7))));
        assert!(!b.apply(&update("a", "FILLED", Some(7))));
        assert_eq!(b.duplicates(), 1, "noticed rather than silently dropped");
    }

    #[test]
    fn two_different_fills_are_both_applied() {
        let mut b = Book::new();
        assert!(b.apply(&update("a", "PARTIALLY_FILLED", Some(7))));
        assert!(b.apply(&update("a", "FILLED", Some(8))));
        assert_eq!(b.duplicates(), 0);
    }

    #[test]
    fn a_fill_without_a_trade_id_is_not_applied() {
        // It cannot be deduplicated, so applying it would make a
        // redelivery indistinguishable from a second fill.
        let mut b = Book::new();
        assert!(!b.apply(&update("a", "FILLED", None)));
    }

    #[test]
    fn resting_orders_are_counted_and_removed_when_they_end() {
        let mut b = Book::new();
        b.apply(&update("a", "NEW", None));
        b.apply(&update("b", "NEW", None));
        assert_eq!(b.working(), 2);
        b.apply(&update("a", "CANCELED", None));
        assert_eq!(b.working(), 1);
        b.apply(&update("b", "FILLED", Some(9)));
        assert_eq!(b.working(), 0, "a filled order is no longer resting");
    }

    #[test]
    fn the_same_order_arriving_new_twice_is_counted_once() {
        let mut b = Book::new();
        assert!(b.apply(&update("a", "NEW", None)));
        assert!(!b.apply(&update("a", "NEW", None)));
        assert_eq!(b.working(), 1);
    }

    #[test]
    fn adopting_the_venues_view_replaces_rather_than_merges() {
        // If the two disagree the venue is right by definition, and
        // merging would preserve whatever made them disagree.
        let mut b = Book::new();
        b.adopt(vec![Position {
            symbol: "BTCUSDT".into(),
            side: "LONG".into(),
            amount: 5.0,
        }]);
        b.adopt(vec![Position {
            symbol: "BTCUSDT".into(),
            side: "LONG".into(),
            amount: 1.0,
        }]);
        assert_eq!(b.net("BTCUSDT"), 1.0);
    }

    #[test]
    fn both_legs_of_a_hedged_symbol_net_together() {
        let mut b = Book::new();
        b.adopt(vec![
            Position {
                symbol: "BTCUSDT".into(),
                side: "LONG".into(),
                amount: 3.0,
            },
            Position {
                symbol: "BTCUSDT".into(),
                side: "SHORT".into(),
                amount: -1.0,
            },
        ]);
        assert_eq!(b.net("BTCUSDT"), 2.0);
    }
}
