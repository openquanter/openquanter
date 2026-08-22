//! Maintenance-margin brackets.
//!
//! Venues do not charge one maintenance rate. They charge a rate that
//! steps up with position notional, and they pair each step with a
//! *maintenance amount* — a constant that makes the requirement
//! continuous at the bracket boundary instead of jumping.
//!
//! The continuity is the reason the maintenance amount exists, and the
//! reason it cannot be dropped as an implementation detail. Without it,
//! a position one lot over a boundary would show a discontinuous jump
//! in its requirement, and a liquidation model built on that would
//! liquidate positions the venue would not.
//!
//! Requirement at price `p`:
//!
//! ```text
//! maintenance(p) = notional(p) * rate(bracket) - amount(bracket)
//! ```
//!
//! The bracket is chosen by notional, which depends on price, so a
//! falling market can move a long position into a *lower* bracket and a
//! rising one can move it into a higher one. That is modelled rather
//! than fixed at entry, because it is what the venue does.

use oq_types::{Cash, PriceTicks, QtyLots, Ratio};

/// The scaling that turns ticks and lots into money for one instrument.
///
/// One tick of price on one lot of quantity is worth `tick_cash` units
/// of [`Cash`]. Keeping this in one place means no other module has to
/// know an instrument's tick or lot size, and a wrong scale is a single
/// visible constant rather than a factor smeared across formulas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contract {
    /// Cash units per (1 price tick x 1 quantity lot).
    pub tick_cash: i64,
}

impl Contract {
    #[must_use]
    pub const fn new(tick_cash: i64) -> Self {
        Self { tick_cash }
    }

    /// Notional value of `qty` at `price`.
    ///
    /// Absolute: a short position has negative quantity but positive
    /// notional, and every margin rule is written against the absolute
    /// exposure.
    #[must_use]
    pub const fn notional(&self, price: PriceTicks, qty: QtyLots) -> Cash {
        let v = price.0 as i128 * qty.0.saturating_abs() as i128 * self.tick_cash as i128;
        Cash(v as i64)
    }

    /// Signed profit of a position marked from `entry` to `mark`.
    #[must_use]
    pub const fn unrealized(&self, entry: PriceTicks, mark: PriceTicks, qty: QtyLots) -> Cash {
        let delta = mark.0 as i128 - entry.0 as i128;
        Cash((delta * qty.0 as i128 * self.tick_cash as i128) as i64)
    }
}

/// One maintenance-margin bracket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginTier {
    /// Upper bound of the bracket, by notional. The last bracket uses
    /// [`Cash`] at its maximum to mean "no upper bound".
    pub max_notional: Cash,
    /// Maintenance margin rate for this bracket.
    pub rate: Ratio,
    /// The constant that keeps the requirement continuous across the
    /// bracket boundary below.
    pub amount: Cash,
}

/// A venue's maintenance-margin table for one instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierTable {
    tiers: Vec<MarginTier>,
}

impl TierTable {
    /// Build a table from brackets ordered by increasing notional.
    ///
    /// Returns `None` when the brackets are empty or out of order. An
    /// out-of-order table would silently select the wrong bracket, and
    /// a wrong bracket is a wrong liquidation price — the kind of error
    /// that is invisible until the one path where it matters.
    #[must_use]
    pub fn new(tiers: Vec<MarginTier>) -> Option<Self> {
        if tiers.is_empty() {
            return None;
        }
        if tiers
            .windows(2)
            .any(|w| w[0].max_notional >= w[1].max_notional)
        {
            return None;
        }
        Some(Self { tiers })
    }

    /// The bracket a position of this notional falls into.
    ///
    /// The last bracket catches everything above the table, which is
    /// what venues do rather than refusing the position.
    #[must_use]
    pub fn tier_for(&self, notional: Cash) -> MarginTier {
        for tier in &self.tiers {
            if notional <= tier.max_notional {
                return *tier;
            }
        }
        *self.tiers.last().expect("non-empty by construction")
    }

    /// Maintenance requirement for `qty` at `price`.
    #[must_use]
    pub fn maintenance(&self, contract: Contract, price: PriceTicks, qty: QtyLots) -> Cash {
        let notional = contract.notional(price, qty);
        let tier = self.tier_for(notional);
        let required = notional.scaled(tier.rate).sub(tier.amount);
        // A negative requirement is not a credit. It can only arise
        // from a malformed table, and letting it through would make a
        // position look over-collateralized by an arbitrary amount.
        if required.0 < 0 { Cash::ZERO } else { required }
    }

    #[must_use]
    pub fn tiers(&self) -> &[MarginTier] {
        &self.tiers
    }

    /// The Binance USDT-M BTCUSDT brackets, as a worked example and a
    /// default for tests.
    ///
    /// Values are the published schedule at the time of writing and are
    /// *not* authoritative: production runs load a dated table through
    /// [`crate::TierSchedule`]. A hard-coded table that drifts from the
    /// venue is exactly the failure this crate's bitemporal design
    /// exists to prevent, so this one is named for what it is.
    #[must_use]
    pub fn example_btcusdt() -> Self {
        let usdt = Cash::from_units;
        Self::new(vec![
            MarginTier {
                max_notional: usdt(50_000),
                rate: Ratio::from_ppm(4_000), // 0.40%
                amount: Cash::ZERO,
            },
            MarginTier {
                max_notional: usdt(500_000),
                rate: Ratio::from_ppm(5_000), // 0.50%
                amount: usdt(50),
            },
            MarginTier {
                max_notional: usdt(1_000_000),
                rate: Ratio::from_ppm(10_000), // 1.00%
                amount: usdt(2_550),
            },
            MarginTier {
                max_notional: usdt(10_000_000),
                rate: Ratio::from_ppm(25_000), // 2.50%
                amount: usdt(17_550),
            },
            MarginTier {
                max_notional: Cash(i64::MAX),
                rate: Ratio::from_ppm(50_000), // 5.00%
                amount: usdt(267_550),
            },
        ])
        .expect("brackets are ordered")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BTCUSDT: tick 0.1 USDT, lot 0.001 BTC, so one tick-lot is
    /// 0.0001 USDT = 10_000 cash units at the 1e-8 scale.
    const BTC: Contract = Contract::new(10_000);

    /// 120_000.0 USDT in ticks of 0.1.
    const PRICE: PriceTicks = PriceTicks(1_200_000);

    #[test]
    fn notional_matches_the_arithmetic_by_hand() {
        // 120_000 USDT x 0.002 BTC = 240 USDT
        assert_eq!(BTC.notional(PRICE, QtyLots(2)), Cash::from_units(240));
    }

    #[test]
    fn notional_is_absolute_for_shorts() {
        assert_eq!(
            BTC.notional(PRICE, QtyLots(-2)),
            BTC.notional(PRICE, QtyLots(2))
        );
    }

    #[test]
    fn unrealized_follows_the_sign_of_the_position() {
        let entry = PriceTicks(1_200_000);
        let up = PriceTicks(1_210_000); // +1_000 USDT
        assert_eq!(BTC.unrealized(entry, up, QtyLots(2)), Cash::from_units(2));
        assert_eq!(
            BTC.unrealized(entry, up, QtyLots(-2)),
            Cash::from_units(2).neg()
        );
    }

    #[test]
    fn out_of_order_tables_are_refused() {
        let bad = vec![
            MarginTier {
                max_notional: Cash::from_units(500_000),
                rate: Ratio::from_ppm(5_000),
                amount: Cash::ZERO,
            },
            MarginTier {
                max_notional: Cash::from_units(50_000),
                rate: Ratio::from_ppm(4_000),
                amount: Cash::ZERO,
            },
        ];
        assert!(TierTable::new(bad).is_none());
        assert!(TierTable::new(Vec::new()).is_none());
    }

    #[test]
    fn the_bracket_steps_up_with_notional() {
        let table = TierTable::example_btcusdt();
        assert_eq!(
            table.tier_for(Cash::from_units(10_000)).rate,
            Ratio::from_ppm(4_000)
        );
        assert_eq!(
            table.tier_for(Cash::from_units(100_000)).rate,
            Ratio::from_ppm(5_000)
        );
        assert_eq!(
            table.tier_for(Cash::from_units(50_000_000)).rate,
            Ratio::from_ppm(50_000),
            "above the table, the last bracket applies"
        );
    }

    #[test]
    fn the_requirement_is_continuous_across_a_boundary() {
        // The property the maintenance amount exists to provide. One
        // cash unit either side of a boundary must not jump.
        let table = TierTable::example_btcusdt();
        let boundary = Cash::from_units(50_000);

        let lower = table.tier_for(boundary);
        let upper = table.tier_for(Cash(boundary.0 + 1));
        assert_ne!(
            lower.rate, upper.rate,
            "the boundary must separate brackets"
        );

        let below = boundary.scaled(lower.rate).sub(lower.amount);
        let above = boundary.scaled(upper.rate).sub(upper.amount);
        assert_eq!(
            below, above,
            "maintenance must not jump at a bracket boundary"
        );
    }

    #[test]
    fn maintenance_never_goes_negative() {
        // A malformed table could produce a negative requirement; that
        // must read as zero, not as collateral the account does not
        // have.
        let table = TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_ppm(1),
            amount: Cash::from_units(1_000_000),
        }])
        .expect("single bracket");
        assert_eq!(table.maintenance(BTC, PRICE, QtyLots(1)), Cash::ZERO);
    }

    #[test]
    fn maintenance_grows_with_position_size() {
        let table = TierTable::example_btcusdt();
        let small = table.maintenance(BTC, PRICE, QtyLots(1));
        let large = table.maintenance(BTC, PRICE, QtyLots(100));
        assert!(large > small);
    }
}
