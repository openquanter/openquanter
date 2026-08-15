//! Integration tests: the crate through its public API only.

use oq_parity::{
    BaselineStatus, Difference, Fill, IdentityElement, RunManifest, RunOutput, compare,
};
use oq_types::Side;

const DATA: &[u8] = b"tick data for the reference window";
const CONFIG: &[u8] = b"fidelity: L0\nfees: 0.0004\n";

fn manifest(commit: &str) -> RunManifest {
    RunManifest::from_content(commit, DATA, CONFIG, "L0")
}

fn reference_run() -> RunOutput {
    RunOutput::new(
        vec![
            Fill::new(1_000, "BTCUSDT", Side::Buy, 42_000, 10),
            Fill::new(2_000, "BTCUSDT", Side::Sell, 42_150, 10),
            Fill::new(3_000, "BTCUSDT", Side::Buy, 41_900, 20),
            Fill::new(4_000, "BTCUSDT", Side::Sell, 42_050, 20),
        ],
        4_500.0,
    )
}

#[test]
fn an_identical_run_passes() {
    let report = compare(
        &manifest("abc123"),
        &reference_run(),
        &manifest("abc123"),
        &reference_run(),
    );

    assert_eq!(report.baseline_status, BaselineStatus::Comparable);
    assert!(report.differences.is_empty());
    assert_eq!(report.first_divergence, None);
    assert_eq!(report.pnl_relative_error, Some(0.0));
    assert!(report.passes(1e-6));
}

#[test]
fn a_port_with_identical_behavior_passes_despite_a_new_commit() {
    // The whole point of a parity run during a rewrite: the code is
    // different on purpose, the behavior must not be.
    let report = compare(
        &manifest("old-impl"),
        &reference_run(),
        &manifest("new-impl"),
        &reference_run(),
    );

    assert_eq!(report.baseline_status, BaselineStatus::CodeChanged);
    assert!(report.passes(1e-6), "a faithful port must pass");
}

#[test]
fn a_one_tick_price_difference_is_located_and_quantified() {
    let mut candidate = reference_run();
    candidate.fills[2].price = oq_types::PriceTicks(41_901);
    candidate.pnl = 4_480.0;

    let report = compare(
        &manifest("abc123"),
        &reference_run(),
        &manifest("abc123"),
        &candidate,
    );

    assert_eq!(report.first_divergence, Some(2));
    assert_eq!(report.matched_prefix, 2, "the first two fills agreed");
    assert_eq!(report.differences.len(), 1);

    let Difference::Mismatch { fields, .. } = &report.differences[0] else {
        panic!(
            "expected a field-level mismatch, got {:?}",
            report.differences[0]
        );
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field, "price");
    assert_eq!(
        fields[0].delta,
        Some(1),
        "a one-tick difference, reported as one tick"
    );
    assert!(!report.passes(1e-6));
}

#[test]
fn an_inserted_fill_is_reported_as_extra_and_the_streams_resynchronize() {
    let mut candidate = reference_run();
    candidate
        .fills
        .insert(2, Fill::new(2_500, "BTCUSDT", Side::Buy, 41_950, 5));

    let report = compare(
        &manifest("abc123"),
        &reference_run(),
        &manifest("abc123"),
        &candidate,
    );

    assert_eq!(report.differences.len(), 1, "one insertion, not a cascade");
    match &report.differences[0] {
        Difference::Extra { index, fill } => {
            assert_eq!(*index, 2);
            assert_eq!(fill.qty.0, 5);
        }
        other => panic!("expected an extra fill, got {other:?}"),
    }
}

#[test]
fn a_dropped_fill_is_reported_as_missing() {
    let mut candidate = reference_run();
    candidate.fills.remove(1);

    let report = compare(
        &manifest("abc123"),
        &reference_run(),
        &manifest("abc123"),
        &candidate,
    );

    assert_eq!(report.differences.len(), 1);
    match &report.differences[0] {
        Difference::Missing { index, fill } => {
            assert_eq!(*index, 1);
            assert_eq!(fill.price.0, 42_150);
        }
        other => panic!("expected a missing fill, got {other:?}"),
    }
}

#[test]
fn corrected_input_data_invalidates_the_baseline_instead_of_reporting_a_regression() {
    // The failure mode this crate is designed around. The data was
    // repaired; the code was not touched. A tool that answers "mismatch"
    // here sends the reader bisecting code that is not at fault.
    let baseline_manifest =
        RunManifest::from_content("abc123", b"data with a corrupt week", CONFIG, "L0");
    let candidate_manifest =
        RunManifest::from_content("abc123", b"data, week repaired", CONFIG, "L0");

    let mut candidate = reference_run();
    candidate.fills[0].price = oq_types::PriceTicks(1); // wildly different
    candidate.pnl = -1_000.0;

    let report = compare(
        &baseline_manifest,
        &reference_run(),
        &candidate_manifest,
        &candidate,
    );

    assert_eq!(
        report.baseline_status,
        BaselineStatus::Invalidated {
            changed: vec![IdentityElement::DataHash]
        }
    );
    assert!(
        report.differences.is_empty(),
        "no differences may be reported against a stale baseline"
    );
    assert!(!report.passes(1e-6), "'cannot tell' is not 'they agree'");

    let rendered = report.to_string();
    assert!(rendered.contains("BASELINE INVALIDATED"));
    assert!(rendered.contains("rebase"));
    assert!(
        !rendered.contains("difference"),
        "the report must not offer a difference count that would be read as a regression"
    );
}

#[test]
fn pnl_tolerance_is_relative_and_enforced() {
    let mut candidate = reference_run();
    candidate.pnl = 4_500.0 * (1.0 + 5e-7);

    let report = compare(
        &manifest("abc123"),
        &reference_run(),
        &manifest("abc123"),
        &candidate,
    );

    assert!(report.differences.is_empty(), "fills are identical");
    assert!(report.passes(1e-6), "within tolerance");
    assert!(!report.passes(1e-8), "outside a tighter tolerance");
}

#[test]
fn the_report_leads_with_the_first_divergence() {
    // A single early divergence that cascades: the report must point at
    // the start, not drown the reader in consequences.
    let baseline = RunOutput::new(
        (0..50)
            .map(|i| Fill::new(i * 1_000, "BTCUSDT", Side::Buy, 42_000 + i, 1))
            .collect(),
        1_000.0,
    );
    let candidate = RunOutput::new(
        (0..50)
            .map(|i| {
                let shift = if i >= 10 { 7 } else { 0 };
                Fill::new(i * 1_000, "BTCUSDT", Side::Buy, 42_000 + i + shift, 1)
            })
            .collect(),
        1_010.0,
    );

    let report = compare(
        &manifest("abc123"),
        &baseline,
        &manifest("abc123"),
        &candidate,
    );

    assert_eq!(report.first_divergence, Some(10));
    assert_eq!(report.matched_prefix, 10);
    let rendered = report.to_string();
    assert!(rendered.contains("first divergence at fill 10"));
    assert!(rendered.contains("after 10 matching fills"));
}
