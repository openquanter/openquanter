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
    /// Fills already applied, by trade id *and* the order it filled.
    ///
    /// The trade id alone is not enough: when two systems on one account
    /// trade against each other, both sides of the match carry the same
    /// trade id, and keying on it alone would discard the second side as
    /// a redelivery of the first.
    seen_trades: HashSet<(i64, String)>,
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
    ///
    /// The prefix must end where an id this process composes would put
    /// the sequence: at a `-`, or at the digits themselves on a venue
    /// with no punctuation. A bare `starts_with` claimed `oq1…` for a
    /// process whose prefix is `oq`, and another system's orders then
    /// counted against this one's limits and could be withdrawn by it.
    pub fn is_ours(&self, client_id: &str) -> bool {
        if self.prefix.is_empty() {
            return true;
        }
        let Some(rest) = client_id.strip_prefix(&self.prefix) else {
            return false;
        };
        let sequence = rest.strip_prefix('-').unwrap_or(rest);
        !sequence.is_empty() && sequence.bytes().all(|b| b.is_ascii_digit())
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
        let ours = self.is_ours(&u.client_id);
        let is_fill = matches!(u.status.as_str(), "PARTIALLY_FILLED" | "FILLED");
        if is_fill {
            let Some(trade_id) = u.trade_id else {
                // A fill event with no trade id cannot be
                // deduplicated, so it is not applied. Applying it
                // would make a redelivery indistinguishable from a
                // second fill, which is the error that compounds.
                if !ours {
                    self.foreign += 1;
                }
                return false;
            };
            if !self.seen_trades.insert((trade_id, u.client_id.clone())) {
                if ours {
                    self.duplicates += 1;
                }
                return false;
            }
            // Every fill moves the position, whoever placed the order.
            // The position is the account's, and it is what the risk
            // gate caps: until this line it moved only when the venue's
            // own number was adopted — at startup and after a lost
            // stream — so a ladder filling rung by rung on a healthy link
            // was checked against the position it started with, and the
            // cap could not fire.
            self.book_fill(u);
        }
        if !ours {
            self.foreign += 1;
            return is_fill;
        }
        match u.status.as_str() {
            "NEW" => {
                if self.working.iter().any(|w| w == &u.client_id) {
                    return false;
                }
                self.working.push(u.client_id.clone());
                true
            }
            // `EXPIRED_IN_MATCH` is self-trade prevention ending an order:
            // an ending like any other, and read as none it held a
            // working slot for the rest of the run.
            "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "REJECTED" => {
                let before = self.working.len();
                self.working.retain(|w| w != &u.client_id);
                before != self.working.len()
            }
            "PARTIALLY_FILLED" | "FILLED" => {
                if u.status == "FILLED" {
                    self.working.retain(|w| w != &u.client_id);
                }
                true
            }
            _ => false,
        }
    }

    /// Move the leg a fill names by the quantity it traded.
    fn book_fill(&mut self, u: &OrderUpdate) {
        let Ok(qty) = u.last_qty.parse::<f64>() else {
            return;
        };
        let signed = if u.side.eq_ignore_ascii_case("BUY") {
            qty
        } else {
            -qty
        };
        let leg = if u.position_side.is_empty() {
            "BOTH"
        } else {
            u.position_side.as_str()
        };
        match self
            .positions
            .iter_mut()
            .find(|p| p.symbol == u.symbol && p.side.eq_ignore_ascii_case(leg))
        {
            Some(p) => p.amount += signed,
            None => self.positions.push(Position {
                symbol: u.symbol.clone(),
                side: leg.to_ascii_uppercase(),
                amount: signed,
            }),
        }
    }

    /// An order the venue accepted, believed resting from now.
    ///
    /// The `NEW` event says the same thing and says it later. Waiting
    /// for it means a burst of orders is counted as none of them: the
    /// bound on resting orders is checked against a number that has not
    /// caught up, and seven rungs go out under a limit of four because
    /// each was measured before any of them had been acknowledged.
    ///
    /// Idempotent with `apply`, which inserts the same id when the
    /// event does arrive.
    pub fn on_sent(&mut self, client_id: &str) {
        if self.working.iter().any(|w| w == client_id) {
            return;
        }
        self.working.push(client_id.to_string());
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

    /// One leg's signed quantity in lots: `LONG` is positive, `SHORT`
    /// negative, as the venue reports them.
    ///
    /// What an opening order on a hedged account is capped against. The
    /// net of the two legs is the wrong number there: long 20 and short
    /// 20 net to nothing, and a cap on the net lets both legs grow
    /// without bound in step.
    #[must_use]
    pub fn leg_lots(&self, symbol: &str, leg: &str, qty_scale: u8) -> QtyLots {
        let scale = 10_f64.powi(i32::from(qty_scale));
        let amount: f64 = self
            .positions
            .iter()
            .filter(|p| p.symbol == symbol && p.side.eq_ignore_ascii_case(leg))
            .map(|p| p.amount)
            .sum();
        QtyLots((amount * scale).round() as i64)
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
            venue_id: "1".to_string(),
            status: status.into(),
            last_qty: "0.001".into(),
            cumulative_qty: "0.001".into(),
            last_price: "60000".into(),
            side: "BUY".into(),
            position_side: "BOTH".into(),
            maker: false,
            trade_id,
            event_ms: 0,
            initiator: oq_gateway::Initiator::Account,
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
            venue_id: "1".to_string(),
            status: status.into(),
            last_qty: "0.001".into(),
            cumulative_qty: "0.001".into(),
            last_price: "60000".into(),
            side: "BUY".into(),
            position_side: "BOTH".into(),
            maker: false,
            trade_id,
            event_ms: 0,
            initiator: oq_gateway::Initiator::Account,
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

    /// A longer prefix that begins with ours is not ours.
    #[test]
    fn a_prefix_that_merely_begins_with_ours_is_not_ours() {
        let b = Book::owning("oq");
        assert!(b.is_ours("oq-17"), "hyphenated, as on Binance");
        assert!(b.is_ours("oq17"), "bare digits, as on OKX");
        assert!(!b.is_ours("oq1abc"));
        assert!(!b.is_ours("oq1-5"), "another system whose prefix is oq1");
        assert!(!b.is_ours("oq"), "no sequence at all");
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
        b.apply(&update("someone-else-1", "FILLED", Some(7)));
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

#[cfg(test)]
mod fills {
    use super::*;

    fn fill(client_id: &str, trade_id: i64, side: &str) -> OrderUpdate {
        OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: client_id.into(),
            venue_id: "1".to_string(),
            status: "FILLED".into(),
            last_qty: "0.004".into(),
            cumulative_qty: "0.004".into(),
            last_price: "60000".into(),
            side: side.into(),
            position_side: "BOTH".into(),
            maker: true,
            trade_id: Some(trade_id),
            event_ms: 0,
            initiator: oq_gateway::Initiator::Account,
        }
    }

    /// Two systems on one account trading against each other: both
    /// sides carry one trade id, and both moved the account.
    #[test]
    fn both_sides_of_a_match_between_two_systems_are_applied() {
        let mut b = Book::owning("oq123");
        b.apply(&fill("oq123-1", 7, "BUY"));
        b.apply(&fill("other-1", 7, "SELL"));
        assert_eq!(b.net_lots("BTCUSDT", 3), QtyLots(0));
        assert_eq!(b.duplicates(), 0);
    }
}

#[cfg(test)]
mod stp {
    use super::*;

    #[test]
    fn an_order_ended_by_self_trade_prevention_stops_working() {
        let mut b = Book::owning("oq");
        let mut u = OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: "oq-1".into(),
            venue_id: "1".into(),
            status: "NEW".into(),
            last_qty: "0".into(),
            cumulative_qty: "0".into(),
            last_price: "0".into(),
            side: "BUY".into(),
            position_side: "BOTH".into(),
            maker: false,
            trade_id: None,
            event_ms: 0,
            initiator: oq_gateway::Initiator::Account,
        };
        b.apply(&u);
        assert_eq!(b.working(), 1);
        u.status = "EXPIRED_IN_MATCH".into();
        assert!(b.apply(&u));
        assert_eq!(b.working(), 0);
    }
}
