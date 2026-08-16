//! The state machine: `apply(State, Event) -> Outputs`.
//!
//! This is the whole of the core's contract. It has no clock, no
//! randomness, no I/O, and no threads. Every input is an [`Event`] and
//! every result is an [`Output`]. Two consequences follow, and they are
//! the reason the architecture is shaped this way:
//!
//! - **Replay is exact.** Feeding a journal back through a fresh kernel
//!   reproduces the original state and the original outputs, byte for
//!   byte. Debugging becomes reading rather than guessing, and recovery
//!   after a crash is the same code path as a normal start.
//! - **Testing can be exhaustive.** A deterministic state machine can
//!   be driven through generated scenarios with a seed, and any failure
//!   reproduces from `(seed, commit)` alone. That is a different
//!   activity from writing example tests by hand, and it only works if
//!   nothing in the core can quietly consult the outside world.
//!
//! Margin is applied on every tick rather than at settlement points
//! only: a position that becomes liquidatable between two funding
//! settlements is liquidated then, not later, which is what the venue
//! does and what a backtest that only checks at settlement misses.

use crate::event::Event;
use oq_engine::{L0Engine, Tick};
use oq_margin::{Contract, FundingRate, MarginedPosition, TierTable};
use oq_types::{Cash, Fill, InstrumentId, Nanos, OrderId, PriceTicks, QtyLots, Side, Working};

/// Something the core produced.
///
/// Outputs are values, not callbacks. A callback could re-enter the
/// core and mutate state mid-decision; a value cannot, so the order of
/// effects is a property of the code rather than of who happened to
/// call whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// An order executed.
    Filled(Fill),
    /// An order was withdrawn.
    Cancelled(OrderId),
    /// A submission was refused.
    Rejected { id: OrderId, reason: RejectReason },
    /// Funding settled against the position.
    Funded { amount: Cash, at: Nanos },
    /// The venue closed the position.
    ///
    /// Carries the price at which it happened, so a report can show the
    /// path rather than only the outcome.
    Liquidated {
        at: Nanos,
        price: PriceTicks,
        qty: QtyLots,
        equity: Cash,
    },
}

/// Why a submission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Quantity was not positive.
    NonPositiveQty,
    /// An order with this id is already working.
    DuplicateId,
    /// The account has no collateral.
    NoMargin,
}

/// What the venue charges per fill.
///
/// Recorded per side because the two differ by an order of magnitude,
/// and a strategy that rests orders earns a different schedule from one
/// that crosses the spread. A maker rate may be negative — a rebate —
/// and the arithmetic must carry that through rather than clamping it,
/// because being paid to provide liquidity is the entire economics of
/// some strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fees {
    pub maker: oq_types::Ratio,
    pub taker: oq_types::Ratio,
}

impl Fees {
    #[must_use]
    pub const fn flat(rate: oq_types::Ratio) -> Self {
        Self {
            maker: rate,
            taker: rate,
        }
    }

    #[must_use]
    pub const fn none() -> Self {
        Self {
            maker: oq_types::Ratio::ZERO,
            taker: oq_types::Ratio::ZERO,
        }
    }

    /// The charge for a fill. Positive is a cost.
    #[must_use]
    pub const fn charge(&self, contract: Contract, fill: &Fill) -> Cash {
        let notional = contract.notional(fill.price, fill.qty);
        let rate = match fill.liquidity {
            oq_types::Liquidity::Maker => self.maker,
            oq_types::Liquidity::Taker => self.taker,
        };
        notional.scaled(rate)
    }
}

/// Account and market state for one instrument.
#[derive(Debug)]
pub struct State {
    pub engine: L0Engine,
    pub contract: Contract,
    pub table: TierTable,
    /// Signed position; zero when flat.
    pub qty: QtyLots,
    /// Average entry price of the open position.
    pub entry: PriceTicks,
    /// Free collateral plus position margin.
    pub balance: Cash,
    /// Realized profit and loss, cumulative.
    pub realized: Cash,
    /// Funding paid (negative) or received (positive), cumulative.
    pub funding: Cash,
    /// Trading fees charged, cumulative and positive.
    ///
    /// Tracked separately from realized profit because a strategy whose
    /// gross edge is real and whose net result is negative is a
    /// different problem from one with no edge, and a single number
    /// cannot tell them apart.
    pub fees: Cash,
    /// What the venue charges.
    pub fee_schedule: Fees,
    /// The last time the core was told about.
    pub now: Nanos,
    /// The most recent traded price.
    pub mark: PriceTicks,
    /// Whether the venue is allowed to close the account.
    ///
    /// Enabled in anything describing a real account. Disabling it
    /// models an account with unlimited collateral — which no venue
    /// offers, and which is what a backtest without a margin model
    /// silently assumes. It exists so that assumption can be run as the
    /// control arm of an experiment rather than left implicit.
    pub enforce_liquidation: bool,
}

impl State {
    #[must_use]
    pub fn new(
        instrument: InstrumentId,
        contract: Contract,
        table: TierTable,
        starting_balance: Cash,
    ) -> Self {
        Self {
            engine: L0Engine::new(instrument),
            contract,
            table,
            qty: QtyLots::ZERO,
            entry: PriceTicks::ZERO,
            balance: starting_balance,
            realized: Cash::ZERO,
            funding: Cash::ZERO,
            now: Nanos::ZERO,
            mark: PriceTicks::ZERO,
            fees: Cash::ZERO,
            fee_schedule: Fees::none(),
            enforce_liquidation: true,
        }
    }

    /// The same state with a fee schedule.
    ///
    /// Fees default to zero and must be set deliberately. That is the
    /// safer default only because the alternative — a plausible-looking
    /// rate nobody chose — produces a result that is wrong in a way no
    /// reader can see. A run with no fees is at least obviously a run
    /// with no fees.
    #[must_use]
    pub const fn with_fees(mut self, fees: Fees) -> Self {
        self.fee_schedule = fees;
        self
    }

    /// The same state with liquidation disabled.
    ///
    /// Named for what it is rather than for a mode: a caller reading
    /// `without_liquidation()` at a call site cannot mistake it for a
    /// performance option.
    #[must_use]
    pub fn without_liquidation(mut self) -> Self {
        self.enforce_liquidation = false;
        self
    }

    /// The position as the margin model sees it.
    #[must_use]
    pub fn position(&self) -> MarginedPosition {
        MarginedPosition::new(self.contract, self.entry, self.qty, self.balance)
    }

    /// Account equity at the current mark.
    #[must_use]
    pub fn equity(&self) -> Cash {
        self.position().equity(self.mark)
    }

    /// Unrealized profit at the current mark.
    #[must_use]
    pub fn unrealized(&self) -> Cash {
        if self.qty.is_zero() {
            Cash::ZERO
        } else {
            self.contract.unrealized(self.entry, self.mark, self.qty)
        }
    }

    /// Apply a fill to the position, realizing profit on the part that
    /// reduces exposure.
    ///
    /// Handles the case a naive implementation gets wrong: a fill that
    /// crosses through flat — closing a long and opening a short in one
    /// execution — realizes on the closed part only, and the new
    /// position's entry price is the fill price rather than a blend of
    /// the two sides.
    fn apply_fill(&mut self, fill: &Fill) {
        let signed = fill.position_delta();
        let old = self.qty;
        let new = old.add(signed);

        let reduces = old.0 != 0 && (old.0 > 0) != (signed.0 > 0);
        if reduces {
            let closed = signed.0.abs().min(old.0.abs());
            let closed_signed = QtyLots(closed * if old.0 > 0 { 1 } else { -1 });
            let pnl = self
                .contract
                .unrealized(self.entry, fill.price, closed_signed);
            self.realized = self.realized.add(pnl);
            self.balance = self.balance.add(pnl);
        }

        if new.0 == 0 {
            self.entry = PriceTicks::ZERO;
        } else if old.0 == 0 || (old.0 > 0) == (new.0 > 0) && !reduces {
            // Opening or adding: the entry price is the size-weighted
            // average, which is what the venue reports and what every
            // margin calculation is written against.
            let old_notional = self.entry.0 as i128 * old.0.abs() as i128;
            let add_notional = fill.price.0 as i128 * signed.0.abs() as i128;
            let total = old.0.abs() as i128 + signed.0.abs() as i128;
            if total > 0 {
                self.entry = PriceTicks(((old_notional + add_notional) / total) as i64);
            }
        } else if (old.0 > 0) != (new.0 > 0) {
            // Crossed through flat: the remainder is a new position at
            // the fill price, not a blend with the side just closed.
            self.entry = fill.price;
        }
        self.qty = new;
    }

    /// Close the position at `price` because the venue liquidated it.
    fn liquidate(&mut self, price: PriceTicks) -> Output {
        let qty = self.qty;
        let pnl = self.contract.unrealized(self.entry, price, qty);
        self.realized = self.realized.add(pnl);
        self.balance = self.balance.add(pnl);
        let equity = self.balance;
        self.qty = QtyLots::ZERO;
        self.entry = PriceTicks::ZERO;
        self.engine.cancel_all();
        Output::Liquidated {
            at: self.now,
            price,
            qty,
            equity,
        }
    }
}

/// The deterministic core.
#[derive(Debug)]
pub struct Kernel {
    state: State,
    outputs: Vec<Output>,
    working: Vec<OrderId>,
}

impl Kernel {
    #[must_use]
    pub fn new(state: State) -> Self {
        Self {
            state,
            outputs: Vec::with_capacity(16),
            working: Vec::with_capacity(16),
        }
    }

    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }

    /// Apply one event and return what it produced.
    ///
    /// The returned slice borrows an internal buffer that the next call
    /// overwrites.
    pub fn apply(&mut self, event: &Event) -> &[Output] {
        self.outputs.clear();
        match *event {
            Event::Tick(tick) => self.on_tick(&tick),
            Event::Submit {
                id,
                side,
                price,
                qty,
                offset,
                stamp,
            } => {
                if qty.0 <= 0 {
                    self.outputs.push(Output::Rejected {
                        id,
                        reason: RejectReason::NonPositiveQty,
                    });
                } else if self.working.contains(&id) {
                    self.outputs.push(Output::Rejected {
                        id,
                        reason: RejectReason::DuplicateId,
                    });
                } else if self.state.balance.0 <= 0 {
                    // A liquidated account does not get to keep trading.
                    self.outputs.push(Output::Rejected {
                        id,
                        reason: RejectReason::NoMargin,
                    });
                } else {
                    match price {
                        Some(p) => {
                            self.state
                                .engine
                                .submit_limit_with(id, side, p, qty, stamp, offset);
                        }
                        None => {
                            self.state
                                .engine
                                .submit_market_with(id, side, qty, stamp, offset);
                        }
                    }
                    self.working.push(id);
                }
            }
            Event::Cancel { id, .. } => {
                if self.state.engine.cancel(id) {
                    self.working.retain(|w| *w != id);
                    self.outputs.push(Output::Cancelled(id));
                }
            }
            Event::Funding { at, rate, mark } => {
                self.state.now = at;
                if !self.state.qty.is_zero() {
                    let settlement = FundingRate::new(at, rate, mark)
                        .settle(self.state.contract, self.state.qty);
                    self.state.balance = self.state.balance.add(settlement.amount);
                    self.state.funding = self.state.funding.add(settlement.amount);
                    self.outputs.push(Output::Funded {
                        amount: settlement.amount,
                        at,
                    });
                    // Funding can be what pushes a position over the
                    // edge, so the check runs here too rather than
                    // waiting for the next tick.
                    self.check_liquidation(mark);
                }
            }
            Event::Time(at) => self.state.now = at,
            Event::MarginDeposit { amount, at } => {
                self.state.now = at;
                self.state.balance = self.state.balance.add(Cash(amount));
            }
        }
        &self.outputs
    }

    fn on_tick(&mut self, tick: &Tick) {
        self.state.now = tick.stamp.exch;
        self.state.mark = tick.last;

        // Copy out before touching state: the engine's buffer is
        // reused, and applying a fill mutates the state the next fill
        // is computed against.
        let fills: Vec<Fill> = self
            .state
            .engine
            .on_tick(tick)
            .iter()
            .map(|f| f.fill)
            .collect();

        for fill in &fills {
            // Charged before the position update so the fee is computed
            // against the fill that incurred it, not against whatever
            // the position became afterwards.
            let fee = self.state.fee_schedule.charge(self.state.contract, fill);
            self.state.fees = self.state.fees.add(fee);
            self.state.balance = self.state.balance.sub(fee);
            self.state.apply_fill(fill);
            self.working.retain(|w| *w != fill.order);
            self.outputs.push(Output::Filled(*fill));
        }

        self.check_liquidation(tick.last);
    }

    /// Close the position if the venue would have.
    ///
    /// Checked on every tick, not only at settlement instants: a
    /// position that becomes liquidatable mid-window is liquidated
    /// there, and a model that only checks periodically reports
    /// survival through moves that ended the account.
    fn check_liquidation(&mut self, mark: PriceTicks) {
        if !self.state.enforce_liquidation || self.state.qty.is_zero() {
            return;
        }
        let position = self.state.position();
        if position.is_liquidatable(&self.state.table, mark) {
            let out = self.state.liquidate(mark);
            self.working.clear();
            self.outputs.push(out);
        }
    }

    /// Orders currently working.
    #[must_use]
    pub fn working(&self) -> &[OrderId] {
        &self.working
    }

    /// Rest an order directly, bypassing the event path.
    ///
    /// For tests and for adapters that have already validated the
    /// order; the event path is what a journal replays.
    pub fn submit_raw(&mut self, order: Working) {
        self.working.push(order.id());
        self.state.engine.submit(order);
    }

    /// Everything a replay must reproduce.
    ///
    /// Use this in equivalence assertions rather than [`Kernel::summary`],
    /// which is a projection and cannot see the order book.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            summary: self.summary(),
            book: self
                .state
                .engine
                .book()
                .iter()
                .map(|r| (r.id(), r.order.remaining().0))
                .collect(),
            working: self.working.clone(),
            enforce_liquidation: self.state.enforce_liquidation,
            id_watermark: self.state.engine.id_watermark(),
        }
    }

    /// A compact summary of account state, for reports and assertions.
    #[must_use]
    pub fn summary(&self) -> Summary {
        Summary {
            qty: self.state.qty,
            entry: self.state.entry,
            balance: self.state.balance,
            realized: self.state.realized,
            funding: self.state.funding,
            fees: self.state.fees,
            equity: self.state.equity(),
            mark: self.state.mark,
            now: self.state.now,
        }
    }
}

/// Everything about a kernel that a replay must reproduce.
///
/// The determinism test compares this rather than the account summary.
/// A summary is a projection — eight aggregates — and a replay that
/// rebuilt the order book incorrectly, or not at all, would still match
/// it. Comparing a fingerprint that is *derived from the whole state*
/// means the test does not have to be updated every time state grows,
/// which is exactly the maintenance failure that lets such a gap open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub summary: Summary,
    /// Resting orders, in book order, with their remaining quantity.
    pub book: Vec<(OrderId, i64)>,
    /// Orders the kernel considers working.
    pub working: Vec<OrderId>,
    pub enforce_liquidation: bool,
    /// Identifier watermark, so a replay that skipped an id is caught.
    pub id_watermark: (u64, u64),
}

/// Account state at a point in the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub qty: QtyLots,
    pub entry: PriceTicks,
    pub balance: Cash,
    pub realized: Cash,
    pub funding: Cash,
    pub fees: Cash,
    pub equity: Cash,
    pub mark: PriceTicks,
    pub now: Nanos,
}

/// Convenience: a buy is a positive delta, a sell negative.
#[must_use]
pub const fn signed(side: Side, qty: QtyLots) -> QtyLots {
    QtyLots(qty.0 * side.sign())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_margin::{MarginTier, TierTable};
    use oq_types::{Ratio, Stamp};

    const BTC: Contract = Contract::new(10_000);

    fn table() -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    fn kernel(balance_units: i64) -> Kernel {
        Kernel::new(State::new(
            InstrumentId::new(1),
            BTC,
            table(),
            Cash::from_units(balance_units),
        ))
    }

    fn tick(n: i64, last: i64) -> Event {
        Event::Tick(Tick::trades_only(Stamp::synthetic(n), last, last, last))
    }

    fn buy(id: u64, price: i64, qty: i64, n: i64) -> Event {
        Event::Submit {
            id: OrderId::new(id),
            side: Side::Buy,
            price: Some(PriceTicks(price)),
            qty: QtyLots(qty),
            stamp: Stamp::synthetic(n),
            offset: oq_types::Offset::Open,
        }
    }

    #[test]
    fn fees_are_charged_per_fill_and_tracked_separately() {
        // The gap a 608-day run surfaced: fills were free. Over a
        // window with hundreds of round trips on a doubling ladder, the
        // omission is not a rounding error — it was an order of
        // magnitude on the reported result.
        let mut k = Kernel::new(
            State::new(InstrumentId::new(1), BTC, table(), Cash::from_units(10_000))
                .with_fees(Fees::flat(Ratio::from_ppm(500))), // 0.05%
        );
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));

        // The fixture's tick-lot is 0.0001 of quote currency, so
        // 1_000_000 ticks of price on 10 lots is 1000 USDT of notional,
        // and 0.05% of that is 0.50 USDT.
        let s = k.summary();
        assert_eq!(s.fees, Cash::from_units(1).scaled(Ratio::from_percent(50)));
        assert!(s.fees.0 > 0, "a fee is a cost, recorded positive");
        assert_eq!(
            s.balance,
            Cash::from_units(10_000).sub(s.fees),
            "the fee leaves the balance"
        );
    }

    #[test]
    fn a_maker_rebate_is_carried_through_rather_than_clamped() {
        // Being paid to provide liquidity is the whole economics of
        // some strategies; a fee model that floors at zero cannot
        // express them.
        let fees = Fees {
            maker: Ratio::from_ppm(-200),
            taker: Ratio::from_ppm(500),
        };
        let fill = Fill {
            stamp: Stamp::synthetic(0),
            instrument: InstrumentId::new(1),
            order: OrderId::new(1),
            trade: oq_types::TradeId::new(1),
            side: Side::Buy,
            offset: oq_types::Offset::Open,
            price: PriceTicks(1_000_000),
            qty: QtyLots(10),
            liquidity: oq_types::Liquidity::Maker,
        };
        assert!(fees.charge(BTC, &fill).0 < 0, "a rebate must stay negative");
    }

    #[test]
    fn no_fee_schedule_means_no_fees() {
        let mut k = kernel(10_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));
        assert_eq!(k.summary().fees, Cash::ZERO);
    }

    #[test]
    fn a_fill_opens_a_position_at_the_fill_price() {
        let mut k = kernel(1_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 999_000, 10, 1));
        let outs = k.apply(&tick(2, 998_000)).to_vec();
        assert!(matches!(outs.as_slice(), [Output::Filled(_)]));
        assert_eq!(k.summary().qty, QtyLots(10));
        assert_eq!(k.summary().entry, PriceTicks(999_000));
    }

    #[test]
    fn adding_to_a_position_averages_the_entry() {
        let mut k = kernel(10_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));
        k.apply(&buy(2, 900_000, 10, 2));
        k.apply(&tick(3, 900_000));
        assert_eq!(k.summary().qty, QtyLots(20));
        assert_eq!(
            k.summary().entry,
            PriceTicks(950_000),
            "size-weighted average of 1_000_000 and 900_000"
        );
    }

    #[test]
    fn closing_realizes_profit_and_flattens() {
        let mut k = kernel(10_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));
        // Sell back higher.
        k.apply(&Event::Submit {
            id: OrderId::new(2),
            side: Side::Sell,
            price: Some(PriceTicks(1_010_000)),
            qty: QtyLots(10),
            stamp: Stamp::synthetic(2),
            offset: oq_types::Offset::Open,
        });
        k.apply(&tick(3, 1_020_000));
        let s = k.summary();
        assert_eq!(s.qty, QtyLots::ZERO);
        assert_eq!(s.entry, PriceTicks::ZERO);
        assert!(s.realized.0 > 0, "a profitable round trip realizes profit");
    }

    #[test]
    fn a_duplicate_order_id_is_rejected() {
        let mut k = kernel(1_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 900_000, 1, 1));
        let outs = k.apply(&buy(1, 900_000, 1, 1)).to_vec();
        assert!(matches!(
            outs.as_slice(),
            [Output::Rejected {
                reason: RejectReason::DuplicateId,
                ..
            }]
        ));
    }

    #[test]
    fn a_non_positive_quantity_is_rejected() {
        let mut k = kernel(1_000);
        let outs = k.apply(&buy(1, 900_000, 0, 1)).to_vec();
        assert!(matches!(
            outs.as_slice(),
            [Output::Rejected {
                reason: RejectReason::NonPositiveQty,
                ..
            }]
        ));
    }

    #[test]
    fn funding_moves_the_balance_and_is_tracked_separately() {
        let mut k = kernel(10_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));
        let before = k.summary().balance;
        k.apply(&Event::Funding {
            at: Nanos::from_secs(28_800),
            rate: Ratio::from_ppm(100),
            mark: PriceTicks(1_000_000),
        });
        let s = k.summary();
        assert!(s.balance < before, "a long pays a positive rate");
        assert_eq!(s.funding, s.balance.sub(before));
    }

    #[test]
    fn a_position_is_liquidated_when_the_market_reaches_the_level() {
        // The behaviour that a backtest without margin cannot produce.
        let mut k = kernel(100);
        k.apply(&tick(1, 1_200_000));
        k.apply(&buy(1, 1_200_000, 10, 1));
        k.apply(&tick(2, 1_200_000));
        assert_eq!(k.summary().qty, QtyLots(10));

        let liq = k
            .state()
            .position()
            .liquidation_price(&table())
            .expect("has one");
        let outs = k.apply(&tick(3, liq.0)).to_vec();
        assert!(
            outs.iter().any(|o| matches!(o, Output::Liquidated { .. })),
            "expected a liquidation at {liq:?}, got {outs:?}"
        );
        assert_eq!(k.summary().qty, QtyLots::ZERO, "the venue closed it");
    }

    #[test]
    fn funding_alone_can_cause_liquidation() {
        // A position that price never stopped out, ended by financing.
        // Note the entry: the resting buy at 1_200_000 filled at
        // 1_150_000 because a limit order gets price improvement, so
        // the position is flat to the mark and only funding moves it.
        let mut k = kernel(100);
        k.apply(&tick(1, 1_200_000));
        k.apply(&buy(1, 1_200_000, 10, 1));
        k.apply(&tick(2, 1_150_000));
        assert_eq!(k.summary().qty, QtyLots(10));
        assert_eq!(k.summary().entry, PriceTicks(1_150_000));
        assert!(
            !k.state()
                .position()
                .is_liquidatable(&table(), PriceTicks(1_150_000)),
            "price alone leaves this position safe; the point is that funding does not"
        );

        // 1_150 USDT of notional at 10% is more than the whole balance.
        let outs = k
            .apply(&Event::Funding {
                at: Nanos::from_secs(28_800),
                rate: Ratio::from_percent(10), // a squeeze
                mark: PriceTicks(1_150_000),
            })
            .to_vec();
        assert!(
            outs.iter().any(|o| matches!(o, Output::Liquidated { .. })),
            "funding should have ended it, got {outs:?}"
        );
        assert_eq!(k.summary().qty, QtyLots::ZERO);
    }

    #[test]
    fn a_liquidated_account_cannot_keep_trading_on_no_collateral() {
        let mut k = kernel(1);
        k.apply(&tick(1, 1_200_000));
        k.apply(&buy(1, 1_200_000, 100, 1));
        k.apply(&tick(2, 1_200_000));
        // Drive the balance to zero or below.
        k.apply(&Event::MarginDeposit {
            amount: -Cash::from_units(1_000).0,
            at: Nanos(3),
        });
        let outs = k.apply(&buy(2, 1_000_000, 1, 3)).to_vec();
        assert!(matches!(
            outs.as_slice(),
            [Output::Rejected {
                reason: RejectReason::NoMargin,
                ..
            }]
        ));
    }

    #[test]
    fn crossing_through_flat_starts_a_new_position_at_the_fill_price() {
        let mut k = kernel(100_000);
        k.apply(&tick(1, 1_000_000));
        k.apply(&buy(1, 1_000_000, 10, 1));
        k.apply(&tick(2, 1_000_000));
        assert_eq!(k.summary().qty, QtyLots(10));

        // Sell 30: closes 10 long, opens 20 short.
        k.apply(&Event::Submit {
            id: OrderId::new(2),
            side: Side::Sell,
            price: Some(PriceTicks(1_010_000)),
            qty: QtyLots(30),
            stamp: Stamp::synthetic(2),
            offset: oq_types::Offset::Open,
        });
        k.apply(&tick(3, 1_020_000));
        let s = k.summary();
        assert_eq!(s.qty, QtyLots(-20));
        assert!(s.realized.0 > 0, "the closed leg realized");
        assert!(
            s.entry.0 >= 1_010_000,
            "the new short's entry is the fill price, not a blend"
        );
    }
}
