//! The live process's own books, kept by the kernel a backtest uses.
//!
//! # The defect this exists to fix
//!
//! Until this module the live loop built its strategy's `Context` from
//! literal zeros:
//!
//! ```text
//! position: QtyLots(0),
//! entry: PriceTicks(0),
//! equity: Cash(0),
//! ```
//!
//! Every example strategy in this repository decides whether to open or
//! close by reading `ctx.position`. Live, all of them saw zero on every
//! observation, forever — so a strategy that opens when flat opens
//! again on the next tick, and one that closes when long never closes.
//! It is the same shape as the hardcoded position the risk gate was
//! found with on a real venue, one layer up, and it survived because
//! nothing live had a position to be wrong about yet.
//!
//! # Why this is the kernel and not a tally
//!
//! Counting fills into an integer would have fixed the symptom in ten
//! lines. It would also have been a second implementation of position,
//! entry price, fees, funding and equity — and `WHY.md`'s third wall is
//! that the predecessor had exactly two implementations that were
//! *supposed* to agree, with nothing enforcing it, and that a matching
//! defect had to be fixed twice because two engines disagreed.
//!
//! So the books are `oq_core::Kernel` in [`Matching::Venue`], which is
//! the same kernel a backtest runs with the same accounting. The venue
//! decides which orders trade; everything downstream of that decision is
//! one implementation.
//!
//! # What it is not
//!
//! # Fills are deduplicated by the venue's trade id
//!
//! A reconnecting stream repeats what it already said — that is routine
//! rather than exotic, and `oq-sim`'s corpus has a scenario for it. A
//! set of books that applied a redelivered fill would double the
//! position, and the second copy is indistinguishable from the first.
//!
//! A fill with **no** trade id is refused rather than applied. It cannot
//! be deduplicated, so accepting it means accepting an unbounded number
//! of copies of one trade; and a position that is too large because of
//! a redelivery looks exactly like a position that is too large because
//! of a bug. Only [`Books::adopt`] bypasses this, and it does so through
//! its own path — a position adopted at startup answers to no venue
//! trade at all.
//!
//! # What it is not
//!
//! Not the source of truth about the account. The venue is, and
//! [`Books::reconcile`] exists because the kernel's view and the
//! venue's can differ — a fill this process never heard about, a
//! position adjusted by something nobody here sent. It reports the
//! difference rather than silently adopting either side, because
//! `FR-RISK-4` makes unknown state fatal and a set of books that quietly
//! corrected itself would have destroyed the evidence.

use oq_core::kernel::Matching;
use oq_core::{Event, Kernel, Output, State};
use oq_engine::Tick;
use oq_margin::{Contract, TierTable};
use oq_strategy::Context;
use oq_types::{Cash, Fill, InstrumentId, Nanos, Offset, OrderId, PriceTicks, QtyLots, Side};

/// A disagreement between these books and the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    /// What this process believes it holds, long minus short.
    pub ours: QtyLots,
    /// What the venue says.
    pub theirs: QtyLots,
    /// When the comparison was made.
    pub at: Nanos,
}

impl Mismatch {
    /// How far out, signed the venue's way.
    #[must_use]
    pub const fn drift(&self) -> QtyLots {
        QtyLots(self.theirs.0 - self.ours.0)
    }
}

/// What happened to a fill the venue reported.
#[derive(Debug, Clone, PartialEq)]
pub enum Booked {
    /// Applied, with whatever the kernel decided.
    Applied(Vec<Output>),
    /// This trade id was already booked. The books did not move.
    Duplicate,
    /// The report carries no trade id, so it cannot be deduplicated and
    /// was not applied.
    Unidentifiable,
}

/// The live account, kept by the kernel.
pub struct Books {
    kernel: Kernel,
    instrument: InstrumentId,
    /// Venue trade ids already booked.
    seen: std::collections::HashSet<u64>,
    /// Orders submitted and not yet resolved, so `Context::working` is
    /// the process's own count rather than a guess.
    working: usize,
}

impl Books {
    /// Open a set of books for one instrument.
    ///
    /// `starting_balance` is what the venue says the account holds at
    /// startup. It is a statement of fact from the venue rather than a
    /// configured number: books opened at a balance nobody checked would
    /// report an equity curve about a different account.
    #[must_use]
    pub fn new(
        instrument: InstrumentId,
        contract: Contract,
        table: TierTable,
        starting_balance: Cash,
        mode: oq_core::PositionMode,
    ) -> Self {
        let mut state = State::new(instrument, contract, table, starting_balance);
        // The venue is matching. Not a configuration choice — it is what
        // makes these books the live account's rather than a simulation
        // running alongside it.
        state.matching = Matching::Venue;
        // Whether the account keeps long and short apart, as the venue
        // reports it. Defaulting to netting on an account that does not
        // net is not a small error: two legs of equal size cancel, the
        // books report a flat account, and the margin the venue charges
        // for both is charged against a position this side believes is
        // zero. Everything downstream reads that zero — the liquidation
        // check, the equity, and any strategy that asks what it holds.
        state.mode = mode;
        Self {
            kernel: Kernel::new(state),
            instrument,
            seen: std::collections::HashSet::new(),
            working: 0,
        }
    }

    /// Adopt a position the venue already holds.
    ///
    /// Used at startup beside an existing position, and by the cutover
    /// procedure's step 5. Expressed as a fill rather than by writing
    /// the fields, so the entry price, the fees and the equity are
    /// computed by the same code that computes them for every other
    /// fill — a position installed by assignment would be the one
    /// position in the run whose accounting nobody checked.
    pub fn adopt(&mut self, side: Side, qty: QtyLots, entry: PriceTicks, at: Nanos) {
        if qty.0 <= 0 {
            return;
        }
        self.kernel.apply(&Event::VenueFill(Fill {
            stamp: oq_types::Stamp::new(at.0, at.0),
            instrument: self.instrument,
            // Zero: this fill answers to no order this process sent, and
            // an id borrowed from one would attach it to an order that
            // is not this.
            order: OrderId(0),
            trade: oq_types::TradeId(0),
            side,
            offset: Offset::Open,
            price: entry,
            qty,
            liquidity: oq_types::Liquidity::Taker,
        }));
    }

    /// Fold an observation into the books.
    ///
    /// Returns anything the kernel decided — which under venue matching
    /// is a liquidation and nothing else, since it does not fill.
    pub fn on_tick(&mut self, tick: &Tick) -> Vec<Output> {
        self.kernel.apply(&Event::Tick(*tick)).to_vec()
    }

    /// Record that an order was sent.
    pub fn on_submit(&mut self, id: OrderId, side: Side, qty: QtyLots, offset: Offset, at: Nanos) {
        self.working += 1;
        self.kernel.apply(&Event::Submit {
            id,
            side,
            // A live order's resting price is the venue's business; what
            // these books need from a submit is that the order exists.
            price: None,
            qty,
            offset,
            stamp: oq_types::Stamp::new(at.0, at.0),
        });
    }

    /// Record that an order ended without filling.
    pub fn on_closed(&mut self) {
        self.working = self.working.saturating_sub(1);
    }

    /// Book a fill the venue reported.
    ///
    /// Returns [`Booked::Duplicate`] for a trade already applied and
    /// [`Booked::Unidentifiable`] for one with no trade id — neither
    /// changes the books. A redelivered fill is routine after a
    /// reconnect, and applying one would double a position in a way
    /// indistinguishable from a bug.
    pub fn on_venue_fill(&mut self, fill: &Fill) -> Booked {
        if fill.trade.0 == 0 {
            // Not deduplicable, so accepting it means accepting an
            // unbounded number of copies of one trade.
            return Booked::Unidentifiable;
        }
        if !self.seen.insert(fill.trade.0) {
            return Booked::Duplicate;
        }
        self.working = self.working.saturating_sub(1);
        Booked::Applied(self.kernel.apply(&Event::VenueFill(*fill)).to_vec())
    }

    /// Distinct trades booked.
    #[must_use]
    pub fn booked(&self) -> usize {
        self.seen.len()
    }

    /// The strategy's view, for this observation.
    ///
    /// The whole reason this module exists: every field here was a
    /// literal zero, and a strategy reading `ctx.position` to decide
    /// whether to open or close was reading a constant.
    #[must_use]
    pub fn context(&self, tick: Tick) -> Context {
        let s = self.kernel.summary();
        Context {
            tick,
            position: s.qty,
            entry: s.entry,
            short_position: s.short_qty,
            short_entry: s.short_entry,
            equity: s.equity,
            working: self.working,
        }
    }

    /// Net position, long minus short.
    #[must_use]
    pub fn net_position(&self) -> QtyLots {
        let s = self.kernel.summary();
        QtyLots(s.qty.0 - s.short_qty.0)
    }

    /// Realized P&L net of fees and funding, as the venue's fills made
    /// it.
    ///
    /// The live half of the attribution gap. It has to come from these
    /// books and not from the shadow's: the two are fed different fill
    /// streams — this one the venue's, the other the model's — and a gap
    /// computed from one source is a number subtracted from itself.
    #[must_use]
    pub fn realized_net(&self) -> Cash {
        let s = self.kernel.state();
        Cash(s.realized.0 - s.fees.0 + s.funding.0)
    }

    /// Equity at the last mark seen.
    #[must_use]
    pub fn equity(&self) -> Cash {
        self.kernel.summary().equity
    }

    /// Compare against what the venue says it holds.
    ///
    /// `None` means they agree. A difference is **reported, not
    /// corrected**: `FR-RISK-4` makes unknown state fatal, and books
    /// that quietly adopted the venue's number would have destroyed the
    /// evidence of how they came to differ — which is the only thing
    /// that could have explained it.
    #[must_use]
    pub fn reconcile(&self, venue: QtyLots, at: Nanos) -> Option<Mismatch> {
        let ours = self.net_position();
        (ours != venue).then_some(Mismatch {
            ours,
            theirs: venue,
            at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{Liquidity, Stamp, TradeId};

    const SEC: i64 = 1_000_000_000;

    fn books() -> Books {
        Books::new(
            InstrumentId::new(1),
            Contract::new(10_000),
            TierTable::example_btcusdt(),
            Cash::from_units(100_000),
            // These exercise the netting path, which is what they were
            // written against.
            oq_core::PositionMode::OneWay,
        )
    }

    fn tick(ns: i64, price: i64) -> Tick {
        Tick {
            stamp: Stamp::new(ns, ns),
            last: PriceTicks(price),
            high: PriceTicks(price),
            low: PriceTicks(price),
            bid: PriceTicks(price - 1),
            ask: PriceTicks(price + 1),
            volume: QtyLots(0),
        }
    }

    fn fill(ns: i64, order: u64, side: Side, price: i64, qty: i64, offset: Offset) -> Fill {
        Fill {
            stamp: Stamp::new(ns, ns),
            instrument: InstrumentId::new(1),
            order: OrderId(order),
            // Distinct per fill: the books deduplicate by trade id, so
            // a fixture that reused one would silently test the
            // deduplication rather than whatever it meant to test.
            trade: TradeId(order * 1_000 + ns.unsigned_abs()),
            side,
            offset,
            price: PriceTicks(price),
            qty: QtyLots(qty),
            liquidity: Liquidity::Taker,
        }
    }

    /// **The defect this module exists for.** Every example strategy
    /// decides whether to open or close by reading `ctx.position`, and
    /// live it was a literal zero on every observation forever — so a
    /// strategy that opens when flat opened again on the next tick, and
    /// one that closes when long never closed.
    #[test]
    fn the_context_reflects_the_position_rather_than_a_constant() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        assert_eq!(b.context(tick(SEC, 6_000_000)).position, QtyLots(0));

        b.on_venue_fill(&fill(2 * SEC, 1, Side::Buy, 6_000_000, 4, Offset::Open));
        let ctx = b.context(tick(3 * SEC, 6_010_000));

        assert_eq!(
            ctx.position,
            QtyLots(4),
            "the strategy must see its position"
        );
        assert_eq!(ctx.entry, PriceTicks(6_000_000), "and where it got in");
        assert_ne!(ctx.equity, Cash(0), "and what the account is worth");
    }

    /// Equity follows the mark, which is what makes a live strategy able
    /// to size against its own account rather than against a number
    /// frozen at startup.
    #[test]
    fn equity_moves_with_the_market() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        b.on_venue_fill(&fill(SEC, 1, Side::Buy, 6_000_000, 10, Offset::Open));
        let flat = b.equity();

        b.on_tick(&tick(2 * SEC, 6_100_000));
        let up = b.equity();
        b.on_tick(&tick(3 * SEC, 5_900_000));
        let down = b.equity();

        assert!(up.0 > flat.0, "a long into a rising market gains");
        assert!(down.0 < up.0, "and gives it back");
    }

    /// A position adopted at startup must be accounted for by the same
    /// code that accounts for every other fill. One installed by writing
    /// the fields would be the only position in the run whose entry
    /// price, fees and equity nobody computed.
    #[test]
    fn an_adopted_position_is_accounted_like_any_other() {
        let mut adopted = books();
        adopted.on_tick(&tick(SEC, 6_000_000));
        adopted.adopt(Side::Buy, QtyLots(7), PriceTicks(5_950_000), Nanos(SEC));
        adopted.on_tick(&tick(2 * SEC, 6_000_000));

        let mut filled = books();
        filled.on_tick(&tick(SEC, 6_000_000));
        filled.on_venue_fill(&fill(SEC, 0, Side::Buy, 5_950_000, 7, Offset::Open));
        filled.on_tick(&tick(2 * SEC, 6_000_000));

        let a = adopted.context(tick(2 * SEC, 6_000_000));
        let f = filled.context(tick(2 * SEC, 6_000_000));
        assert_eq!(a.position, f.position);
        assert_eq!(a.entry, f.entry);
        assert_eq!(a.equity, f.equity, "including the fee it paid");
    }

    /// Closing reduces, rather than adding a second position in the
    /// other direction. Getting this wrong is how a flat account reports
    /// two open legs.
    #[test]
    fn a_close_reduces_the_position() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        b.on_venue_fill(&fill(SEC, 1, Side::Buy, 6_000_000, 5, Offset::Open));
        b.on_venue_fill(&fill(2 * SEC, 2, Side::Sell, 6_010_000, 5, Offset::Close));
        assert_eq!(b.net_position(), QtyLots(0), "flat again");
    }

    /// The working count is the process's own, and a strategy sizing
    /// against a stale one would place orders it thought it had not.
    #[test]
    fn the_working_count_follows_what_this_process_sent() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        assert_eq!(b.context(tick(SEC, 6_000_000)).working, 0);

        b.on_submit(OrderId(1), Side::Buy, QtyLots(1), Offset::Open, Nanos(SEC));
        b.on_submit(OrderId(2), Side::Buy, QtyLots(1), Offset::Open, Nanos(SEC));
        assert_eq!(b.context(tick(SEC, 6_000_000)).working, 2);

        b.on_venue_fill(&fill(2 * SEC, 1, Side::Buy, 6_000_000, 1, Offset::Open));
        b.on_closed();
        assert_eq!(b.context(tick(2 * SEC, 6_000_000)).working, 0);
    }

    /// A count that has already reached zero must not go negative when
    /// a duplicate report arrives — a redelivered cancel is routine, and
    /// an underflowed counter would report a working set of billions.
    #[test]
    fn a_duplicate_ending_does_not_underflow_the_count() {
        let mut b = books();
        b.on_closed();
        b.on_closed();
        assert_eq!(b.context(tick(SEC, 6_000_000)).working, 0);
    }

    /// FR-RISK-4 makes unknown state fatal. Books that quietly adopted
    /// the venue's number would have destroyed the evidence of how they
    /// came to differ, which is the only thing that could explain it.
    #[test]
    fn a_disagreement_with_the_venue_is_reported_and_not_corrected() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        b.on_venue_fill(&fill(SEC, 1, Side::Buy, 6_000_000, 3, Offset::Open));

        assert_eq!(
            b.reconcile(QtyLots(3), Nanos(2 * SEC)),
            None,
            "agreement is silent"
        );

        let m = b
            .reconcile(QtyLots(5), Nanos(2 * SEC))
            .expect("a difference must be reported");
        assert_eq!((m.ours, m.theirs), (QtyLots(3), QtyLots(5)));
        assert_eq!(m.drift(), QtyLots(2));
        assert_eq!(
            b.net_position(),
            QtyLots(3),
            "and the books must not have moved themselves"
        );
    }

    /// Under venue matching the kernel never fills, so a price walking
    /// past where an order was sent must not produce one. If it did, the
    /// live process would book trades the venue never made.
    #[test]
    fn the_books_never_invent_a_fill() {
        let mut b = books();
        b.on_submit(OrderId(1), Side::Buy, QtyLots(5), Offset::Open, Nanos(SEC));
        for i in 1..20 {
            let outputs = b.on_tick(&tick(i * SEC, 6_000_000 - i * 1_000));
            assert!(
                !outputs.iter().any(|o| matches!(o, Output::Filled(_))),
                "the books filled an order the venue never reported, at tick {i}"
            );
        }
        assert_eq!(b.net_position(), QtyLots(0));
    }
    /// A redelivered fill must not double the position. A reconnecting
    /// stream repeats what it already said, which is routine.
    #[test]
    fn a_redelivered_fill_does_not_double_the_position() {
        let mut b = books();
        b.on_tick(&tick(SEC, 6_000_000));
        let f = fill(2 * SEC, 1, Side::Buy, 6_000_000, 4, Offset::Open);
        b.on_venue_fill(&f);
        let after_first = b.net_position();
        b.on_venue_fill(&f);
        assert_eq!(
            b.net_position(),
            after_first,
            "the same trade arrived twice and was booked twice"
        );
    }
}
