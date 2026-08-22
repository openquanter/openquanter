//! What a drift monitor has to get right before anyone should act on it.

use super::*;

/// Values drawn from a shape, deterministically, so a test that fails
/// fails for a reason rather than a seed.
fn ramp(n: usize, from: f64, to: f64) -> Vec<Option<f64>> {
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f64 / (n - 1) as f64;
            Some(from + t * (to - from))
        })
        .collect()
}

fn fit(values: &[Option<f64>]) -> Reference {
    Reference::fit("f", values, 10).expect("fittable")
}

/// The same distribution reports no drift.
///
/// The floor under everything else: a monitor that alarms on its own
/// reference is one that will be muted within a week.
#[test]
fn the_reference_does_not_drift_from_itself() {
    let values = ramp(1_000, 0.0, 1.0);
    let reference = fit(&values);
    let mut m = reference.watch();
    for v in &values {
        m.observe(*v);
    }
    let d = m.drift();
    assert!(d.psi < 1e-9, "{}", d.summary_line());
    assert_eq!(d.outside(), 0);
    assert!(!d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100));
}

/// A shifted distribution reports drift.
#[test]
fn a_shift_shows_up() {
    let reference = fit(&ramp(1_000, 0.0, 1.0));
    let mut m = reference.watch();
    // Same shape, moved half its width up.
    for v in ramp(1_000, 0.5, 1.5) {
        m.observe(v);
    }
    let d = m.drift();
    assert!(d.psi > CONVENTIONAL_PSI_THRESHOLD, "{}", d.summary_line());
    assert!(d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100));
}

/// **The failure frozen bins exist to prevent.** A distribution that has
/// moved bodily must not be able to re-bin itself into looking stable.
///
/// Asserted by fitting a second reference on the shifted data and
/// showing it reports calm against itself — which is exactly what a
/// monitor that re-derived its bins each window would report.
#[test]
fn re_deriving_bins_would_have_hidden_the_shift() {
    let shifted = ramp(1_000, 0.5, 1.5);

    let frozen = fit(&ramp(1_000, 0.0, 1.0));
    let mut against_frozen = frozen.watch();
    for v in &shifted {
        against_frozen.observe(*v);
    }

    let re_derived = fit(&shifted);
    let mut against_itself = re_derived.watch();
    for v in &shifted {
        against_itself.observe(*v);
    }

    let a = against_frozen.drift();
    let b = against_itself.drift();
    assert!(
        a.psi > 100.0 * b.psi.max(1e-12),
        "frozen {} vs re-derived {}",
        a.summary_line(),
        b.summary_line()
    );
}

/// Values beyond anything the reference saw are counted, not absorbed.
///
/// They land in an edge bin, where the index cannot tell them from
/// ordinary mass at the edge — and a feature emitting values nothing in
/// training resembled is the most alarming case there is.
#[test]
fn values_outside_the_reference_range_are_counted_separately() {
    let reference = fit(&ramp(1_000, 0.0, 1.0));
    let mut m = reference.watch();
    for v in ramp(990, 0.0, 1.0) {
        m.observe(v);
    }
    for _ in 0..10 {
        m.observe(Some(50.0));
    }
    let d = m.drift();
    assert_eq!(d.above, 10, "{}", d.summary_line());
    assert_eq!(d.below, 0);
    assert!(
        d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100),
        "ten impossible values must raise something: {}",
        d.summary_line()
    );
}

/// A feature that stops producing values moves no bin, so the index
/// cannot see it. The undefined share can.
#[test]
fn a_feature_that_stops_answering_is_caught_by_something_else() {
    let reference = fit(&ramp(1_000, 0.0, 1.0));
    let mut m = reference.watch();
    for v in ramp(500, 0.0, 1.0) {
        m.observe(v);
    }
    for _ in 0..500 {
        m.observe(None);
    }
    let d = m.drift();
    assert!(
        (d.undefined_share - 0.5).abs() < 1e-9,
        "{}",
        d.summary_line()
    );
    assert!(d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100));
}

/// A reference that was undefined some of the time does not alarm on the
/// same share appearing live.
#[test]
fn an_always_gappy_feature_does_not_alarm_on_its_usual_gaps() {
    let mut values = ramp(1_000, 0.0, 1.0);
    for i in (0..1_000).step_by(5) {
        values[i] = None;
    }
    let reference = fit(&values);
    assert!((reference.undefined_share() - 0.2).abs() < 1e-9);

    let mut m = reference.watch();
    for v in &values {
        m.observe(*v);
    }
    let d = m.drift();
    assert!(
        !d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100),
        "{}",
        d.summary_line()
    );
}

/// Too few observations must not produce an alarm, however extreme.
#[test]
fn a_handful_of_observations_is_not_evidence() {
    let reference = fit(&ramp(1_000, 0.0, 1.0));
    let mut m = reference.watch();
    m.observe(Some(1e9));
    let d = m.drift();
    assert!(d.outside() > 0, "it is recorded");
    assert!(
        !d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100),
        "but one observation is not evidence: {}",
        d.summary_line()
    );
}

/// A reference with nothing to bin is refused rather than degraded.
#[test]
fn a_reference_needs_something_to_describe() {
    let flat: Vec<Option<f64>> = vec![Some(1.0); 100];
    assert_eq!(
        Reference::fit("f", &flat, 10),
        Err(Error::NoSpread { value: 1.0 })
    );

    let thin = ramp(5, 0.0, 1.0);
    assert_eq!(
        Reference::fit("f", &thin, 10),
        Err(Error::TooFewValues {
            defined: 5,
            bins: 10
        })
    );

    assert_eq!(
        Reference::fit("f", &ramp(100, 0.0, 1.0), 1),
        Err(Error::TooFewBins { bins: 1 })
    );
}

/// Heavy ties collapse tied edges rather than producing bins nothing can
/// fall into.
#[test]
fn tied_values_reduce_the_bin_count_instead_of_making_empty_bins() {
    let mut values: Vec<Option<f64>> = vec![Some(0.0); 900];
    values.extend(ramp(100, 1.0, 2.0));
    let reference = Reference::fit("f", &values, 10).expect("fittable");
    assert!(
        reference.bins() < 10,
        "tied edges should collapse, got {} bins",
        reference.bins()
    );

    let mut m = reference.watch();
    for v in &values {
        m.observe(*v);
    }
    let d = m.drift();
    assert!(
        d.psi < 1e-9,
        "and the reference must still not drift from itself: {}",
        d.summary_line()
    );
}

/// A NaN is a defect, not a large value, and must not pass silently.
#[test]
fn a_nan_does_not_slip_through() {
    let reference = fit(&ramp(1_000, 0.0, 1.0));
    let mut m = reference.watch();
    for v in ramp(500, 0.0, 1.0) {
        m.observe(v);
    }
    for _ in 0..500 {
        m.observe(Some(f64::NAN));
    }
    let d = m.drift();
    assert!(
        d.alarming(CONVENTIONAL_PSI_THRESHOLD, 100),
        "500 NaNs must raise something: {}",
        d.summary_line()
    );
}
