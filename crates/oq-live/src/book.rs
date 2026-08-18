//! What the process believes it holds.
//!
//! Built from the venue's own event stream, and never trusted on its
//! own — [`crate::Session`] compares it against the venue on a schedule,
//! because a belief assembled from messages is only as good as the
//! messages that arrived.
//!
//! # The account stream is not this process's stream
//!
//! A venue's user data stream is scoped to the **account**, not to a
//! symbol and not to a process. Every order the account places arrives
//! on it, including orders placed by something else entirely — measured
//! on a shared testnet account, another system's resting orders showed
//! up here and were counted as this process's own.
//!
//! That is not a bandwidth problem. Measured over 45 minutes the whole
//! account produced three events, against seven thousand market ticks in
//! the same window. It is a semantics problem, and it has teeth: the
//! count of resting orders feeds the risk gate's cap, so another
//! system's orders were consuming this one's limit.
//!
//! Filtering by symbol is not enough, because two systems can trade the
//! same symbol on one account — which is exactly what was observed. The
//! only sound filter is the client id prefix this process chose for
//! itself, since that is the one thing no other system will reproduce.
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
use oq_types::QtyLots;

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
    /// Client id prefix this process issues. Events naming an order that
    /// does not start with it belong to something else.
    prefix: String,
    /// Events discarded as another system's. Counted rather than
    /// ignored: a rising number is how you learn the account is shared,
    /// which is worth knowing before it is inferred from a limit.
    foreign: u64,
}

impl Book {
    /// A book that accepts every event, for callers with the account to
    /// themselves.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A book that only accepts orders whose client id starts with
    /// `prefix`.
    #[must_use]
    pub fn owning(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            ..Self::default()
        }
    }

    /// Whether an event names an order this process issued.
    #[must_use]
    pub fn is_ours(&self, client_id: &str) -> bool {
        self.prefix.is_empty() || client_id.starts_with(&self.prefix)
    }

    /// Events discarded as belonging to another system.
    #[must_use]
    pub const fn foreign(&self) -> u64 {
        self.foreign
    }

    /// Apply one order update.
    ///
    /// Returns whether it changed anything. A redelivered fill returns
    /// `false` and is counted rather than silently dropped: a stream
    /// redelivering steadily is worth noticing even though each
    /// individual duplicate is handled correctly.
    pub fn apply(&mut self, u: &OrderUpdate) -> bool {
        if !self.is_ours(&u.client_id) {
            self.foreign += 1;
            return false;
        }
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

    /// Net signed quantity in the contract's own lots.
    ///
    /// The form the risk gate needs. Venues report positions as decimal
    /// text and this crate keeps them as `f64`, but a limit is compared
    /// in lots, and converting at the comparison rather than here is how
    /// one caller ends up comparing coins against lots — a factor of ten
    /// thousand on this contract, in the check that is supposed to stop
    /// exactly that kind of mistake.
    #[must_use]
    pub fn net_lots(&self, symbol: &str, qty_scale: u8) -> QtyLots {
        let scale = 10_f64.powi(i32::from(qty_scale));
        // Rounded rather than truncated: a position of 0.0159999 from a
        // decimal round-trip is 160 lots, and truncating it to 159 would
        // make the gate believe the account is smaller than it is.
        QtyLots((self.net(symbol) * scale).round() as i64)
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
            side: "BUY".into(),
            maker: false,
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

#[cfg(test)]
mod ownership {
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
            side: "BUY".into(),
            maker: false,
            trade_id,
            event_ms: 0,
        }
    }

    #[test]
    fn another_systems_order_does_not_count_as_resting() {
        // Observed on a shared testnet account: a resting order placed by
        // a different system arrived on this stream and was counted here,
        // and the count feeds the risk gate's cap — so somebody else's
        // orders were consuming this process's limit.
        //
        // The id below is synthetic. The one actually observed carried a
        // venue broker-referral prefix, which identifies whoever placed
        // the order — a deployment detail, and §8's boundary policy keeps
        // those out of this repository. The test needs a prefix that is
        // not ours; it does not need a real one.
        let mut b = Book::owning("oq123");
        assert!(!b.apply(&update("x-brokerref-4471", "NEW", None)));
        assert_eq!(b.working(), 0, "not ours, not counted");
        assert_eq!(b.foreign(), 1, "counted as foreign rather than ignored");
    }

    #[test]
    fn our_own_order_still_counts() {
        let mut b = Book::owning("oq123");
        assert!(b.apply(&update("oq123-1", "NEW", None)));
        assert_eq!(b.working(), 1);
        assert_eq!(b.foreign(), 0);
    }

    #[test]
    fn another_systems_fill_does_not_reach_the_deduplication_table() {
        // Worse than the count: a foreign trade id would occupy a slot,
        // and a later fill of ours reusing that id — venues number trades
        // per symbol, not per client — would be discarded as a duplicate.
        let mut b = Book::owning("oq123");
        assert!(!b.apply(&update("someone-else-1", "FILLED", Some(7))));
        assert!(
            b.apply(&update("oq123-1", "FILLED", Some(7))),
            "our fill with the same trade id must still be applied"
        );
    }

    #[test]
    fn the_same_symbol_traded_by_two_systems_is_the_case_that_matters() {
        // Filtering by symbol would not have caught this: both orders are
        // BTCUSDT on one account, which is exactly what was observed.
        let mut b = Book::owning("oq123");
        b.apply(&update("x-other-1", "NEW", None));
        b.apply(&update("oq123-1", "NEW", None));
        assert_eq!(b.working(), 1);
        assert_eq!(b.foreign(), 1);
    }

    #[test]
    fn a_book_with_no_prefix_accepts_everything() {
        // For a caller that has the account to itself, and to keep the
        // previous behaviour reachable rather than silently changed.
        let mut b = Book::new();
        assert!(b.apply(&update("anything", "NEW", None)));
        assert_eq!(b.working(), 1);
    }
}
