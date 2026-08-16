//! The last thing that runs before an order leaves.
//!
//! A backtest can be wrong and lose nothing. This layer exists because
//! the live path has no such property, and because the failures that
//! empty an account are rarely subtle: an order sized in the wrong
//! units, a price with a digit missing, a loop that resubmits, a
//! strategy that keeps trading through a condition nobody anticipated.
//! None of those are strategy bugs that better backtesting would have
//! caught. They are bugs in the machinery around the strategy, and the
//! only defence that works against a bug nobody predicted is a limit
//! that does not care why.
//!
//! # The permit
//!
//! Checking an order and then sending an order are two statements, and
//! nothing in the type system normally connects them — which is how a
//! check ends up validating one quantity while a different one is sent.
//! So a passing check does not return `true`; it returns a [`Permit`]
//! that **contains** the order it approved, and the order cannot be
//! read out of a permit and modified. A caller can decline to ask, but
//! it cannot ask about one order and send another.
//!
//! Making the asking itself mandatory needs a process host that hands
//! out nothing else, which is `oq-live`'s job and does not exist yet.
//! The permit is shaped for it now because retrofitting it after
//! external code holds the alternative is the expensive order.
//!
//! # Refusals say what, not why-not
//!
//! A refusal names the limit it hit and the numbers on both sides. An
//! operator reading it at three in the morning needs to know whether
//! the limit is wrong or the strategy is, and "risk check failed"
//! answers neither.
//!
//! # This crate has no clock and no I/O
//!
//! Every decision is a function of the arguments. Time is passed in,
//! not read, so the whole gate is testable and a replay of a live
//! session reaches the same decisions it reached live — which is what
//! makes an incident reconstructable rather than merely regrettable.

#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicBool, Ordering};

use oq_types::{Cash, Instrument, Nanos, PriceTicks, QtyLots, Ratio, Side};

/// An order a strategy wants to send, before anything has approved it.
///
/// Domain types rather than a venue's wire shape: this crate must not
/// depend on a gateway, both because the gate outlives any one venue
/// and because a risk layer that pulls in a TLS stack cannot be the
/// thing everything else is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposedOrder {
    pub side: Side,
    /// `None` for a market order.
    pub limit_price: Option<PriceTicks>,
    /// Always positive. The direction is [`ProposedOrder::side`].
    pub qty: QtyLots,
    /// Refuse to open or increase a position.
    pub reduce_only: bool,
}

/// What the account looks like right now, as far as the caller knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccountState {
    /// Signed position: positive long, negative short.
    pub position: QtyLots,
    /// The venue's mark price, for the price band and for notionals.
    pub mark: PriceTicks,
    /// Orders already resting.
    pub working: u32,
}

/// The limits an account trades under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest single order.
    pub max_order_qty: QtyLots,
    /// Largest absolute position the account may reach.
    pub max_position_qty: QtyLots,
    /// Largest notional a single order may carry.
    pub max_order_notional: Cash,
    /// How far a limit price may sit from the mark, in parts per
    /// billion. Guards the missing digit, which is the typo that gets
    /// filled instantly and at the worst possible price.
    pub price_band: Ratio,
    /// Most orders that may rest at once.
    pub max_working: u32,
    /// Most orders that may be sent within [`Limits::rate_window`].
    pub max_rate: u32,
    /// The window the rate is measured over.
    pub rate_window: Nanos,
}

impl Limits {
    /// Limits that refuse everything.
    ///
    /// The default is deliberately unusable rather than permissive. An
    /// account trading under limits nobody set is an account with no
    /// limits, and a gate that fails open is decoration.
    #[must_use]
    pub const fn closed() -> Self {
        Self {
            max_order_qty: QtyLots(0),
            max_position_qty: QtyLots(0),
            max_order_notional: Cash(0),
            price_band: Ratio(0),
            max_working: 0,
            max_rate: 0,
            rate_window: Nanos(1_000_000_000),
        }
    }
}

/// Why an order was refused.
///
/// Every variant carries the limit and the value that broke it, because
/// the first question after a refusal is always whether the limit is
/// wrong or the strategy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breach {
    /// The switch is down. Nothing passes until it is reset.
    Halted,
    OrderTooLarge {
        qty: QtyLots,
        limit: QtyLots,
    },
    PositionWouldExceed {
        resulting: QtyLots,
        limit: QtyLots,
    },
    NotionalTooLarge {
        notional: Cash,
        limit: Cash,
    },
    /// A limit price too far from the mark.
    PriceOutsideBand {
        price: PriceTicks,
        mark: PriceTicks,
        limit: Ratio,
    },
    TooManyWorking {
        working: u32,
        limit: u32,
    },
    TooFast {
        sent: u32,
        limit: u32,
    },
    /// A quantity of zero, or a price a venue cannot use.
    Malformed(&'static str),
    /// The instrument's definition does not yield a notional.
    Unpriceable,
}

/// An order that passed every check, carrying the order it approved.
///
/// The order cannot be taken out and changed. That is the point: a
/// check that returns a boolean approves a moment, and a permit
/// approves an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permit {
    order: ProposedOrder,
    at: Nanos,
}

impl Permit {
    /// The approved order, by value.
    #[must_use]
    pub const fn order(&self) -> ProposedOrder {
        self.order
    }

    /// When it was approved.
    #[must_use]
    pub const fn at(&self) -> Nanos {
        self.at
    }
}

/// Permitted, or refused with a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Permit(Permit),
    Refuse(Breach),
}

impl Decision {
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        matches!(self, Self::Permit(_))
    }
}

/// A switch that stops everything and stays stopped.
///
/// Tripping is one way on purpose. Something that can un-trip itself
/// after a cooldown will un-trip itself during the incident, which is
/// the moment it exists for. Clearing it is a decision a person makes
/// with a reason, and the reason is kept so the next reader knows what
/// was believed at the time.
#[derive(Debug, Default)]
pub struct KillSwitch {
    tripped: AtomicBool,
}

impl KillSwitch {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tripped: AtomicBool::new(false),
        }
    }

    /// Stop trading. Idempotent: tripping a tripped switch is not an
    /// error, because the second caller is usually a second detector of
    /// the same problem.
    pub fn trip(&self) {
        self.tripped.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    /// Resume trading.
    ///
    /// Named `clear` rather than `reset` because it is not a return to
    /// a previous state: whatever the switch stopped already happened.
    pub fn clear(&self) {
        self.tripped.store(false, Ordering::SeqCst);
    }
}

/// The gate itself.
#[derive(Debug)]
pub struct RiskGate {
    limits: Limits,
    kill: KillSwitch,
    /// Timestamps of recent sends, oldest first.
    recent: Vec<Nanos>,
}

impl RiskGate {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            kill: KillSwitch::new(),
            recent: Vec::new(),
        }
    }

    #[must_use]
    pub const fn kill_switch(&self) -> &KillSwitch {
        &self.kill
    }

    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Decide whether an order may be sent.
    ///
    /// `now` is passed rather than read so that the same inputs produce
    /// the same decision on a replay, which is what makes a live
    /// incident reconstructable.
    pub fn check(
        &mut self,
        order: &ProposedOrder,
        account: &AccountState,
        instrument: &Instrument,
        now: Nanos,
    ) -> Decision {
        if self.kill.is_tripped() {
            return Decision::Refuse(Breach::Halted);
        }
        if order.qty.0 <= 0 {
            return Decision::Refuse(Breach::Malformed(
                "quantity must be positive; direction is the side",
            ));
        }
        if let Some(p) = order.limit_price {
            if p.0 <= 0 {
                return Decision::Refuse(Breach::Malformed(
                    "a limit price of zero is an order to trade at nothing",
                ));
            }
        }
        if order.qty.0 > self.limits.max_order_qty.0 {
            return Decision::Refuse(Breach::OrderTooLarge {
                qty: order.qty,
                limit: self.limits.max_order_qty,
            });
        }

        // Where the position ends up if this fills entirely. A
        // reduce-only order cannot increase it, so it is checked
        // against the current position rather than the sum.
        let signed = match order.side {
            Side::Buy => order.qty.0,
            Side::Sell => -order.qty.0,
        };
        let resulting = if order.reduce_only {
            account.position.0
        } else {
            account.position.0.saturating_add(signed)
        };
        if resulting.abs() > self.limits.max_position_qty.0 {
            return Decision::Refuse(Breach::PositionWouldExceed {
                resulting: QtyLots(resulting),
                limit: self.limits.max_position_qty,
            });
        }

        // Notional at the price this would actually trade at: the limit
        // price when there is one, the mark when there is not.
        let reference = order.limit_price.unwrap_or(account.mark);
        let Some(notional) = instrument.notional(reference, order.qty) else {
            return Decision::Refuse(Breach::Unpriceable);
        };
        if notional.0 > self.limits.max_order_notional.0 {
            return Decision::Refuse(Breach::NotionalTooLarge {
                notional,
                limit: self.limits.max_order_notional,
            });
        }

        // The price band. Skipped when there is no mark to compare
        // against, because a band around an unknown centre refuses
        // everything or nothing depending on which way the zero falls,
        // and both are worse than saying so.
        if let (Some(price), true) = (order.limit_price, account.mark.0 > 0) {
            let distance = (price.0 - account.mark.0).abs();
            let allowed = i128::from(account.mark.0) * i128::from(self.limits.price_band.0)
                / i128::from(oq_types::RATIO_SCALE);
            if i128::from(distance) > allowed {
                return Decision::Refuse(Breach::PriceOutsideBand {
                    price,
                    mark: account.mark,
                    limit: self.limits.price_band,
                });
            }
        }

        if account.working >= self.limits.max_working {
            return Decision::Refuse(Breach::TooManyWorking {
                working: account.working,
                limit: self.limits.max_working,
            });
        }

        // Rate, over a sliding window. Counted at the moment of
        // permission rather than of sending, because an order that was
        // permitted and then failed still consumed the venue's patience.
        self.recent
            .retain(|t| now.0.saturating_sub(t.0) < self.limits.rate_window.0);
        let sent = u32::try_from(self.recent.len()).unwrap_or(u32::MAX);
        if sent >= self.limits.max_rate {
            return Decision::Refuse(Breach::TooFast {
                sent,
                limit: self.limits.max_rate,
            });
        }
        self.recent.push(now);

        Decision::Permit(Permit {
            order: *order,
            at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.01 USDT price steps, 0.001 BTC quantity steps, quantity is the
    /// asset. Cash per tick x lot is 1000, so 0.002 BTC at 120_000 USDT
    /// is 240 USDT.
    fn btc() -> Instrument {
        Instrument::linear(2, 3)
    }

    const MARK: PriceTicks = PriceTicks(12_000_000); // 120_000.00

    fn workable() -> Limits {
        Limits {
            max_order_qty: QtyLots(10),
            max_position_qty: QtyLots(20),
            max_order_notional: Cash(1_000 * oq_types::CASH_SCALE),
            price_band: Ratio(50_000_000), // 5%
            max_working: 5,
            max_rate: 3,
            rate_window: Nanos(1_000_000_000),
        }
    }

    fn buy(qty: i64) -> ProposedOrder {
        ProposedOrder {
            side: Side::Buy,
            limit_price: Some(MARK),
            qty: QtyLots(qty),
            reduce_only: false,
        }
    }

    fn flat() -> AccountState {
        AccountState {
            position: QtyLots(0),
            mark: MARK,
            working: 0,
        }
    }

    #[test]
    fn the_default_limits_refuse_everything() {
        // A gate that fails open is decoration. Limits nobody set must
        // not silently mean limits nobody has.
        let mut g = RiskGate::new(Limits::closed());
        let d = g.check(&buy(1), &flat(), &btc(), Nanos(0));
        assert!(!d.is_permitted(), "{d:?}");
    }

    #[test]
    fn an_ordinary_order_passes_and_the_permit_carries_it() {
        let mut g = RiskGate::new(workable());
        match g.check(&buy(2), &flat(), &btc(), Nanos(0)) {
            Decision::Permit(p) => {
                assert_eq!(
                    p.order(),
                    buy(2),
                    "the permit approves this order, not a moment"
                );
                assert_eq!(p.at(), Nanos(0));
            }
            Decision::Refuse(b) => panic!("refused: {b:?}"),
        }
    }

    #[test]
    fn a_tripped_switch_stops_everything_and_stays_stopped() {
        let mut g = RiskGate::new(workable());
        g.kill_switch().trip();
        assert_eq!(
            g.check(&buy(1), &flat(), &btc(), Nanos(0)),
            Decision::Refuse(Breach::Halted)
        );
        // Tripping twice is not an error: the second caller is usually
        // a second detector of the same problem.
        g.kill_switch().trip();
        assert!(g.kill_switch().is_tripped());
        g.kill_switch().clear();
        assert!(g.check(&buy(1), &flat(), &btc(), Nanos(0)).is_permitted());
    }

    #[test]
    fn a_missing_digit_is_caught_by_the_band_not_by_the_notional() {
        // 12_000.00 instead of 120_000.00 — an order that would fill
        // instantly at a price nobody meant. The notional is *smaller*,
        // so a notional cap alone would wave it through.
        let mut g = RiskGate::new(workable());
        let mut o = buy(2);
        o.limit_price = Some(PriceTicks(1_200_000));
        match g.check(&o, &flat(), &btc(), Nanos(0)) {
            Decision::Refuse(Breach::PriceOutsideBand { price, mark, .. }) => {
                assert_eq!(price, PriceTicks(1_200_000));
                assert_eq!(mark, MARK);
            }
            other => panic!("a missing digit must not pass: {other:?}"),
        }
    }

    #[test]
    fn a_position_that_would_exceed_the_cap_is_refused_before_it_exists() {
        let mut g = RiskGate::new(workable());
        let account = AccountState {
            position: QtyLots(19),
            ..flat()
        };
        match g.check(&buy(5), &account, &btc(), Nanos(0)) {
            Decision::Refuse(Breach::PositionWouldExceed { resulting, limit }) => {
                assert_eq!(resulting, QtyLots(24));
                assert_eq!(limit, QtyLots(20));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reduce_only_order_is_measured_against_the_position_it_reduces() {
        // Closing a position that is already at the cap must not be
        // refused for reaching the cap. A gate that will not let you
        // out is worse than no gate.
        let mut g = RiskGate::new(workable());
        let account = AccountState {
            position: QtyLots(20),
            ..flat()
        };
        let mut o = buy(5);
        o.side = Side::Sell;
        o.reduce_only = true;
        assert!(g.check(&o, &account, &btc(), Nanos(0)).is_permitted());
    }

    #[test]
    fn a_short_is_capped_by_the_same_number_as_a_long() {
        let mut g = RiskGate::new(workable());
        let account = AccountState {
            position: QtyLots(-19),
            ..flat()
        };
        let mut o = buy(5);
        o.side = Side::Sell;
        match g.check(&o, &account, &btc(), Nanos(0)) {
            Decision::Refuse(Breach::PositionWouldExceed { resulting, .. }) => {
                assert_eq!(resulting, QtyLots(-24), "the cap is on the magnitude");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn notional_is_capped_even_when_the_quantity_is_not() {
        let mut g = RiskGate::new(Limits {
            max_order_notional: Cash(100 * oq_types::CASH_SCALE),
            ..workable()
        });
        // 0.01 BTC at 120_000 is 1_200 USDT, over a 100 USDT cap.
        match g.check(&buy(10), &flat(), &btc(), Nanos(0)) {
            Decision::Refuse(Breach::NotionalTooLarge { notional, limit }) => {
                assert_eq!(notional, Cash(1_200 * oq_types::CASH_SCALE));
                assert_eq!(limit, Cash(100 * oq_types::CASH_SCALE));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_zero_quantity_is_malformed_rather_than_merely_small() {
        let mut g = RiskGate::new(workable());
        let mut o = buy(0);
        o.qty = QtyLots(0);
        assert!(matches!(
            g.check(&o, &flat(), &btc(), Nanos(0)),
            Decision::Refuse(Breach::Malformed(_))
        ));
    }

    #[test]
    fn a_negative_quantity_is_refused_rather_than_read_as_a_sell() {
        // The direction lives in the side. A negative quantity here
        // means two places disagree about which way the order points,
        // and guessing which one is right is how a hedge becomes a
        // doubled position.
        let mut g = RiskGate::new(workable());
        let mut o = buy(1);
        o.qty = QtyLots(-1);
        assert!(matches!(
            g.check(&o, &flat(), &btc(), Nanos(0)),
            Decision::Refuse(Breach::Malformed(_))
        ));
    }

    #[test]
    fn a_runaway_loop_is_stopped_by_the_rate_limit() {
        // The failure nobody predicts: something resubmits. The gate
        // does not need to know why.
        let mut g = RiskGate::new(workable());
        for i in 0..3 {
            assert!(
                g.check(&buy(1), &flat(), &btc(), Nanos(i)).is_permitted(),
                "order {i} within the rate"
            );
        }
        match g.check(&buy(1), &flat(), &btc(), Nanos(3)) {
            Decision::Refuse(Breach::TooFast { sent, limit }) => {
                assert_eq!((sent, limit), (3, 3));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_rate_window_slides_rather_than_resetting() {
        let mut g = RiskGate::new(workable());
        for i in 0..3 {
            assert!(g.check(&buy(1), &flat(), &btc(), Nanos(i)).is_permitted());
        }
        // A whole window later the earliest sends no longer count.
        assert!(
            g.check(&buy(1), &flat(), &btc(), Nanos(2_000_000_000))
                .is_permitted()
        );
    }

    #[test]
    fn resting_orders_are_capped_too() {
        let mut g = RiskGate::new(workable());
        let account = AccountState {
            working: 5,
            ..flat()
        };
        assert!(matches!(
            g.check(&buy(1), &account, &btc(), Nanos(0)),
            Decision::Refuse(Breach::TooManyWorking { .. })
        ));
    }

    #[test]
    fn a_market_order_is_priced_at_the_mark_for_the_notional_check() {
        // No limit price to measure, so the band does not apply, but
        // the notional still must.
        let mut g = RiskGate::new(Limits {
            max_order_notional: Cash(100 * oq_types::CASH_SCALE),
            ..workable()
        });
        let mut o = buy(10);
        o.limit_price = None;
        assert!(matches!(
            g.check(&o, &flat(), &btc(), Nanos(0)),
            Decision::Refuse(Breach::NotionalTooLarge { .. })
        ));
    }

    #[test]
    fn the_same_inputs_reach_the_same_decision_twice() {
        // No clock and no I/O, so a replay of a live session decides
        // what the live session decided.
        let mut a = RiskGate::new(workable());
        let mut b = RiskGate::new(workable());
        for i in 0..3 {
            assert_eq!(
                a.check(&buy(2), &flat(), &btc(), Nanos(i)),
                b.check(&buy(2), &flat(), &btc(), Nanos(i))
            );
        }
    }
}
