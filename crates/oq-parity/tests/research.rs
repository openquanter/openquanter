use std::cell::Cell;

use oq_engine::Tick;
use oq_features::{builtin::MidReturn, warmup::*};
use oq_parity::{BaselineStatus, research::*};
use oq_stats::trials::TrialRegistry;
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
fn config() -> WarmupConfig {
    WarmupConfig {
        evaluation_start: 8,
        evaluation_end: 16,
        warmup_ticks: vec![2, 4],
        tolerance: 1e-6,
    }
}
fn identity() -> StudyIdentity {
    StudyIdentity {
        code_commit: "a".repeat(40),
        question: "synthetic-midpoint-stability".into(),
        sampling_basis: "diagnostic-no-returns".into(),
        policy: b"mid_return period=2; rule=positive".to_vec(),
    }
}
fn path() -> impl WarmupPath {
    FeatureDecisions::new(MidReturn::new(2), |_: &Tick, value: Option<f64>| {
        value.map(|value| value > 0.0)
    })
}

#[test]
fn identity_binds_only_consumed_inputs_and_effective_configuration() {
    let input = ticks();
    let mut ledger = TrialRegistry::new();
    let first = warmup_study(&input, &config(), &identity(), &mut ledger, path).unwrap();
    assert_eq!(first.consumed_start, 4);
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger.unscored().len(), 2);
    let mut changed = input.clone();
    changed[0].bid = PriceTicks(1000);
    changed[19].bid = PriceTicks(1000);
    let same = warmup_study(&changed, &config(), &identity(), &mut ledger, path).unwrap();
    assert_eq!(first.manifest, same.manifest);
    assert_eq!(ledger.len(), 4, "repeated attempts still count");
    assert!(
        first.verify_registry(&ledger).is_err(),
        "later appends change the as-of ledger"
    );
    changed[5].volume = QtyLots(2);
    let different = warmup_study(&changed, &config(), &identity(), &mut ledger, path).unwrap();
    assert!(different.verify_baseline(&first.manifest).is_err());
    let mut code = identity();
    code.code_commit = "b".repeat(40);
    let changed_code = warmup_study(&input, &config(), &code, &mut ledger, path).unwrap();
    assert_eq!(
        changed_code.verify_baseline(&first.manifest).unwrap(),
        BaselineStatus::CodeChanged
    );
    let mut cfg = config();
    cfg.tolerance = 1e-5;
    let changed_config = warmup_study(&input, &cfg, &identity(), &mut ledger, path).unwrap();
    assert!(changed_config.verify_baseline(&first.manifest).is_err());
}

#[test]
fn attachment_round_trip_verifies_its_exact_ledger_and_rejects_corruption() {
    let mut ledger = TrialRegistry::new();
    let study = warmup_study(&ticks(), &config(), &identity(), &mut ledger, path).unwrap();
    let text = study.artifact_text();
    let attachment = parse_attachment(&text).unwrap();
    assert_eq!(attachment.manifest, study.manifest);
    assert_eq!(attachment.body, study.report.render());
    assert!(attachment.summary_line().contains("2 cumulative trials"));
    let reloaded = TrialRegistry::from_ledger(&ledger.to_ledger()).unwrap();
    attachment.verify_registry(&reloaded).unwrap();
    assert_eq!(
        attachment.verify_baseline(&study.manifest).unwrap(),
        BaselineStatus::Comparable
    );
    for bad in [
        text.replace("evidence 1", "evidence 2"),
        text.replace("trials=2", "trials=99999999"),
        text.replace("consumed_start=4", "consumed_start=04"),
        format!("{text}tampered\n"),
        text.replace("body_hash=", "unexpected="),
    ] {
        assert!(parse_attachment(&bad).is_err());
    }
    let mut unknown = study.manifest.clone();
    unknown.code_commit = "unknown".into();
    assert!(attachment.verify_baseline(&unknown).is_err());
    ledger.record_unscored("later attempt");
    assert!(attachment.verify_registry(&ledger).is_err());
}

#[test]
fn foreign_question_or_basis_does_not_erase_or_append_trials() {
    let mut ledger = TrialRegistry::new();
    warmup_study(&ticks(), &config(), &identity(), &mut ledger, path).unwrap();
    let prior = ledger.to_ledger();
    let mut foreign = identity();
    foreign.question = "another question".into();
    assert!(warmup_study(&ticks(), &config(), &foreign, &mut ledger, path).is_err());
    assert_eq!(ledger.to_ledger(), prior);
    foreign = identity();
    foreign.sampling_basis = "different sampling".into();
    assert!(warmup_study(&ticks(), &config(), &foreign, &mut ledger, path).is_err());
    assert_eq!(ledger.to_ledger(), prior);
}

struct Named(String);
impl WarmupPath for Named {
    type Decision = bool;
    fn name(&self) -> &str {
        &self.0
    }
    fn on_history(&mut self, _: &Tick) {}
    fn on_evaluation(&mut self, _: &Tick) -> (Option<f64>, Option<bool>) {
        (Some(0.0), Some(false))
    }
}
#[test]
fn failed_started_paths_remain_unscored_and_unattempted_paths_do_not_count() {
    let calls = Cell::new(0);
    let mut ledger = TrialRegistry::new();
    let mut cfg = config();
    cfg.warmup_ticks = vec![0, 2, 4];
    assert!(
        warmup_study(&ticks(), &cfg, &identity(), &mut ledger, || {
            let call = calls.get();
            calls.set(call + 1);
            Named(format!("path-{call}"))
        })
        .is_err()
    );
    assert_eq!(calls.get(), 2);
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger.unscored().len(), 2);
}
