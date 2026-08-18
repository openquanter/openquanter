//! The margin invariants, over generated inputs.
//!
//! `FR-MARGIN-7` names three: margin usage is never negative, the
//! liquidation price moves monotonically with position size in the
//! expected direction, and equity is conserved across the application of
//! fees and funding.
//!
//! Until this file they were covered by hand-written cases. Those are
//! worth having and they check the examples somebody thought of, which
//! is exactly the set that excludes the input that breaks it. A
//! generated input does not know what the author expected.
//!
//! # These check properties, not outputs
//!
//! No number here is pinned. What is asserted is a relationship that has
//! to hold for every input — which is what makes a failure interesting:
//! it arrives with a counterexample the suite shrank down to its
//! smallest form, and that counterexample is the bug report.

use oq_margin::{Contract, LiquidationOutcome, MarginTier, MarginedPosition, TierTable};
use oq_types::{Cash, PriceTicks, QtyLots, Ratio};
use proptest::prelude::*;

/// Prices in a range wide enough to cross tiers and narrow enough that
/// the products stay in range.
fn price() -> impl Strategy<Value = PriceTicks> {
    (1i64..100_000_000).prop_map(PriceTicks)
}

/// Quantities on both sides of flat, because a short's maintenance is
/// the mirror of a long's and a property that only held for one would be
/// half a property.
fn qty() -> impl Strategy<Value = QtyLots> {
    (-1_000_000i64..1_000_000).prop_map(QtyLots)
}

/// A tier table that is well formed by construction: rates rising with
/// notional, which is what every venue publishes.
fn table() -> impl Strategy<Value = TierTable> {
    (1i64..1_000_000, 1u32..5_000, 1u32..2_000).prop_map(|(cap, base, step)| {
        let tiers: Vec<MarginTier> = (0..4)
            .map(|i| MarginTier {
                max_notional: Cash(cap * (i + 1) * oq_types::CASH_SCALE),
                rate: Ratio(i64::from(base + step * u32::try_from(i).unwrap_or(0)) * 1_000),
                amount: Cash(0),
            })
            .collect();
        TierTable::new(tiers).unwrap_or_else(TierTable::example_btcusdt)
    })
}

proptest! {
    /// Margin usage is never negative.
    ///
    /// A negative requirement would mean a position the venue pays you
    /// to hold, and every downstream comparison — headroom, liquidation
    /// distance, the risk gate's cap — is a subtraction against it.
    #[test]
    fn maintenance_is_never_negative(
        t in table(),
        p in price(),
        q in qty(),
        size in 1i64..1_000_000,
    ) {
        let contract = Contract::new(size);
        let m = t.maintenance(contract, p, q);
        prop_assert!(m.0 >= 0, "maintenance {} for qty {} at {}", m.0, q.0, p.0);
    }

    /// A flat position requires nothing.
    ///
    /// Separate from the above because "never negative" is satisfied by
    /// a constant, and a maintenance requirement that charged a flat
    /// account would be charging for a position nobody holds.
    #[test]
    fn a_flat_position_requires_no_margin(t in table(), p in price(), size in 1i64..1_000_000) {
        prop_assert_eq!(t.maintenance(Contract::new(size), p, QtyLots(0)), Cash(0));
    }

    /// Maintenance is symmetric in direction.
    ///
    /// A short of n costs what a long of n costs: the requirement is
    /// against exposure, and exposure has no sign. A model that charged
    /// them differently would make one side of every hedge cheaper for
    /// no reason a venue publishes.
    #[test]
    fn maintenance_is_the_same_for_both_directions(
        t in table(),
        p in price(),
        n in 1i64..1_000_000,
        size in 1i64..1_000_000,
    ) {
        let contract = Contract::new(size);
        prop_assert_eq!(
            t.maintenance(contract, p, QtyLots(n)),
            t.maintenance(contract, p, QtyLots(-n))
        );
    }

    /// Maintenance rises with size, never falls.
    ///
    /// This is FR-MARGIN-7's monotonicity, stated on the requirement
    /// rather than on the liquidation price — the two are the same fact
    /// and this one holds even where the liquidation price has no
    /// solution. A requirement that fell as a position grew would let a
    /// trader reduce their margin by buying more.
    #[test]
    fn maintenance_never_falls_as_the_position_grows(
        t in table(),
        p in price(),
        a in 1i64..500_000,
        extra in 1i64..500_000,
        size in 1i64..1_000_000,
    ) {
        let contract = Contract::new(size);
        let small = t.maintenance(contract, p, QtyLots(a));
        let large = t.maintenance(contract, p, QtyLots(a + extra));
        prop_assert!(
            large.0 >= small.0,
            "maintenance fell from {} to {} as the position grew from {} to {}",
            small.0,
            large.0,
            a,
            a + extra
        );
    }

    /// A long's liquidation price is below where it got in, and a
    /// short's is above — **for a position that was solvent when it was
    /// opened.**
    ///
    /// The direction FR-MARGIN-7 asks for. A long is liquidated when the
    /// price falls; one whose liquidation price sat above its entry
    /// would be liquidated on the way up, which is the sign error that
    /// looks like a profitable strategy until it is run.
    ///
    /// The qualifier is not a loophole and it was not in the first
    /// version of this test. A generated input found a position posted
    /// with less margin than its own maintenance requirement — already
    /// liquidatable at the instant it opened — and for that one the
    /// liquidation price is correctly *above* the entry, because the
    /// account is already on the wrong side of the line. The code was
    /// right and the property was too strong. Asserting it
    /// unconditionally would have meant weakening the engine to satisfy
    /// a test, which is the wrong direction to resolve a disagreement
    /// between the two.
    #[test]
    fn a_liquidation_price_sits_on_the_losing_side_of_the_entry(
        t in table(),
        entry in 1_000i64..100_000_000,
        n in 1i64..100_000,
        size in 1i64..1_000_000,
        margin in 1i64..1_000_000,
    ) {
        let contract = Contract::new(size);
        let m = Cash(margin * oq_types::CASH_SCALE);
        let long = MarginedPosition::new(contract, PriceTicks(entry), QtyLots(n), m);
        let short = MarginedPosition::new(contract, PriceTicks(entry), QtyLots(-n), m);

        let solvent = |p: &MarginedPosition| {
            !matches!(
                p.assess(&t, PriceTicks(entry)),
                LiquidationOutcome::Liquidatable { .. }
            )
        };

        if solvent(&long)
            && let Some(l) = long.liquidation_price(&t)
        {
            prop_assert!(
                l.0 <= entry,
                "a long entered at {entry} would be liquidated at {} — above its entry",
                l.0
            );
        }
        if solvent(&short)
            && let Some(s) = short.liquidation_price(&t)
        {
            prop_assert!(
                s.0 >= entry,
                "a short entered at {entry} would be liquidated at {} — below its entry",
                s.0
            );
        }
    }

    /// And the case the qualifier carved out is itself a property: a
    /// position posted with less margin than it requires is liquidatable
    /// the moment it exists. That is the honest reading of the
    /// counterexample above, and leaving it unasserted would have turned
    /// a finding into an exemption.
    #[test]
    fn a_position_that_cannot_pay_its_maintenance_is_liquidatable_at_once(
        t in table(),
        entry in 1_000i64..100_000_000,
        n in 1i64..100_000,
        size in 1i64..1_000_000,
    ) {
        let contract = Contract::new(size);
        let required = t.maintenance(contract, PriceTicks(entry), QtyLots(n));
        // A single unit of margin against a requirement larger than it.
        prop_assume!(required.0 > oq_types::CASH_SCALE);

        let p = MarginedPosition::new(
            contract,
            PriceTicks(entry),
            QtyLots(n),
            Cash(oq_types::CASH_SCALE),
        );
        prop_assert!(
            matches!(
                p.assess(&t, PriceTicks(entry)),
                LiquidationOutcome::Liquidatable { .. }
            ),
            "a position needing {} and holding one unit was not liquidatable",
            required.0
        );
    }

    /// More margin never brings liquidation closer.
    ///
    /// Adding collateral to a position must move its liquidation price
    /// away, or the account is punished for the one action that makes it
    /// safer.
    #[test]
    fn adding_margin_never_moves_liquidation_closer(
        t in table(),
        entry in 1_000i64..100_000_000,
        n in 1i64..100_000,
        size in 1i64..1_000_000,
        margin in 1i64..500_000,
        extra in 1i64..500_000,
    ) {
        let contract = Contract::new(size);
        let thin = MarginedPosition::new(
            contract,
            PriceTicks(entry),
            QtyLots(n),
            Cash(margin * oq_types::CASH_SCALE),
        );
        let thick = MarginedPosition::new(
            contract,
            PriceTicks(entry),
            QtyLots(n),
            Cash((margin + extra) * oq_types::CASH_SCALE),
        );
        if let (Some(a), Some(b)) = (thin.liquidation_price(&t), thick.liquidation_price(&t)) {
            prop_assert!(
                b.0 <= a.0,
                "a long with more margin would be liquidated at {} rather than {}",
                b.0,
                a.0
            );
        }
    }

    /// A tier table always answers, and always with a rate from itself.
    ///
    /// A notional past the last tier is the case a table is most likely
    /// to fall off: the venue's own table ends somewhere and a position
    /// larger than its last row still has a requirement.
    #[test]
    fn every_notional_lands_on_a_tier(t in table(), notional in 0i64..i64::MAX / 2) {
        let tier = t.tier_for(Cash(notional));
        prop_assert!(
            t.tiers().contains(&tier),
            "tier_for returned a tier that is not in the table"
        );
        prop_assert!(tier.rate.0 >= 0);
    }
}
