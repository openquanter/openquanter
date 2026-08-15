//! Funding: the cost of holding a perpetual position.
//!
//! Perpetual futures have no expiry, so the venue keeps them anchored
//! to spot by exchanging a payment between longs and shorts at fixed
//! settlement instants. For a strategy that holds through many
//! settlements, funding is not a rounding error — it is a recurring
//! cost or credit that compounds, and a backtest that omits it reports
//! a P&L the account would never have had.
//!
//! Two properties are modelled deliberately:
//!
//! - **Funding is charged on notional at the settlement mark**, not on
//!   the entry price. A position that has moved pays funding on what it
//!   is worth now.
//! - **The sign follows the side.** A positive rate means longs pay
//!   shorts. A model that always debits confuses a cost with a carry
//!   and misprices every strategy whose edge is being on the paid side.
//!
//! ## Spikes are the point
//!
//! Ordinary funding is a slow drain. What ends leveraged positions is
//! the tail: rates that jump by an order of magnitude for a few
//! settlements during a squeeze, at exactly the moment the position is
//! already under water. [`FundingSchedule::with_spike`] exists so that
//! this can be injected deliberately rather than waited for, because a
//! strategy's behaviour under a funding spike is a design question, not
//! a statistic to be discovered in production.

use oq_types::{Cash, Nanos, PriceTicks, QtyLots, Ratio};

use crate::tier::Contract;

/// A funding rate in effect at a settlement instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundingRate {
    /// When the venue settles.
    pub at: Nanos,
    /// Positive: longs pay shorts. Negative: shorts pay longs.
    pub rate: Ratio,
    /// The mark price the venue uses for the notional.
    ///
    /// Distinct from the last traded price: venues settle funding
    /// against a mark that is smoothed against index, and using the
    /// trade price instead misprices settlements during exactly the
    /// volatile moments that matter.
    pub mark: PriceTicks,
}

/// What one settlement cost or paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundingSettlement {
    pub at: Nanos,
    /// Signed cash flow to the position: negative is a payment.
    pub amount: Cash,
    pub rate: Ratio,
    pub mark: PriceTicks,
}

impl FundingRate {
    #[must_use]
    pub const fn new(at: Nanos, rate: Ratio, mark: PriceTicks) -> Self {
        Self { at, rate, mark }
    }

    /// The cash flow this settlement produces for `qty`.
    ///
    /// Sign convention, stated once so no call site has to re-derive
    /// it: `amount` is what happens *to the position's collateral*.
    /// A long with a positive rate pays, so the amount is negative.
    #[must_use]
    pub const fn settle(&self, contract: Contract, qty: QtyLots) -> FundingSettlement {
        let notional = contract.notional(self.mark, qty);
        let magnitude = notional.scaled(self.rate);
        // Long (qty > 0) pays a positive rate; short receives it.
        let amount = if qty.0 >= 0 {
            magnitude.neg()
        } else {
            magnitude
        };
        FundingSettlement {
            at: self.at,
            amount,
            rate: self.rate,
            mark: self.mark,
        }
    }
}

/// Funding rates over time, in settlement order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FundingSchedule {
    rates: Vec<FundingRate>,
}

impl FundingSchedule {
    /// Build a schedule, sorting by settlement time.
    ///
    /// Sorted rather than refused-if-unsorted: capture pipelines
    /// deliver rates in whatever order the venue's history endpoint
    /// returns them, and a constructor that rejects that would just be
    /// re-sorted at every call site.
    #[must_use]
    pub fn new(mut rates: Vec<FundingRate>) -> Self {
        rates.sort_by_key(|r| r.at.0);
        Self { rates }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rates.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rates.len()
    }

    #[must_use]
    pub fn rates(&self) -> &[FundingRate] {
        &self.rates
    }

    /// Settlements strictly after `from` and at or before `to`.
    ///
    /// Half-open on the left so that advancing tick by tick never
    /// settles the same instant twice — the double-charge that a
    /// closed-closed interval produces is invisible in a summary and
    /// obvious only in the tail.
    #[must_use]
    pub fn between(&self, from: Nanos, to: Nanos) -> &[FundingRate] {
        let start = self.rates.partition_point(|r| r.at <= from);
        let end = self.rates.partition_point(|r| r.at <= to);
        &self.rates[start..end]
    }

    /// The same schedule with a multiplier applied over a window.
    ///
    /// For asking what a strategy does when financing turns hostile:
    /// take a real history, multiply a stretch of it, and re-run. The
    /// alternative — waiting for a squeeze to appear in the sample — is
    /// how a strategy discovers its funding sensitivity in production.
    #[must_use]
    pub fn with_spike(&self, from: Nanos, to: Nanos, multiple: i64) -> Self {
        let rates = self
            .rates
            .iter()
            .map(|r| {
                if r.at >= from && r.at <= to {
                    FundingRate {
                        rate: Ratio(r.rate.0.saturating_mul(multiple)),
                        ..*r
                    }
                } else {
                    *r
                }
            })
            .collect();
        Self { rates }
    }

    /// Total cash flow for holding `qty` across `(from, to]`.
    #[must_use]
    pub fn accrue(&self, contract: Contract, qty: QtyLots, from: Nanos, to: Nanos) -> Cash {
        self.between(from, to)
            .iter()
            .fold(Cash::ZERO, |acc, r| acc.add(r.settle(contract, qty).amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: Contract = Contract::new(10_000);
    const MARK: PriceTicks = PriceTicks(1_200_000); // 120_000.0 USDT

    fn hourly(hours: i64, rate_ppm: i64) -> FundingRate {
        FundingRate::new(
            Nanos::from_secs(hours * 3_600),
            Ratio::from_ppm(rate_ppm),
            MARK,
        )
    }

    #[test]
    fn a_long_pays_a_positive_rate_and_a_short_receives_it() {
        // 0.01 BTC (10 lots) at 120_000 = 1_200 USDT notional,
        // rate 0.01% (100 ppm) -> 0.12 USDT.
        let r = FundingRate::new(Nanos::ZERO, Ratio::from_ppm(100), MARK);
        let long = r.settle(BTC, QtyLots(10));
        let short = r.settle(BTC, QtyLots(-10));
        assert_eq!(long.amount, Cash(12_000_000).neg(), "long pays 0.12 USDT");
        assert_eq!(short.amount, Cash(12_000_000), "short receives it");
        assert_eq!(long.amount, short.amount.neg());
    }

    #[test]
    fn a_negative_rate_reverses_who_pays() {
        let r = FundingRate::new(Nanos::ZERO, Ratio::from_ppm(-100), MARK);
        assert!(r.settle(BTC, QtyLots(10)).amount.0 > 0, "long receives");
        assert!(r.settle(BTC, QtyLots(-10)).amount.0 < 0, "short pays");
    }

    #[test]
    fn a_flat_position_settles_nothing() {
        let r = FundingRate::new(Nanos::ZERO, Ratio::from_ppm(100), MARK);
        assert_eq!(r.settle(BTC, QtyLots::ZERO).amount, Cash::ZERO);
    }

    #[test]
    fn the_window_is_half_open_so_nothing_settles_twice() {
        let s = FundingSchedule::new(vec![hourly(8, 100), hourly(16, 100), hourly(24, 100)]);
        let first = s.between(Nanos::ZERO, Nanos::from_secs(16 * 3_600));
        assert_eq!(first.len(), 2);
        // Advancing from where the last window ended must not re-settle
        // the boundary instant.
        let second = s.between(Nanos::from_secs(16 * 3_600), Nanos::from_secs(24 * 3_600));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].at, Nanos::from_secs(24 * 3_600));
    }

    #[test]
    fn unsorted_input_is_sorted_rather_than_refused() {
        let s = FundingSchedule::new(vec![hourly(24, 1), hourly(8, 2), hourly(16, 3)]);
        let order: Vec<i64> = s.rates().iter().map(|r| r.at.0).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted);
    }

    #[test]
    fn accrual_across_a_day_compounds_the_cost() {
        let s = FundingSchedule::new(vec![hourly(8, 100), hourly(16, 100), hourly(24, 100)]);
        let cost = s.accrue(BTC, QtyLots(10), Nanos::ZERO, Nanos::from_secs(24 * 3_600));
        // Three settlements of 0.12 USDT each, all paid by the long.
        assert_eq!(cost, Cash(36_000_000).neg());
    }

    #[test]
    fn a_spike_multiplies_only_inside_its_window() {
        let s = FundingSchedule::new(vec![hourly(8, 100), hourly(16, 100), hourly(24, 100)]);
        let spiked = s.with_spike(
            Nanos::from_secs(16 * 3_600),
            Nanos::from_secs(16 * 3_600),
            20,
        );
        let rates: Vec<i64> = spiked.rates().iter().map(|r| r.rate.0).collect();
        assert_eq!(
            rates,
            vec![
                Ratio::from_ppm(100).0,
                Ratio::from_ppm(2_000).0,
                Ratio::from_ppm(100).0
            ]
        );
    }

    #[test]
    fn a_spike_can_dominate_the_holding_cost() {
        // The tail behaviour worth being able to ask about: one hostile
        // settlement outweighing a day of ordinary ones.
        let s = FundingSchedule::new(vec![hourly(8, 100), hourly(16, 100), hourly(24, 100)]);
        let ordinary = s.accrue(BTC, QtyLots(10), Nanos::ZERO, Nanos::from_secs(24 * 3_600));
        let spiked = s
            .with_spike(
                Nanos::from_secs(16 * 3_600),
                Nanos::from_secs(16 * 3_600),
                50,
            )
            .accrue(BTC, QtyLots(10), Nanos::ZERO, Nanos::from_secs(24 * 3_600));
        assert!(spiked.0 < ordinary.0);
        assert!(
            spiked.0.abs() > ordinary.0.abs() * 10,
            "a 50x settlement should dominate the day"
        );
    }

    #[test]
    fn an_empty_schedule_accrues_nothing() {
        let s = FundingSchedule::default();
        assert!(s.is_empty());
        assert_eq!(
            s.accrue(BTC, QtyLots(10), Nanos::ZERO, Nanos::from_secs(86_400)),
            Cash::ZERO
        );
    }
}
