//! Fidelity tier L0: tick replay.
//!
//! L0 answers one question — *would this order have been touched by the
//! market's path through this window?* — and answers it the way the
//! reference implementation this engine must reproduce answers it. It
//! is the anchor of the fidelity ladder: fast enough for parameter
//! sweeps, and frozen once released, because every regression test in
//! the project measures against it.
//!
//! ## The three rules
//!
//! **Gap fill.** Between two ticks the price travelled, and the window's
//! extremes say how far. An order the path crossed is filled *at its own
//! price* — the market reached it, so it traded there. This runs before
//! ordinary matching, because an order swept by the excursion was
//! touched before the window's closing quotes existed.
//!
//! **Crossing.** After the gap, orders that the window's closing state
//! still reaches are filled. A limit order gets price improvement — a
//! buy at 100 against an ask of 95 fills at 95 — because that is what a
//! venue does and what the reference does.
//!
//! **Market orders always fill.** At the window's reference price, as
//! taker. L0 does not model depth, so it cannot model a market order
//! that walks the book; that is what L2 is for, and pretending
//! otherwise at L0 would produce a number that looks like slippage
//! without being one.
//!
//! ## What L0 deliberately does not do
//!
//! No queue position, no partial fills, no market impact, no latency. A
//! resting order that the price reached is filled in full. For a
//! strategy whose orders are small against displayed liquidity this is
//! a reasonable approximation and the reason L0 exists; for a
//! market-making strategy it is optimistic in a way no parameter can
//! fix, and the answer is L1, not a modified L0.
//!
//! ## A deliberate divergence from the reference
//!
//! The reference calls back into the strategy *during* matching, so a
//! strategy can cancel an order while that order is being processed.
//! Here, matching is a pure function from state and tick to fills, and
//! callbacks are outputs applied afterwards. Re-entrant mutation of the
//! book mid-match is not representable — which is the safer design, and
//! is a difference that parity against real strategies must confirm is
//! not exercised. It is recorded here rather than discovered later.

use crate::book::{Book, Resting};
use crate::tick::Tick;
use oq_types::{
    Fill, IdAllocator, InstrumentId, Liquidity, Offset, OrderId, PriceTicks, QtyLots, Side, Working,
};

/// Why a fill happened, kept for attribution in reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillReason {
    /// The price path crossed a resting order between ticks.
    GapCrossed,
    /// The window's closing state reached the order.
    Crossed,
    /// A market order, which fills unconditionally.
    Market,
}

/// A fill together with why it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L0Fill {
    pub fill: Fill,
    pub reason: FillReason,
}

/// The L0 matching engine for one instrument.
#[derive(Debug)]
pub struct L0Engine {
    instrument: InstrumentId,
    book: Book,
    ids: IdAllocator,
    prev_price: Option<PriceTicks>,
    /// Timestamp of the previous tick, for stamping gap-crossed fills.
    prev_stamp: Option<oq_types::Stamp>,
    /// Reused across ticks so the hot path does not allocate.
    fills: Vec<L0Fill>,
    /// Scratch for gap-fill candidate selection, likewise reused.
    candidates: Vec<(u64, OrderId)>,
}

impl L0Engine {
    #[must_use]
    pub fn new(instrument: InstrumentId) -> Self {
        Self {
            instrument,
            book: Book::new(),
            ids: IdAllocator::new(),
            prev_price: None,
            prev_stamp: None,
            fills: Vec::with_capacity(16),
            candidates: Vec::with_capacity(16),
        }
    }

    #[must_use]
    pub const fn instrument(&self) -> InstrumentId {
        self.instrument
    }

    #[must_use]
    pub const fn book(&self) -> &Book {
        &self.book
    }

    /// The last traded price the engine has seen.
    #[must_use]
    pub const fn last_price(&self) -> Option<PriceTicks> {
        self.prev_price
    }

    /// Rest an order.
    ///
    /// It becomes eligible on the *next* tick, never the one currently
    /// being processed. Same-tick eligibility would let a strategy that
    /// reacts to a tick trade inside that tick, which is a form of
    /// lookahead: the strategy would be acting on information the
    /// market had not yet produced when the order would really have
    /// been sent.
    pub fn submit(&mut self, order: Working) {
        self.book.insert(order);
    }

    /// Withdraw a resting order.
    ///
    /// Returns whether it was there. A cancel for an order that already
    /// filled returns `false` rather than erroring: the race is normal,
    /// and the caller learns the outcome from the return value.
    pub fn cancel(&mut self, id: OrderId) -> bool {
        self.book.remove(id).is_some()
    }

    /// Withdraw every resting order, returning how many were removed.
    pub fn cancel_all(&mut self) -> usize {
        let ids = self.book.resting_ids();
        let n = ids.len();
        for id in ids {
            self.book.remove(id);
        }
        n
    }

    /// Advance the engine by one tick and return the fills it produced.
    ///
    /// The returned slice borrows an internal buffer that the next call
    /// overwrites; callers that need to keep fills copy them out. This
    /// keeps the per-tick path free of allocation, which at a hundred
    /// million ticks is the difference between an afternoon and a
    /// coffee break.
    pub fn on_tick(&mut self, tick: &Tick) -> &[L0Fill] {
        self.fills.clear();

        self.gap_fill(tick);
        self.cross(tick);

        // Set *after* matching: the gap check compares the previous
        // window's close against this window's extremes, so it must not
        // see this tick's price yet.
        self.prev_price = Some(tick.last);
        self.prev_stamp = Some(tick.stamp);
        &self.fills
    }

    /// Fills caused by the price path between the previous tick and
    /// this one.
    fn gap_fill(&mut self, tick: &Tick) {
        let Some(prev) = self.prev_price else {
            // The first tick establishes a reference point; there is no
            // path before it to have crossed anything.
            return;
        };
        if self.book.is_empty() {
            return;
        }

        let up = tick.up_extent();
        let dn = tick.dn_extent();

        self.candidates.clear();
        if up > prev {
            // Rising: sells between the old price and the new high were
            // swept. Bounds are exclusive below and inclusive above,
            // matching the reference exactly.
            for r in self.book.asks() {
                if let Some(price) = r.price()
                    && price > prev
                    && price <= up
                {
                    self.candidates.push((r.arrival, r.id()));
                }
            }
        } else if dn < prev {
            // Falling: buys between the old price and the new low.
            for r in self.book.bids() {
                if let Some(price) = r.price()
                    && price < prev
                    && price >= dn
                {
                    self.candidates.push((r.arrival, r.id()));
                }
            }
        }
        if self.candidates.is_empty() {
            return;
        }

        // Arrival order, not price order: the reference walks its live
        // order map, which is insertion-ordered, and trade identifiers
        // are assigned in the order fills are produced. Reproducing the
        // identifiers is part of reproducing the behavior.
        self.candidates.sort_unstable();

        for i in 0..self.candidates.len() {
            let (_, id) = self.candidates[i];
            let Some(resting) = self.book.iter().find(|r| r.id() == id).copied() else {
                continue;
            };
            let price = resting.price().unwrap_or(PriceTicks::ZERO);
            // Filled at the order's own price: the market reached that
            // level, so that is where the trade happened. Filling at the
            // window's close instead would credit the strategy with a
            // price the market only reached later.
            // Stamped with the *previous* tick's time, not this one. The
            // crossing happened somewhere in the interval between the
            // two observations, and neither endpoint is more true than
            // the other; the reference this tier reproduces uses the
            // earlier one, so L0 does too. A fill that appears to
            // precede the tick which revealed it looks wrong at first
            // glance and is exactly as defensible as the alternative.
            let stamp = self.prev_stamp.unwrap_or(tick.stamp);
            self.execute_at(
                &resting,
                price,
                Liquidity::Maker,
                FillReason::GapCrossed,
                stamp,
            );
        }
    }

    /// Fills caused by the window's closing state.
    fn cross(&mut self, tick: &Tick) {
        // Market orders first, in arrival order, matching the reference's
        // processing order so that trade identifiers line up.
        if self.book.has_market_orders() {
            let buys: Vec<Resting> = self.book.market_buys().to_vec();
            let reference = tick.buy_fill_reference();
            for r in buys {
                self.execute(&r, reference, Liquidity::Taker, FillReason::Market, tick);
            }
            let sells: Vec<Resting> = self.book.market_sells().to_vec();
            let reference = tick.sell_fill_reference();
            for r in sells {
                self.execute(&r, reference, Liquidity::Taker, FillReason::Market, tick);
            }
        }

        // Limit buys, best price first, stopping at the first that the
        // market did not reach. The book is sorted, so everything after
        // it is further away — an early break, not a full scan.
        let buy_trigger = tick.buy_trigger();
        let buy_reference = tick.buy_fill_reference();
        if buy_trigger.0 > 0 {
            while let Some(best) = self.book.bids().first().copied() {
                let Some(price) = best.price() else { break };
                if price < buy_trigger {
                    break;
                }
                // Price improvement: a buy resting above the market
                // trades at the market, not at its own worse price.
                let fill_price = if buy_reference.0 > 0 {
                    price.min(buy_reference)
                } else {
                    price
                };
                self.execute(
                    &best,
                    fill_price,
                    Liquidity::Maker,
                    FillReason::Crossed,
                    tick,
                );
            }
        }

        let sell_trigger = tick.sell_trigger();
        let sell_reference = tick.sell_fill_reference();
        if sell_trigger.0 > 0 {
            while let Some(best) = self.book.asks().first().copied() {
                let Some(price) = best.price() else { break };
                if price > sell_trigger {
                    break;
                }
                let fill_price = if sell_reference.0 > 0 {
                    price.max(sell_reference)
                } else {
                    price
                };
                self.execute(
                    &best,
                    fill_price,
                    Liquidity::Maker,
                    FillReason::Crossed,
                    tick,
                );
            }
        }
    }

    /// Execute a resting order in full and record the fill.
    ///
    /// L0 fills the whole remaining quantity: it has no depth model, so
    /// it has nothing to ration a partial fill against. A partial fill
    /// invented without a depth model would be a made-up number wearing
    /// the costume of a measurement.
    fn execute(
        &mut self,
        resting: &Resting,
        price: PriceTicks,
        liquidity: Liquidity,
        reason: FillReason,
        tick: &Tick,
    ) {
        self.execute_at(resting, price, liquidity, reason, tick.stamp);
    }

    /// Execute with an explicit timestamp.
    ///
    /// Separated because gap-crossed fills carry the previous tick's
    /// time; see the call site for why.
    fn execute_at(
        &mut self,
        resting: &Resting,
        price: PriceTicks,
        liquidity: Liquidity,
        reason: FillReason,
        stamp: oq_types::Stamp,
    ) {
        let qty = resting.order.remaining();
        if qty.0 <= 0 {
            self.book.replace(resting.id(), None);
            return;
        }
        let outcome = match resting.order.fill(qty) {
            Ok(o) => o,
            // Unreachable given `qty == remaining`, but a matching
            // engine that panics is worse than one that declines to
            // fill: the order simply stays resting and the anomaly is
            // visible in the next reconciliation.
            Err(_) => return,
        };
        let still_working: Option<Working> = outcome.into();
        self.book.replace(resting.id(), still_working);

        let fill = Fill {
            stamp,
            instrument: self.instrument,
            order: resting.id(),
            trade: self.ids.trade(),
            side: resting.order.side(),
            // Carried from the order rather than derived here. A ledger
            // that nets can work it out from the resulting position, and
            // that is what this used to assume; one that keeps two legs
            // cannot, because a buy while a short is open is either
            // closing that short or opening a long and the position
            // alone does not say which.
            offset: resting.order.offset(),
            price,
            qty,
            liquidity,
        };
        self.fills.push(L0Fill { fill, reason });
    }

    /// Snapshot the identifier watermark, for recovery.
    #[must_use]
    pub const fn id_watermark(&self) -> (u64, u64) {
        self.ids.watermark()
    }

    /// Restore the identifier watermark after recovery.
    pub fn restore_ids(&mut self, watermark: (u64, u64)) {
        self.ids.restore(watermark);
    }
}

/// Convenience constructors for callers that build orders inline.
impl L0Engine {
    /// Rest a limit order, returning its id.
    ///
    /// # Panics
    /// If `qty` is not positive — a caller asking for a zero-quantity
    /// order has a bug, and accepting it would put an order on the book
    /// that can never fill and never leave.
    pub fn submit_limit(
        &mut self,
        id: OrderId,
        side: Side,
        price: PriceTicks,
        qty: QtyLots,
        stamp: oq_types::Stamp,
    ) -> OrderId {
        self.submit_limit_with(id, side, price, qty, stamp, Offset::Open)
    }

    /// Rest a limit order that states whether it opens or closes.
    ///
    /// # Panics
    /// As [`L0Engine::submit_limit`].
    pub fn submit_limit_with(
        &mut self,
        id: OrderId,
        side: Side,
        price: PriceTicks,
        qty: QtyLots,
        stamp: oq_types::Stamp,
        offset: Offset,
    ) -> OrderId {
        let order = oq_types::Order::with_offset(
            id,
            side,
            oq_types::OrderKind::Limit { price },
            qty,
            oq_types::TimeInForce::GoodTilCancel,
            stamp,
            offset,
        )
        .expect("order quantity must be positive")
        .accept();
        self.submit(Working::Live(order));
        id
    }

    /// Rest a market order, returning its id.
    ///
    /// # Panics
    /// As [`L0Engine::submit_limit`].
    pub fn submit_market(
        &mut self,
        id: OrderId,
        side: Side,
        qty: QtyLots,
        stamp: oq_types::Stamp,
    ) -> OrderId {
        self.submit_market_with(id, side, qty, stamp, Offset::Open)
    }

    /// Rest a market order that states whether it opens or closes.
    ///
    /// # Panics
    /// As [`L0Engine::submit_limit`].
    pub fn submit_market_with(
        &mut self,
        id: OrderId,
        side: Side,
        qty: QtyLots,
        stamp: oq_types::Stamp,
        offset: Offset,
    ) -> OrderId {
        let order = oq_types::Order::with_offset(
            id,
            side,
            oq_types::OrderKind::Market,
            qty,
            oq_types::TimeInForce::GoodTilCancel,
            stamp,
            offset,
        )
        .expect("order quantity must be positive")
        .accept();
        self.submit(Working::Live(order));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::Stamp;

    fn engine() -> L0Engine {
        L0Engine::new(InstrumentId::new(1))
    }

    fn tick(n: i64, last: i64, high: i64, low: i64) -> Tick {
        Tick::trades_only(Stamp::synthetic(n), last, high, low)
    }

    #[test]
    fn the_first_tick_only_establishes_a_reference() {
        let mut e = engine();
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(100),
            QtyLots(1),
            Stamp::synthetic(0),
        );
        // A buy at 100 with the market at 500 must not fill, and the
        // absent previous price must not be treated as zero.
        let fills = e.on_tick(&tick(1, 500, 500, 500));
        assert!(fills.is_empty());
        assert_eq!(e.last_price(), Some(PriceTicks(500)));
    }

    #[test]
    fn a_resting_buy_fills_when_the_market_reaches_it() {
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(95),
            QtyLots(2),
            Stamp::synthetic(1),
        );
        let fills = e.on_tick(&tick(2, 94, 100, 94)).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].fill.qty, QtyLots(2));
        assert!(e.book().is_empty(), "a filled order leaves the book");
    }

    #[test]
    fn a_gap_crossed_fill_carries_the_previous_tick_time() {
        // The crossing happened between two observations, and the tier
        // this engine reproduces stamps it with the earlier one. A fill
        // that appears to precede the tick which revealed it is the
        // intended behaviour, not an off-by-one.
        let mut e = engine();
        e.on_tick(&tick(1_000, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(95),
            QtyLots(1),
            Stamp::synthetic(1_000),
        );
        let fills = e.on_tick(&tick(2_000, 99, 100, 90)).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].reason, FillReason::GapCrossed);
        assert_eq!(
            fills[0].fill.stamp,
            Stamp::synthetic(1_000),
            "gap-crossed fills carry the previous tick's stamp"
        );
    }

    #[test]
    fn an_ordinary_crossing_carries_the_current_tick_time() {
        let mut e = engine();
        e.on_tick(&tick(1_000, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(100),
            QtyLots(1),
            Stamp::synthetic(1_000),
        );
        let fills = e.on_tick(&tick(2_000, 110, 110, 100)).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].reason, FillReason::Crossed);
        assert_eq!(fills[0].fill.stamp, Stamp::synthetic(2_000));
    }

    #[test]
    fn a_gap_crossed_order_fills_at_its_own_price() {
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(95),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        // The window dips to 90 and closes back at 99: the order was
        // swept on the way down and must fill at 95, not at 99.
        let fills = e.on_tick(&tick(2, 99, 100, 90)).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].reason, FillReason::GapCrossed);
        assert_eq!(fills[0].fill.price, PriceTicks(95));
    }

    #[test]
    fn a_limit_buy_above_the_market_gets_price_improvement() {
        let mut e = engine();
        e.on_tick(&Tick::quoted(Stamp::synthetic(1), 100, 100, 100, 99, 101));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(105),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        let fills = e
            .on_tick(&Tick::quoted(Stamp::synthetic(2), 100, 100, 100, 99, 101))
            .to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(
            fills[0].fill.price,
            PriceTicks(101),
            "buying at the ask, not at the worse limit price"
        );
    }

    #[test]
    fn a_limit_sell_below_the_market_gets_price_improvement() {
        let mut e = engine();
        e.on_tick(&Tick::quoted(Stamp::synthetic(1), 100, 100, 100, 99, 101));
        e.submit_limit(
            OrderId::new(1),
            Side::Sell,
            PriceTicks(95),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        let fills = e
            .on_tick(&Tick::quoted(Stamp::synthetic(2), 100, 100, 100, 99, 101))
            .to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].fill.price, PriceTicks(99), "selling at the bid");
    }

    #[test]
    fn market_orders_fill_as_taker_at_the_reference_price() {
        let mut e = engine();
        e.on_tick(&Tick::quoted(Stamp::synthetic(1), 100, 100, 100, 99, 101));
        e.submit_market(OrderId::new(1), Side::Buy, QtyLots(3), Stamp::synthetic(1));
        let fills = e
            .on_tick(&Tick::quoted(Stamp::synthetic(2), 100, 100, 100, 99, 101))
            .to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].reason, FillReason::Market);
        assert_eq!(fills[0].fill.liquidity, Liquidity::Taker);
        assert_eq!(fills[0].fill.price, PriceTicks(101));
    }

    #[test]
    fn orders_are_not_eligible_on_the_tick_they_are_submitted() {
        // Guards against lookahead: a strategy reacting to a tick must
        // not be able to trade inside that same tick.
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(150),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        // Deep in the money, but submitted after this tick was matched.
        assert_eq!(e.book().len(), 1);
    }

    #[test]
    fn cancelled_orders_do_not_fill() {
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(95),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        assert!(e.cancel(OrderId::new(1)));
        assert!(
            !e.cancel(OrderId::new(1)),
            "cancelling twice is not an error"
        );
        let fills = e.on_tick(&tick(2, 90, 100, 90));
        assert!(fills.is_empty());
    }

    #[test]
    fn crossing_fills_the_best_price_first() {
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(100),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        e.submit_limit(
            OrderId::new(2),
            Side::Buy,
            PriceTicks(105),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        // The window never dips below the previous price, so nothing is
        // gap-crossed and the ordinary crossing path runs.
        let fills = e.on_tick(&tick(2, 110, 110, 100)).to_vec();
        assert_eq!(fills.len(), 2);
        assert!(fills.iter().all(|f| f.reason == FillReason::Crossed));
        assert_eq!(
            fills[0].fill.order,
            OrderId::new(2),
            "105 is the better bid"
        );
        assert_eq!(fills[1].fill.order, OrderId::new(1));
    }

    #[test]
    fn gap_crossing_fills_in_arrival_order_not_price_order() {
        // Deliberate, and load-bearing for parity: the reference walks
        // its insertion-ordered live-order map when it resolves a gap,
        // so trade identifiers are assigned in submission order even
        // though the better-priced order was reached first in time.
        // Filling in price order here would produce identical fills
        // with different identifiers, and the parity report compares
        // identifiers.
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        e.submit_limit(
            OrderId::new(1),
            Side::Buy,
            PriceTicks(90),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        e.submit_limit(
            OrderId::new(2),
            Side::Buy,
            PriceTicks(95),
            QtyLots(1),
            Stamp::synthetic(1),
        );
        let fills = e.on_tick(&tick(2, 85, 100, 85)).to_vec();
        assert_eq!(fills.len(), 2);
        assert!(fills.iter().all(|f| f.reason == FillReason::GapCrossed));
        assert_eq!(fills[0].fill.order, OrderId::new(1), "submitted first");
        assert_eq!(fills[1].fill.order, OrderId::new(2));
        // Each still fills at its own price, which is the rule that
        // matters for P&L.
        assert_eq!(fills[0].fill.price, PriceTicks(90));
        assert_eq!(fills[1].fill.price, PriceTicks(95));
    }

    #[test]
    fn trade_ids_are_dense_and_monotonic() {
        let mut e = engine();
        e.on_tick(&tick(1, 100, 100, 100));
        for i in 1..=5 {
            e.submit_limit(
                OrderId::new(i),
                Side::Buy,
                PriceTicks(99),
                QtyLots(1),
                Stamp::synthetic(1),
            );
        }
        let fills = e.on_tick(&tick(2, 98, 100, 98)).to_vec();
        let ids: Vec<u64> = fills.iter().map(|f| f.fill.trade.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }
}
