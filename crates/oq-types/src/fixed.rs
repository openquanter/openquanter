//! Fixed-point money.
//!
//! Every monetary quantity in the hot path is an integer. Prices count
//! ticks, quantities count lots, cash counts a fixed number of decimal
//! places. The conversion factors live with the instrument definition,
//! not with the number, so a price cannot be silently interpreted in
//! the wrong scale.
//!
//! The alternative — `f64` throughout — fails a requirement the project
//! depends on: two runs of the same inputs must produce byte-identical
//! output, across machines and optimization levels. Floating point
//! addition is not associative, and compilers reassociate. Integers do
//! not have that freedom, so the determinism is structural rather than
//! hoped-for.
//!
//! Overflow is checked in construction paths and saturating in
//! accumulation paths. An account balance that wraps is a catastrophe;
//! an account balance that saturates is a visibly wrong number that
//! surfaces in the first assertion that reads it.

/// A price, counted in instrument ticks.
///
/// The tick size lives in the instrument definition. `PriceTicks(4213)`
/// is 42.13 for an instrument whose tick is 0.01 and 4.213 for one whose
/// tick is 0.001; the number alone does not claim to know which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PriceTicks(pub i64);

/// A quantity, counted in instrument lots.
///
/// Signed: a position is a quantity, and positions can be short. Order
/// quantities are non-negative by convention, enforced at construction
/// in [`crate::order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QtyLots(pub i64);

/// Money, in units of 10^-8 of the quote currency.
///
/// Eight decimal places is the smallest scale that covers crypto
/// venues' quote precision without a second scale for "small" numbers.
/// At this scale an `i64` still spans roughly ±9.2e10 units of quote
/// currency, which is several orders of magnitude beyond any account
/// this framework is designed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Cash(pub i64);

/// A dimensionless ratio in parts per billion.
///
/// Fee rates, funding rates, and maintenance-margin rates are all
/// ratios. Parts per billion holds a 0.0001% maker rebate (1000 ppb)
/// and a 75% maintenance margin (750_000_000 ppb) in the same type
/// without a scale change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ratio(pub i64);

/// Parts per billion, the scale [`Ratio`] counts in.
pub const RATIO_SCALE: i64 = 1_000_000_000;

/// Units per whole unit of quote currency, the scale [`Cash`] counts in.
pub const CASH_SCALE: i64 = 100_000_000;

impl Cash {
    pub const ZERO: Self = Self(0);

    /// Cash from a whole number of quote-currency units.
    #[must_use]
    pub const fn from_units(units: i64) -> Self {
        Self(units.saturating_mul(CASH_SCALE))
    }

    /// Saturating addition.
    ///
    /// Saturating rather than wrapping: a balance pinned at the maximum
    /// is an obviously wrong number that fails the next invariant check,
    /// where a wrapped balance is a plausible-looking number that does
    /// not.
    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    #[must_use]
    pub const fn sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    #[must_use]
    pub const fn neg(self) -> Self {
        Self(self.0.saturating_neg())
    }

    /// This amount scaled by a ratio, truncated toward zero.
    ///
    /// Truncation toward zero rather than rounding: it is the rule every
    /// venue fee calculation in scope uses, and a rounding rule that
    /// differs from the venue's produces a slow drift that only shows up
    /// after thousands of fills.
    #[must_use]
    pub const fn scaled(self, ratio: Ratio) -> Self {
        Self((self.0 as i128 * ratio.0 as i128 / RATIO_SCALE as i128) as i64)
    }

    /// Whole quote-currency units, for display only.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / CASH_SCALE as f64
    }
}

impl Ratio {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(RATIO_SCALE);

    /// A ratio from parts per million, the unit venue documentation
    /// usually quotes fees in.
    #[must_use]
    pub const fn from_ppm(ppm: i64) -> Self {
        Self(ppm.saturating_mul(1_000))
    }

    /// A ratio from a percentage.
    #[must_use]
    pub const fn from_percent(percent: i64) -> Self {
        Self(percent.saturating_mul(RATIO_SCALE / 100))
    }

    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / RATIO_SCALE as f64
    }
}

impl QtyLots {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn abs(self) -> Self {
        Self(self.0.saturating_abs())
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    #[must_use]
    pub const fn sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    /// Signed side of a position, or `None` when flat.
    #[must_use]
    pub const fn side(self) -> Option<crate::Side> {
        if self.0 > 0 {
            Some(crate::Side::Buy)
        } else if self.0 < 0 {
            Some(crate::Side::Sell)
        } else {
            None
        }
    }
}

impl PriceTicks {
    pub const ZERO: Self = Self(0);

    /// Whether this price means "market order" in the order paths.
    ///
    /// Zero is the sentinel the matching engine uses for market orders,
    /// carried over from the reference implementation this engine has
    /// to stay compatible with. It is a named predicate rather than a
    /// bare `== 0` at each call site so that the convention is greppable
    /// and its one meaning is documented in one place.
    #[must_use]
    pub const fn is_market(self) -> bool {
        self.0 == 0
    }

    /// Notional value of `qty` at this price.
    ///
    /// Computed in `i128` and narrowed once: prices and quantities are
    /// each comfortably inside `i64`, but their product at realistic
    /// crypto prices and sizes is not.
    #[must_use]
    pub const fn notional(self, qty: QtyLots, lot_scale: i64, tick_scale: i64) -> Cash {
        let ticks = self.0 as i128;
        let lots = qty.0.saturating_abs() as i128;
        let scaled = ticks * lots * tick_scale as i128 * lot_scale as i128 / CASH_SCALE as i128;
        Cash(scaled as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_constructors_agree() {
        assert_eq!(Ratio::from_percent(50), Ratio(RATIO_SCALE / 2));
        assert_eq!(Ratio::from_ppm(1_000_000), Ratio::ONE);
    }

    #[test]
    fn cash_scaling_truncates_toward_zero() {
        // 1.00000000 * 0.15% = 0.0015; and the negative case truncates
        // toward zero rather than toward negative infinity.
        assert_eq!(
            Cash::from_units(1).scaled(Ratio::from_ppm(1_500)),
            Cash(150_000)
        );
        assert_eq!(Cash(-7).scaled(Ratio::from_percent(50)), Cash(-3));
    }

    #[test]
    fn cash_saturates_rather_than_wraps() {
        let huge = Cash(i64::MAX);
        assert_eq!(huge.add(Cash(1)), Cash(i64::MAX));
        assert_eq!(Cash(i64::MIN).sub(Cash(1)), Cash(i64::MIN));
    }

    #[test]
    fn qty_side_is_none_when_flat() {
        assert_eq!(QtyLots::ZERO.side(), None);
        assert_eq!(QtyLots(3).side(), Some(crate::Side::Buy));
        assert_eq!(QtyLots(-3).side(), Some(crate::Side::Sell));
    }

    #[test]
    fn zero_price_is_the_market_sentinel() {
        assert!(PriceTicks::ZERO.is_market());
        assert!(!PriceTicks(1).is_market());
    }

    #[test]
    fn notional_does_not_overflow_at_crypto_scale() {
        // 120_000.00 USDT, 100 BTC, tick 0.01 (1e6 at CASH_SCALE),
        // lot 0.001 -> the intermediate product exceeds i64.
        let price = PriceTicks(12_000_000);
        let qty = QtyLots(100_000);
        let value = price.notional(qty, 100_000, 1_000_000);
        assert!(value.0 > 0, "notional must stay positive, got {value:?}");
    }
}
