use oq_engine::Tick;
use oq_features::{builtin::MidReturn, warmup::*};
use oq_types::{PriceTicks, QtyLots, Stamp};

fn ticks() -> Vec<Tick> {
    (0..20)
        .map(|i| Tick {
            stamp: Stamp::new(i, i),
            last: PriceTicks(100),
            high: PriceTicks(100),
            low: PriceTicks(100),
            bid: PriceTicks(99),
            ask: PriceTicks(101),
            volume: QtyLots(1),
        })
        .collect()
}

fn config(warmup_ticks: Vec<usize>) -> WarmupConfig {
    WarmupConfig {
        evaluation_start: 8,
        evaluation_end: 16,
        warmup_ticks,
        tolerance: 1e-6,
    }
}

#[test]
fn cold_undefined_is_not_zero_or_a_defined_hold() {
    let factory = || {
        FeatureDecisions::new(MidReturn::new(2), |_: &Tick, value: Option<f64>| {
            value.map(|value| value > 0.0)
        })
    };
    let report = sensitivity(&ticks(), &config(vec![0, 4, 8]), factory).unwrap();
    assert!(!report.stable());
    let cold = &report.comparisons[0];
    assert!(cold.undefined_candidate > 0);
    assert!(cold.decisions.definedness_mismatches > 0);
    assert!(!cold.decisions.complete_and_stable());
    let warm = &report.comparisons[1];
    assert!(warm.features_converged(1e-6));
    assert!(warm.decisions.complete_and_stable());
    assert_eq!(report.reference_warmup, 8);
    assert!(report.render().contains("definedness mismatches"));
}

struct Threshold {
    history: usize,
}
impl WarmupPath for Threshold {
    type Decision = bool;
    fn name(&self) -> &str {
        "near_threshold"
    }
    fn on_history(&mut self, _: &Tick) {
        self.history += 1;
    }
    fn on_evaluation(&mut self, _: &Tick) -> (Option<f64>, Option<bool>) {
        let value = if self.history == 0 {
            1.0 - 1e-9
        } else {
            1.0 + 1e-9
        };
        (Some(value), Some(value > 1.0))
    }
}

#[test]
fn converged_indicators_can_still_change_every_decision() {
    let report = sensitivity(&ticks(), &config(vec![0, 1]), || Threshold { history: 0 }).unwrap();
    assert!(report.comparisons[0].features_converged(1e-6));
    assert_eq!(report.comparisons[0].decisions.changes, 8);
    assert_eq!(report.comparisons[0].decisions.first_divergence, Some(0));
    assert!(!report.stable());
}

#[test]
fn undefined_on_both_paths_is_not_numeric_convergence() {
    let mut input = ticks();
    for tick in &mut input[8..16] {
        tick.bid = PriceTicks(0);
    }
    let report = sensitivity(&input, &config(vec![4, 8]), || {
        FeatureDecisions::new(MidReturn::new(2), |_: &Tick, value: Option<f64>| {
            value.map(|_| false)
        })
    })
    .unwrap();
    assert_eq!(report.comparisons[0].numeric_pairs, 0);
    assert_eq!(report.comparisons[0].decisions.defined_pairs, 0);
    assert!(!report.stable());
    assert!(report.render().contains("NOT MEASURED"));
}

#[test]
fn unused_future_observations_do_not_change_a_common_window_report() {
    let input = ticks();
    let factory = || {
        FeatureDecisions::new(MidReturn::new(2), |_: &Tick, value: Option<f64>| {
            value.map(|value| value > 0.0)
        })
    };
    let original = sensitivity(&input, &config(vec![4, 8]), factory).unwrap();
    assert!(original.stable());
    let mut changed = input;
    for tick in &mut changed[16..] {
        tick.bid = PriceTicks(10_000);
        tick.ask = PriceTicks(20_000);
    }
    assert_eq!(
        original,
        sensitivity(&changed, &config(vec![4, 8]), factory).unwrap()
    );
}

#[test]
fn unavailable_history_empty_windows_duplicates_and_invalid_tolerances_refuse() {
    for cfg in [
        config(vec![]),
        config(vec![4]),
        config(vec![4, 4]),
        config(vec![4, 9]),
        WarmupConfig {
            evaluation_start: 8,
            evaluation_end: 8,
            ..config(vec![0, 4])
        },
        WarmupConfig {
            tolerance: f64::NAN,
            ..config(vec![0, 4])
        },
        WarmupConfig {
            tolerance: -1.0,
            ..config(vec![0, 4])
        },
    ] {
        assert!(sensitivity(&ticks(), &cfg, || Threshold { history: 0 }).is_err());
    }
}

struct Invalid;
impl WarmupPath for Invalid {
    type Decision = bool;
    fn name(&self) -> &str {
        "invalid"
    }
    fn on_history(&mut self, _: &Tick) {}
    fn on_evaluation(&mut self, _: &Tick) -> (Option<f64>, Option<bool>) {
        (Some(f64::INFINITY), Some(false))
    }
}

#[test]
fn equal_nonfinite_features_cannot_pass_even_with_identical_defined_decisions() {
    let report = sensitivity(&ticks(), &config(vec![4, 8]), || Invalid).unwrap();
    assert_eq!(report.comparisons[0].nonfinite_reference, 8);
    assert_eq!(report.comparisons[0].nonfinite_candidate, 8);
    assert!(report.comparisons[0].decisions.complete_and_stable());
    assert!(!report.stable());
}
