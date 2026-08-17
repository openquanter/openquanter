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
//!
//! # Precision is not the same as the grid
//!
//! A second distinction, and it was found the same way — by a venue
//! refusing an order. How many decimal places a price *may* have and
//! which prices are *allowed* are separate facts, and venues publish
//! them separately. One contract quotes two decimal places and moves in
//! steps of ten of them: 50339.10 is a price and 50339.04 is not,
//! though both have two decimals and both round-trip through every
//! parser here.
//!
//! Formatting to the precision therefore produces prices a venue
//! rejects, and the rejection says the price was not increased by the
//! tick size, which is a sentence that only makes sense once you know
//! the two numbers are different. [`Instrument::price_tick`] and
//! [`Instrument::qty_step`] carry the grid so the check can happen
//! before the order is sent.

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
    /// Smallest price increment, counted in the units
    /// [`Instrument::price_scale`] implies.
    ///
    /// `1` means every representable price is allowed. A contract
    /// quoting two decimals but moving in steps of 0.10 has `10` here,
    /// and a price that is not a multiple of it is refused by the venue
    /// however well it is formatted.
    pub price_tick: i64,
    /// Smallest quantity increment, in the units
    /// [`Instrument::qty_scale`] implies. `1` allows every
    /// representable quantity.
    pub qty_step: i64,
    /// Smallest notional the venue will accept for one order, in the
    /// units [`Cash`] counts in. Zero means no floor is known.
    ///
    /// A venue constraint rather than a risk preference, which is why it
    /// sits here beside the grid and not in the risk limits: it is a
    /// property of the contract, the same as how finely it can be
    /// priced. A strategy that has to learn it by being refused learns
    /// it once per strategy.
    pub min_notional: Cash,
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
            price_tick: 1,
            qty_step: 1,
            min_notional: Cash(0),
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
            price_tick: 1,
            qty_step: 1,
            min_notional: Cash(0),
        }
    }

    /// The same contract, restricted to a grid.
    ///
    /// Defaults to no restriction rather than to a guess, because a
    /// grid invented here would reject prices the venue accepts, and a
    /// tool that refuses valid work is harder to trust than one that
    /// forwards a refusal.
    #[must_use]
    pub const fn with_grid(mut self, price_tick: i64, qty_step: i64) -> Self {
        self.price_tick = if price_tick > 0 { price_tick } else { 1 };
        self.qty_step = if qty_step > 0 { qty_step } else { 1 };
        self
    }

    /// The same contract, with the venue's order floor.
    #[must_use]
    pub const fn with_min_notional(mut self, min_notional: Cash) -> Self {
        self.min_notional = min_notional;
        self
    }

    /// Whether a price sits on this contract's grid.
    #[must_use]
    pub const fn price_on_grid(&self, price: PriceTicks) -> bool {
        price.0 % self.price_tick == 0
    }

    /// Whether a quantity sits on this contract's grid.
    #[must_use]
    pub const fn qty_on_grid(&self, qty: QtyLots) -> bool {
        qty.0 % self.qty_step == 0
    }

    /// The nearest allowed price at or below `price`.
    ///
    /// Downward for a reason: moving a buy's limit down and a sell's up
    /// makes the order no more aggressive than asked for. Rounding to
    /// nearest would sometimes improve the price a strategy chose,
    /// which is a decision the strategy did not make.
    #[must_use]
    pub const fn snap_price_down(&self, price: PriceTicks) -> PriceTicks {
        PriceTicks(price.0 - price.0.rem_euclid(self.price_tick))
    }

    /// The nearest allowed price at or above `price`.
    #[must_use]
    pub const fn snap_price_up(&self, price: PriceTicks) -> PriceTicks {
        let r = price.0.rem_euclid(self.price_tick);
        if r == 0 {
            price
        } else {
            PriceTicks(price.0 + (self.price_tick - r))
        }
    }

    /// The nearest allowed quantity at or above `qty`.
    ///
    /// Upward, because the floor a quantity usually has to clear is a
    /// minimum, and rounding down lands under it.
    #[must_use]
    pub const fn snap_qty_up(&self, qty: QtyLots) -> QtyLots {
        let r = qty.0.rem_euclid(self.qty_step);
        if r == 0 {
            qty
        } else {
            QtyLots(qty.0 + (self.qty_step - r))
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

#[cfg(test)]
mod grid {
    use super::*;

    /// The contract that produced this code: two decimal places, and a
    /// tick of ten of them.
    fn btc_testnet() -> Instrument {
        Instrument::linear(2, 4).with_grid(10, 1)
    }

    #[test]
    fn a_price_with_valid_precision_can_still_be_off_the_grid() {
        // 50339.04 was refused by a real venue with "Price not
        // increased by tick size" while having exactly the two decimal
        // places the contract publishes. Both facts are true at once,
        // which is why both are stored.
        let i = btc_testnet();
        assert!(!i.price_on_grid(PriceTicks(5_033_904)));
        assert!(i.price_on_grid(PriceTicks(5_033_900)));
    }

    #[test]
    fn snapping_never_makes_an_order_more_aggressive() {
        // Down for a buy, up for a sell. Rounding to nearest would
        // sometimes improve the price the strategy chose, which is a
        // decision the strategy did not make.
        let i = btc_testnet();
        assert_eq!(
            i.snap_price_down(PriceTicks(5_033_904)),
            PriceTicks(5_033_900)
        );
        assert_eq!(
            i.snap_price_up(PriceTicks(5_033_904)),
            PriceTicks(5_033_910)
        );
    }

    #[test]
    fn snapping_a_price_already_on_the_grid_leaves_it_alone() {
        let i = btc_testnet();
        let exact = PriceTicks(5_033_910);
        assert_eq!(i.snap_price_down(exact), exact);
        assert_eq!(i.snap_price_up(exact), exact);
    }

    #[test]
    fn quantities_snap_upward_because_the_floor_is_a_minimum() {
        // Rounding a quantity down lands under the venue's minimum,
        // and the refusal talks about notional rather than about
        // rounding.
        let i = Instrument::linear(2, 3).with_grid(1, 10);
        assert_eq!(i.snap_qty_up(QtyLots(21)), QtyLots(30));
        assert_eq!(i.snap_qty_up(QtyLots(30)), QtyLots(30));
    }

    #[test]
    fn a_contract_with_no_stated_grid_allows_every_representable_value() {
        // The default must not invent a restriction: a grid guessed
        // here would refuse prices the venue accepts, and a tool that
        // refuses valid work is harder to trust than one that forwards
        // a refusal.
        let i = Instrument::linear(2, 3);
        assert!(i.price_on_grid(PriceTicks(5_033_904)));
        assert!(i.qty_on_grid(QtyLots(7)));
    }

    #[test]
    fn a_nonsense_grid_is_ignored_rather_than_dividing_by_zero() {
        let i = Instrument::linear(2, 3).with_grid(0, -5);
        assert_eq!((i.price_tick, i.qty_step), (1, 1));
    }

    #[test]
    fn a_negative_price_snaps_the_same_way_it_reads() {
        // Prices are not negative on the venues here, but rem_euclid
        // rather than % is what keeps the arithmetic from turning a
        // downward snap into an upward one if they ever are.
        let i = btc_testnet();
        assert_eq!(i.snap_price_down(PriceTicks(-14)), PriceTicks(-20));
        assert_eq!(i.snap_price_up(PriceTicks(-14)), PriceTicks(-10));
    }
}
