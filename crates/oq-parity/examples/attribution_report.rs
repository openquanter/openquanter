//! The report `WHY.md` compresses the project into, produced.
//!
//! ```text
//! cargo run -p oq-parity --example attribution_report
//! ```
//!
//! Two runs: one where every cause was measured, and one where funding
//! was not. The second exists because the difference between them is
//! the point — `FR-ATTRIB-6` says a failure to attribute must be
//! reported as a failure, and the easiest way to see that it is, is to
//! see the two side by side.
//!
//! The figures are illustrative. What is not illustrative is the
//! arithmetic: the gap comes from two independent P&L numbers, the
//! components are computed from evidence, and the residual is what is
//! left.

use oq_parity::attribution::{Evidence, Matched, Unmatched, attribute};
use oq_parity::manifest::RunManifest;
use oq_types::{Cash, Instrument, PriceTicks, QtyLots, Side};

/// A contract where one tick on one lot is one cent, so the printed
/// currency figures are readable.
fn instrument() -> Instrument {
    Instrument {
        price_scale: 0,
        qty_scale: 0,
        contract_size: 1_000_000,
        price_tick: 1,
        qty_step: 1,
        min_notional: Cash(0),
    }
}

/// The observations, chosen so the arithmetic below is checkable by
/// hand: at these scales one tick on one lot is one cent, so a hundred
/// tick-lots is one currency unit.
fn evidence(with_funding: bool) -> Evidence {
    Evidence {
        matched: vec![Matched {
            side: Side::Buy,
            qty: QtyLots(100),
            // Decided when the market was 61 ticks below where it was
            // when the venue matched: 61 * 100 tick-lots = 61 units of
            // latency.
            model_price: PriceTicks(59_939),
            // Filled 148 ticks above the prevailing price: 148 units of
            // slippage.
            venue_price: PriceTicks(60_148),
            reference_price: Some(PriceTicks(60_000)),
        }],
        unmatched: vec![
            // A resting buy the model filled 112 ticks inside the
            // market and the real queue never reached: edge the model
            // claimed and the account never had.
            Unmatched {
                side: Side::Buy,
                qty: QtyLots(100),
                price: PriceTicks(59_888),
                reference_price: Some(PriceTicks(60_000)),
                at_venue: false,
            },
        ],
        // The venue charged 96 more than the model expected.
        funding: with_funding.then_some((Cash(-9_600_000_000), Cash(0))),
        // And 22 more in fees.
        fees: Some((Cash(-2_200_000_000), Cash(0))),
    }
}

fn main() {
    let manifest = RunManifest::from_content(
        "9f2c1ab4d7e0",
        b"one session of captured ticks",
        b"the configuration in force",
        "session-2026-08-18",
    );

    println!("A session where every cause was measured");
    println!("========================================");
    let complete = attribute(
        manifest.clone(),
        &instrument(),
        Cash(1_194_000_000_000),
        Cash(1_240_000_000_000),
        &evidence(true),
    );
    print!("{}", complete.render());

    println!();
    println!("The same session with no funding statement");
    println!("==========================================");
    let incomplete = attribute(
        manifest,
        &instrument(),
        Cash(1_194_000_000_000),
        Cash(1_240_000_000_000),
        &evidence(false),
    );
    print!("{}", incomplete.render());

    println!();
    println!("The second is the requirement, not a degraded version of the first.");
    println!("A gap of -460 with funding silently treated as zero would report a");
    println!("residual that named a measurement nobody took as something nobody");
    println!("can explain. Those are different claims, and only one is true.");
}
