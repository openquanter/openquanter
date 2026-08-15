//! The resting order book, ordered by price then arrival.
//!
//! Price-time priority is maintained by construction: orders are held
//! in price-sorted vectors, and within a price level the insertion
//! point is chosen so that an earlier arrival is never displaced by a
//! later one. The matcher therefore never sorts and never scans past
//! the first order that cannot trigger.
//!
//! Vectors rather than a map of price levels: at the depth a single
//! strategy rests — tens of orders, not thousands — a contiguous vector
//! wins on every operation that matters, and the whole book fits in a
//! cache line or two. The structure is behind this module's API so that
//! a venue-depth book can replace it without touching the matcher.

use oq_types::{OrderId, PriceTicks, Side, Working};

/// A resting order with its arrival rank.
///
/// `arrival` is the sequence in which the engine accepted the order,
/// and it is the tie-break within a price level. Wall-clock time is not
/// used: two orders accepted in the same nanosecond still have a
/// definite order, and that order must be the same on every replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resting {
    pub order: Working,
    pub arrival: u64,
}

impl Resting {
    #[must_use]
    pub const fn id(&self) -> OrderId {
        self.order.id()
    }

    #[must_use]
    pub const fn price(&self) -> Option<PriceTicks> {
        self.order.price()
    }
}

/// Resting orders for one instrument.
///
/// Four collections rather than two: market orders have no price and
/// therefore no position in a price ordering, and mixing them into the
/// limit vectors would either corrupt the sort or require a sentinel
/// price that then has to be special-cased at every comparison.
#[derive(Debug, Default)]
pub struct Book {
    /// Limit buys, best (highest) price first.
    bids: Vec<Resting>,
    /// Limit sells, best (lowest) price first.
    asks: Vec<Resting>,
    /// Market buys, in arrival order.
    market_buys: Vec<Resting>,
    /// Market sells, in arrival order.
    market_sells: Vec<Resting>,
    next_arrival: u64,
}

impl Book {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total resting orders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bids.len() + self.asks.len() + self.market_buys.len() + self.market_sells.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Best resting bid price, if any limit buy rests.
    #[must_use]
    pub fn best_bid(&self) -> Option<PriceTicks> {
        self.bids.first().and_then(Resting::price)
    }

    /// Best resting ask price, if any limit sell rests.
    #[must_use]
    pub fn best_ask(&self) -> Option<PriceTicks> {
        self.asks.first().and_then(Resting::price)
    }

    #[must_use]
    pub fn has_market_orders(&self) -> bool {
        !self.market_buys.is_empty() || !self.market_sells.is_empty()
    }

    /// Add an order to the book.
    pub fn insert(&mut self, order: Working) {
        let arrival = self.next_arrival;
        self.next_arrival += 1;
        let resting = Resting { order, arrival };

        match (order.side(), order.price()) {
            (Side::Buy, None) => self.market_buys.push(resting),
            (Side::Sell, None) => self.market_sells.push(resting),
            (Side::Buy, Some(price)) => {
                // Descending by price; within a level, after everything
                // already there, which is arrival order.
                let at = self
                    .bids
                    .partition_point(|r| r.price().is_some_and(|p| p >= price));
                self.bids.insert(at, resting);
            }
            (Side::Sell, Some(price)) => {
                let at = self
                    .asks
                    .partition_point(|r| r.price().is_some_and(|p| p <= price));
                self.asks.insert(at, resting);
            }
        }
    }

    /// Remove an order by id, returning it if it was resting.
    pub fn remove(&mut self, id: OrderId) -> Option<Working> {
        for side in [
            &mut self.bids,
            &mut self.asks,
            &mut self.market_buys,
            &mut self.market_sells,
        ] {
            if let Some(pos) = side.iter().position(|r| r.id() == id) {
                return Some(side.remove(pos).order);
            }
        }
        None
    }

    /// Whether an order with this id is resting.
    #[must_use]
    pub fn contains(&self, id: OrderId) -> bool {
        self.iter().any(|r| r.id() == id)
    }

    /// Every resting order, in no particular cross-collection order.
    pub fn iter(&self) -> impl Iterator<Item = &Resting> {
        self.bids
            .iter()
            .chain(self.asks.iter())
            .chain(self.market_buys.iter())
            .chain(self.market_sells.iter())
    }

    /// Ids of every resting order, best-priced first within each side.
    #[must_use]
    pub fn resting_ids(&self) -> Vec<OrderId> {
        self.iter().map(Resting::id).collect()
    }

    pub(crate) fn bids(&self) -> &[Resting] {
        &self.bids
    }

    pub(crate) fn asks(&self) -> &[Resting] {
        &self.asks
    }

    pub(crate) fn market_buys(&self) -> &[Resting] {
        &self.market_buys
    }

    pub(crate) fn market_sells(&self) -> &[Resting] {
        &self.market_sells
    }

    /// Replace a resting order in place, or drop it when the fill
    /// completed it.
    pub(crate) fn replace(&mut self, id: OrderId, updated: Option<Working>) {
        for side in [
            &mut self.bids,
            &mut self.asks,
            &mut self.market_buys,
            &mut self.market_sells,
        ] {
            if let Some(pos) = side.iter().position(|r| r.id() == id) {
                match updated {
                    // A partial fill keeps the order's queue position:
                    // being partially filled does not send an order to
                    // the back of its price level at any venue in scope.
                    Some(working) => side[pos].order = working,
                    None => {
                        side.remove(pos);
                    }
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{Order, OrderId, OrderKind, QtyLots, Stamp, TimeInForce};

    fn limit(id: u64, side: Side, price: i64, qty: i64) -> Working {
        Working::Live(
            Order::new(
                OrderId::new(id),
                side,
                OrderKind::Limit {
                    price: PriceTicks(price),
                },
                QtyLots(qty),
                TimeInForce::GoodTilCancel,
                Stamp::synthetic(0),
            )
            .expect("positive qty")
            .accept(),
        )
    }

    fn market(id: u64, side: Side, qty: i64) -> Working {
        Working::Live(
            Order::new(
                OrderId::new(id),
                side,
                OrderKind::Market,
                QtyLots(qty),
                TimeInForce::GoodTilCancel,
                Stamp::synthetic(0),
            )
            .expect("positive qty")
            .accept(),
        )
    }

    #[test]
    fn bids_are_best_first_and_asks_are_best_first() {
        let mut book = Book::new();
        book.insert(limit(1, Side::Buy, 100, 1));
        book.insert(limit(2, Side::Buy, 120, 1));
        book.insert(limit(3, Side::Buy, 110, 1));
        book.insert(limit(4, Side::Sell, 200, 1));
        book.insert(limit(5, Side::Sell, 180, 1));

        assert_eq!(book.best_bid(), Some(PriceTicks(120)));
        assert_eq!(book.best_ask(), Some(PriceTicks(180)));
        let bid_prices: Vec<_> = book.bids().iter().filter_map(Resting::price).collect();
        assert_eq!(
            bid_prices,
            vec![PriceTicks(120), PriceTicks(110), PriceTicks(100)]
        );
    }

    #[test]
    fn equal_prices_keep_arrival_order() {
        let mut book = Book::new();
        book.insert(limit(1, Side::Buy, 100, 1));
        book.insert(limit(2, Side::Buy, 100, 1));
        book.insert(limit(3, Side::Buy, 100, 1));
        let ids: Vec<_> = book.bids().iter().map(Resting::id).collect();
        assert_eq!(
            ids,
            vec![OrderId::new(1), OrderId::new(2), OrderId::new(3)],
            "price-time priority: first in, first in line"
        );
    }

    #[test]
    fn market_orders_are_kept_apart_from_the_price_ordering() {
        let mut book = Book::new();
        book.insert(market(1, Side::Buy, 1));
        book.insert(limit(2, Side::Buy, 100, 1));
        assert_eq!(book.market_buys().len(), 1);
        assert_eq!(book.bids().len(), 1);
        assert_eq!(book.best_bid(), Some(PriceTicks(100)));
    }

    #[test]
    fn remove_finds_orders_on_any_side() {
        let mut book = Book::new();
        book.insert(limit(1, Side::Buy, 100, 1));
        book.insert(limit(2, Side::Sell, 200, 1));
        book.insert(market(3, Side::Sell, 1));
        assert!(book.remove(OrderId::new(2)).is_some());
        assert!(book.remove(OrderId::new(3)).is_some());
        assert!(book.remove(OrderId::new(99)).is_none());
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn a_partial_fill_keeps_its_queue_position() {
        let mut book = Book::new();
        book.insert(limit(1, Side::Buy, 100, 10));
        book.insert(limit(2, Side::Buy, 100, 10));

        let first = book.bids()[0].order;
        let outcome = first.fill(QtyLots(4)).expect("valid fill");
        let still: Option<Working> = outcome.into();
        book.replace(OrderId::new(1), still);

        let ids: Vec<_> = book.bids().iter().map(Resting::id).collect();
        assert_eq!(ids, vec![OrderId::new(1), OrderId::new(2)]);
        assert_eq!(book.bids()[0].order.remaining(), QtyLots(6));
    }
}
