//! What a contract is, in the one place every layer can see it.
//!
//! The definition existed twice and met nowhere. Capture knew a
//! contract's quoting precision and nothing about its economics; margin
//! knew its economics and nothing about how it is quoted; and between
//! them sat `InstrumentId`, a number that names a contract without
//! describing one. Two half-definitions are worse than one, because
//! each looks complete from where it is used.
//!
//! # The field that was missing from both
//!
//! Precision says how many decimal places a quantity has. It does not
//! say what the quantity counts, and venues disagree about that.
//! Binance's USD-M perpetuals quote an amount of the underlying: a
//! quantity of 1 on BTCUSDT is one bitcoin. OKX's swaps quote a number
//! of contracts, and one BTC-USDT-SWAP contract is 0.01 BTC. The same
//! symbol, the same asset, the same field name in the payload, and a
//! factor of a hundred between them.
//!
//! Nothing in a message says which convention is in force. Both venues
//! send a bare decimal string. A pipeline that reads them with only a
//! scale gets numbers that parse cleanly, sum cleanly, and mean
//! different things — the failure mode with no symptom, which is the
//! one worth spending a field on.
//!
//! So [`Instrument::contract_size`] states it: how much of the
//! underlying one quantity unit is. It is 1.0 wherever the quantity is
//! the underlying, which makes the common case explicit rather than
//! assumed.

use crate::fixed::{CASH_SCALE, Cash, PriceTicks, QtyLots};

/// Units per whole unit of the underlying, the scale
/// [`Instrument::contract_size`] counts in.
///
/// Eight decimal places, matching [`CASH_SCALE`], because the sizes that
/// occur in practice are round fractions — 0.01, 0.1, 1, 10 — and this
/// holds all of them exactly.
pub const CONTRACT_SCALE: i64 = 100_000_000;

/// A tradable contract: how it is quoted, and what a unit of it is
/// worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrument {
    /// Decimal places in a price.
    pub price_scale: u8,
    /// Decimal places in a quantity.
    pub qty_scale: u8,
    /// How much of the underlying one whole quantity unit is, in units
    /// of 1/[`CONTRACT_SCALE`].
    ///
    /// [`CONTRACT_SCALE`] itself means the quantity *is* the underlying.
    /// A tenth of that means one unit is 0.1 of it.
    pub contract_size: i64,
}

impl Instrument {
    /// A contract whose quantity is an amount of the underlying.
    ///
    /// The common case, and the one every venue this framework captured
    /// first happened to use — which is how the assumption survived
    /// long enough to be worth naming.
    #[must_use]
    pub const fn linear(price_scale: u8, qty_scale: u8) -> Self {
        Self {
            price_scale,
            qty_scale,
            contract_size: CONTRACT_SCALE,
        }
    }

    /// A contract whose quantity counts contracts, each worth
    /// `contract_size` of the underlying.
    #[must_use]
    pub const fn sized(price_scale: u8, qty_scale: u8, contract_size: i64) -> Self {
        Self {
            price_scale,
            qty_scale,
            contract_size,
        }
    }

    /// Cash per (1 price tick x 1 quantity lot).
    ///
    /// This is the number that turns quoted integers into money, and
    /// deriving it is the point of this type. Written by hand it is a
    /// magic constant that agrees with the venue until a listing
    /// changes, and disagrees silently afterwards: the price still
    /// parses, the quantity still parses, and only the notional is
    /// wrong.
    ///
    /// Returns `None` when the scales are large enough that a tick is
    /// worth less than the smallest cash unit, because a rounded-down
    /// zero here would price every position at nothing.
    #[must_use]
    pub fn tick_cash(&self) -> Option<i64> {
        let divisor = i128::from(CONTRACT_SCALE)
            .checked_mul(pow10(self.price_scale)?)?
            .checked_mul(pow10(self.qty_scale)?)?;
        let v = i128::from(CASH_SCALE)
            .checked_mul(i128::from(self.contract_size))?
            .checked_div(divisor)?;
        if v <= 0 {
            return None;
        }
        i64::try_from(v).ok()
    }

    /// Notional value of `qty` at `price`, or `None` when
    /// [`Instrument::tick_cash`] is not representable.
    ///
    /// Absolute: a short has negative quantity and positive exposure.
    #[must_use]
    pub fn notional(&self, price: PriceTicks, qty: QtyLots) -> Option<Cash> {
        let tick_cash = i128::from(self.tick_cash()?);
        let v = i128::from(price.0)
            .checked_mul(i128::from(qty.0.saturating_abs()))?
            .checked_mul(tick_cash)?;
        i64::try_from(v).ok().map(Cash)
    }
}

/// `10^n` as an `i128`, or `None` past the point it stops being useful.
fn pow10(n: u8) -> Option<i128> {
    if n > 18 {
        return None;
    }
    Some(10_i128.pow(u32::from(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_derived_tick_cash_matches_the_constant_that_was_hand_checked() {
        // oq-margin carried `Contract::new(10_000)` for BTC, arrived at
        // by hand from 0.1 USDT ticks and 0.001 BTC lots. Deriving it
        // has to produce the same number, or the derivation is the
        // thing that is wrong.
        assert_eq!(Instrument::linear(1, 3).tick_cash(), Some(10_000));
    }

    #[test]
    fn a_contract_that_is_not_the_underlying_prices_differently() {
        // OKX BTC-USDT-SWAP: 0.1 USDT ticks, 0.01 lots, 0.01 BTC per
        // contract. One tick times one lot is 0.1 x 0.0001 BTC-worth =
        // 1e-5 USDT, which is 1_000 cash units.
        let okx = Instrument::sized(1, 2, 1_000_000);
        assert_eq!(okx.tick_cash(), Some(1_000));

        // Same scales, read as though the quantity were the coin: a
        // hundred times too much. This is the arithmetic behind the
        // silent failure, written down so it stays visible.
        assert_eq!(Instrument::linear(1, 2).tick_cash(), Some(100_000));
    }

    #[test]
    fn notional_is_absolute_and_agrees_with_arithmetic_by_hand() {
        // 120_000 USDT x 0.002 BTC = 240 USDT.
        let btc = Instrument::linear(1, 3);
        let price = PriceTicks(1_200_000);
        assert_eq!(
            btc.notional(price, QtyLots(2)),
            Some(Cash(240 * CASH_SCALE))
        );
        assert_eq!(
            btc.notional(price, QtyLots(-2)),
            btc.notional(price, QtyLots(2))
        );
    }

    #[test]
    fn a_tick_worth_less_than_a_cash_unit_is_none_rather_than_zero() {
        // Rounding this to zero would price every position at nothing,
        // which reads as a flat account rather than as a bad definition.
        assert_eq!(Instrument::linear(9, 9).tick_cash(), None);
    }
}
