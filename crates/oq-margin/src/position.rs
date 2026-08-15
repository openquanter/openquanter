//! A position with margin, and the price at which the venue takes it.
//!
//! ## Deriving the liquidation price
//!
//! Liquidation happens when equity falls to the maintenance
//! requirement. Both sides of that equation depend on price, so the
//! liquidation price is the solution of an equation rather than a
//! formula to be memorized. Writing the derivation down here means the
//! implementation can be checked against reasoning instead of against a
//! copied constant.
//!
//! For a position of `q` lots entered at `e`, with margin `m`, at a
//! mark price `p`, under a bracket with rate `r` and amount `a`, where
//! `k` converts a tick-lot into cash:
//!
//! ```text
//! equity(p)      = m + (p - e) * q * k          (signed q: shorts gain as p falls)
//! maintenance(p) = p * |q| * k * r - a
//! ```
//!
//! Liquidation is the `p` where those are equal. For a long (`q > 0`):
//!
//! ```text
//! m + (p - e) * q * k = p * q * k * r - a
//! m - e*q*k + a       = p*q*k*r - p*q*k
//! p                   = (e*q*k - m - a) / (q*k*(1 - r))
//! ```
//!
//! For a short (`q < 0`, writing `s = |q|`):
//!
//! ```text
//! m + (e - p) * s * k = p * s * k * r - a
//! m + e*s*k + a       = p*s*k*(1 + r)
//! p                   = (e*s*k + m + a) / (s*k*(1 + r))
//! ```
//!
//! Two consequences worth stating, because they are what make the model
//! useful rather than decorative:
//!
//! - A long's liquidation price is **below** entry and a short's is
//!   **above** it, and both move *toward* entry as margin shrinks. A
//!   strategy that adds margin as it loses is buying room, and this
//!   model prices exactly how much.
//! - Because the bracket depends on notional, a position large enough
//!   to change brackets has a liquidation price that is not a smooth
//!   function of size. The bracket is therefore resolved at the mark,
//!   not fixed at entry.
//!
//! ## Rounding
//!
//! Prices exist only on ticks, so the reported liquidation price is the
//! nearest tick at which the position is *actually* liquidatable: floor
//! for a long (which is liquidated on the way down), ceiling for a short
//! (on the way up). The test suite pins this from both sides — the
//! reported tick must be liquidatable, and the tick one step safer must
//! not be — because a half-tick error here is invisible in aggregate and
//! decisive on the one path that matters.

use crate::tier::{Contract, TierTable};
use oq_types::{Cash, PriceTicks, QtyLots, RATIO_SCALE, Side};

/// A position carrying margin, marked against a tier table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginedPosition {
    pub contract: Contract,
    /// Average entry price.
    pub entry: PriceTicks,
    /// Signed size: positive long, negative short, zero flat.
    pub qty: QtyLots,
    /// Collateral allocated to this position.
    pub margin: Cash,
}

/// What the mark price implies for a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidationOutcome {
    /// The position is flat; nothing to liquidate.
    Flat,
    /// Equity is above the maintenance requirement.
    Safe {
        equity: Cash,
        maintenance: Cash,
        /// How far the mark can move before liquidation, in ticks.
        /// `None` when no positive price would trigger it.
        distance_ticks: Option<i64>,
    },
    /// Equity has fallen to or below the requirement.
    Liquidatable { equity: Cash, maintenance: Cash },
}

impl MarginedPosition {
    #[must_use]
    pub const fn new(contract: Contract, entry: PriceTicks, qty: QtyLots, margin: Cash) -> Self {
        Self {
            contract,
            entry,
            qty,
            margin,
        }
    }

    #[must_use]
    pub const fn side(&self) -> Option<Side> {
        self.qty.side()
    }

    #[must_use]
    pub const fn is_flat(&self) -> bool {
        self.qty.0 == 0
    }

    /// Unrealized profit at `mark`.
    #[must_use]
    pub const fn unrealized(&self, mark: PriceTicks) -> Cash {
        self.contract.unrealized(self.entry, mark, self.qty)
    }

    /// Margin plus unrealized profit.
    #[must_use]
    pub const fn equity(&self, mark: PriceTicks) -> Cash {
        self.margin.add(self.unrealized(mark))
    }

    /// Maintenance requirement at `mark`.
    #[must_use]
    pub fn maintenance(&self, table: &TierTable, mark: PriceTicks) -> Cash {
        table.maintenance(self.contract, mark, self.qty)
    }

    /// The price at which the venue liquidates this position.
    ///
    /// `None` for a flat position, or when the arithmetic has no
    /// positive solution — which happens when the maintenance rate is
    /// at or above one, a degenerate table rather than a real one.
    #[must_use]
    pub fn liquidation_price(&self, table: &TierTable) -> Option<PriceTicks> {
        if self.is_flat() {
            return None;
        }
        // The bracket is resolved at entry notional here; callers that
        // need the bracket re-resolved as the mark moves iterate with
        // `liquidation_price_at`, which is what the venue effectively
        // does.
        self.liquidation_price_at(table, self.entry)
    }

    /// The liquidation price implied by the bracket that applies at
    /// `mark`.
    ///
    /// Separated because bracket selection depends on notional, which
    /// depends on price: the liquidation price and the bracket that
    /// determines it are mutually dependent, and pretending otherwise
    /// silently uses the wrong bracket for large positions.
    #[must_use]
    pub fn liquidation_price_at(&self, table: &TierTable, mark: PriceTicks) -> Option<PriceTicks> {
        if self.is_flat() {
            return None;
        }
        let notional = self.contract.notional(mark, self.qty);
        let tier = table.tier_for(notional);

        let k = self.contract.tick_cash as i128;
        let size = self.qty.0.saturating_abs() as i128;
        let entry = self.entry.0 as i128;
        let margin = self.margin.0 as i128;
        let amount = tier.amount.0 as i128;
        let rate = tier.rate.0 as i128;
        let scale = RATIO_SCALE as i128;

        // Both branches are the derivation in the module docs, with the
        // ratio scale cleared by multiplying the numerator by `scale`.
        let (num, den) = if self.qty.0 > 0 {
            (
                (entry * size * k - margin - amount) * scale,
                size * k * (scale - rate),
            )
        } else {
            (
                (entry * size * k + margin + amount) * scale,
                size * k * (scale + rate),
            )
        };
        if den <= 0 {
            return None;
        }

        // Prices exist only on ticks, so the reported level must be a
        // tick at which the position is *actually* liquidatable — that
        // is the property downstream liquidation logic triggers on.
        //
        // A long is liquidated as price falls, so the first liquidatable
        // tick is the largest one at or below the exact level: floor.
        // A short is liquidated as price rises, so it is the smallest
        // tick at or above: ceiling. Rounding the other way reports a
        // price at which the venue would not yet have acted, which
        // would fire liquidations that never happened.
        let ticks = if self.qty.0 > 0 {
            num.div_euclid(den)
        } else {
            num.div_euclid(den) + i128::from(num.rem_euclid(den) != 0)
        };
        if ticks <= 0 {
            // A long so well collateralized that no positive price
            // liquidates it. Reported as "no liquidation price", not as
            // price zero, which would read as imminent liquidation.
            return if self.qty.0 > 0 {
                None
            } else {
                Some(PriceTicks(0))
            };
        }
        Some(PriceTicks(ticks as i64))
    }

    /// Assess the position at `mark`.
    #[must_use]
    pub fn assess(&self, table: &TierTable, mark: PriceTicks) -> LiquidationOutcome {
        if self.is_flat() {
            return LiquidationOutcome::Flat;
        }
        let equity = self.equity(mark);
        let maintenance = self.maintenance(table, mark);
        if equity <= maintenance {
            return LiquidationOutcome::Liquidatable {
                equity,
                maintenance,
            };
        }
        let distance_ticks = self
            .liquidation_price_at(table, mark)
            .map(|liq| (mark.0 - liq.0).abs());
        LiquidationOutcome::Safe {
            equity,
            maintenance,
            distance_ticks,
        }
    }

    /// Whether the venue would close this position at `mark`.
    #[must_use]
    pub fn is_liquidatable(&self, table: &TierTable, mark: PriceTicks) -> bool {
        matches!(
            self.assess(table, mark),
            LiquidationOutcome::Liquidatable { .. }
        )
    }

    /// Add or remove collateral.
    #[must_use]
    pub const fn with_margin(mut self, margin: Cash) -> Self {
        self.margin = margin;
        self
    }

    /// Apply a cash adjustment — funding, fees, realized profit — to the
    /// collateral backing this position.
    #[must_use]
    pub const fn adjust_margin(mut self, delta: Cash) -> Self {
        self.margin = self.margin.add(delta);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::{MarginTier, TierTable};
    use oq_types::Ratio;

    const BTC: Contract = Contract::new(10_000);
    const ENTRY: PriceTicks = PriceTicks(1_200_000); // 120_000.0 USDT

    /// A flat 1% maintenance rate with no amount, so hand arithmetic is
    /// possible in the tests that check the derivation.
    fn flat_table() -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    #[test]
    fn a_long_liquidates_below_entry_and_a_short_above() {
        let table = flat_table();
        let long = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(100));
        let short = MarginedPosition::new(BTC, ENTRY, QtyLots(-10), Cash::from_units(100));

        let long_liq = long.liquidation_price(&table).expect("long has one");
        let short_liq = short.liquidation_price(&table).expect("short has one");
        assert!(long_liq < ENTRY, "long liquidates below entry");
        assert!(short_liq > ENTRY, "short liquidates above entry");
    }

    #[test]
    fn the_liquidation_price_matches_the_derivation() {
        // 0.01 BTC (10 lots) at 120_000 = 1_200 USDT notional.
        // Margin 100 USDT, maintenance rate 1%, no amount.
        //   p = (e*s*k - m) / (s*k*(1 - r))
        //     = (1_200 - 100) / (0.01 * 0.99)  [in USDT per BTC terms]
        //     = 1_100 / 0.0099 = 111_111.11... USDT
        let table = flat_table();
        let long = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(100));
        let liq = long.liquidation_price(&table).expect("has one");
        // In ticks of 0.1 USDT the exact level is 1_111_111.1, and the
        // first tick a falling price actually triggers at is below it.
        assert_eq!(liq, PriceTicks(1_111_111));
    }

    #[test]
    fn equity_at_the_liquidation_price_has_reached_the_requirement() {
        // The defining property, checked rather than assumed.
        let table = flat_table();
        for qty in [QtyLots(10), QtyLots(-10), QtyLots(3), QtyLots(-7)] {
            let pos = MarginedPosition::new(BTC, ENTRY, qty, Cash::from_units(100));
            let liq = pos.liquidation_price(&table).expect("has one");
            assert!(
                pos.is_liquidatable(&table, liq),
                "qty {qty:?} must be liquidatable at its own liquidation price {liq:?}"
            );
        }
    }

    #[test]
    fn one_tick_the_safe_side_is_not_liquidatable() {
        // Together with the previous test this pins the boundary to a
        // single tick, which is where a rounding error would hide.
        let table = flat_table();
        let long = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(100));
        let liq = long.liquidation_price(&table).expect("has one");
        assert!(!long.is_liquidatable(&table, PriceTicks(liq.0 + 1)));

        let short = MarginedPosition::new(BTC, ENTRY, QtyLots(-10), Cash::from_units(100));
        let sliq = short.liquidation_price(&table).expect("has one");
        assert!(!short.is_liquidatable(&table, PriceTicks(sliq.0 - 1)));
    }

    #[test]
    fn more_margin_moves_liquidation_further_away() {
        let table = flat_table();
        let thin = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(50));
        let thick = thin.with_margin(Cash::from_units(200));
        let thin_liq = thin.liquidation_price(&table).expect("has one");
        let thick_liq = thick.liquidation_price(&table).expect("has one");
        assert!(
            thick_liq < thin_liq,
            "a better-collateralized long survives further down"
        );
    }

    #[test]
    fn liquidation_price_is_monotonic_in_margin() {
        // Property check over a range rather than a single pair: a
        // liquidation price that is not monotonic in collateral would
        // let a strategy improve its risk by removing margin.
        let table = flat_table();
        let mut previous = i64::MAX;
        for units in (10..500).step_by(7) {
            let pos = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(units));
            let liq = pos.liquidation_price(&table).expect("has one").0;
            assert!(
                liq <= previous,
                "adding margin must not raise a long's liquidation price"
            );
            previous = liq;
        }
    }

    #[test]
    fn a_flat_position_has_no_liquidation_price() {
        let table = flat_table();
        let flat = MarginedPosition::new(BTC, ENTRY, QtyLots::ZERO, Cash::from_units(100));
        assert!(flat.liquidation_price(&table).is_none());
        assert_eq!(flat.assess(&table, ENTRY), LiquidationOutcome::Flat);
    }

    #[test]
    fn an_overcollateralized_long_has_no_liquidation_price() {
        let table = flat_table();
        let fortress = MarginedPosition::new(BTC, ENTRY, QtyLots(1), Cash::from_units(1_000_000));
        assert!(
            fortress.liquidation_price(&table).is_none(),
            "no positive price liquidates it, which is not the same as price zero"
        );
    }

    #[test]
    fn the_bracket_is_resolved_at_the_mark_not_frozen_at_entry() {
        // A position whose notional crosses a bracket boundary must see
        // the bracket that actually applies.
        let table = TierTable::example_btcusdt();
        // 0.5 BTC (500 lots) at 120_000 = 60_000 USDT: second bracket.
        let pos = MarginedPosition::new(BTC, ENTRY, QtyLots(500), Cash::from_units(5_000));
        let at_entry = pos.maintenance(&table, ENTRY);
        // Same position marked much lower: notional falls into the
        // first bracket, and the requirement follows it.
        let lower = PriceTicks(600_000); // 60_000 USDT -> 30_000 notional
        let at_lower = pos.maintenance(&table, lower);
        assert!(at_lower < at_entry);
        assert_eq!(
            table.tier_for(BTC.notional(lower, QtyLots(500))).rate,
            Ratio::from_ppm(4_000)
        );
    }

    #[test]
    fn assess_reports_distance_in_ticks() {
        let table = flat_table();
        let pos = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(100));
        match pos.assess(&table, ENTRY) {
            LiquidationOutcome::Safe { distance_ticks, .. } => {
                let d = distance_ticks.expect("a long here has a liquidation price");
                assert_eq!(d, ENTRY.0 - 1_111_111);
            }
            other => panic!("expected Safe, got {other:?}"),
        }
    }

    #[test]
    fn funding_that_drains_margin_pulls_liquidation_closer() {
        // The mechanism behind the tail this crate exists to model: a
        // position that is never stopped out by price alone can still
        // be liquidated by financing costs.
        let table = flat_table();
        let start = MarginedPosition::new(BTC, ENTRY, QtyLots(10), Cash::from_units(100));
        let drained = start.adjust_margin(Cash::from_units(60).neg());
        assert!(
            drained.liquidation_price(&table).expect("has one")
                > start.liquidation_price(&table).expect("has one"),
            "paying funding moves a long's liquidation price up toward the mark"
        );
    }
}
