//! Closed-window evidence is checked by identity, not by aggregate totals.

use oq_core::PositionMode;
use oq_gateway::account::{ExecutionFee, FundingCharge};
use oq_live::{Record, ledger, ledger_coverage::*};
use oq_parity::{BaselineStatus, RunManifest};
use oq_types::{
    Cash, Fee, Fill, Instrument, InstrumentId, Liquidity, Nanos, Offset, OrderId, PriceTicks,
    QtyLots, Side, Stamp, TradeId,
};

fn fixture() -> (Vec<Record>, RunManifest, CoverageEvidence) {
    let instrument = Instrument::linear(0, 0);
    let records = vec![
        Record::SessionStart {
            prefix: "fixture".into(),
            symbol: "SYNTH".into(),
            price_scale: 0,
            qty_scale: 0,
        },
        Record::LedgerStart {
            at: Nanos(0),
            instrument_id: InstrumentId(1),
            instrument,
            tick_cash: oq_margin::Contract::of(&instrument).unwrap().tick_cash,
            balance: Cash::from_units(1000),
            mode: PositionMode::OneWay,
        },
        Record::Reconciled {
            at: Nanos(0),
            legs: vec![],
        },
        Record::Execution {
            client_id: "fixture-1".into(),
            fill: Fill {
                stamp: Stamp::new(10_000_000, 12_000_000),
                instrument: InstrumentId(1),
                order: OrderId(1),
                trade: TradeId(1),
                side: Side::Buy,
                offset: Offset::Open,
                price: PriceTicks(100),
                qty: QtyLots(1),
                liquidity: Liquidity::Maker,
            },
            fee: Fee::Reported(Cash(-7)),
        },
        Record::FundingEvidence {
            at: Nanos(15_000_000),
            settled_ms: 11,
            rate: "0.01".into(),
            mark: "100".into(),
            venue: Cash(-3),
            model: Cash(-3),
            verified: true,
            lines: vec![(9, 11, Cash(-3))],
        },
    ];
    let native = RunManifest::from_content(
        "a".repeat(40),
        b"fixture snapshot",
        b"fixture context",
        "openquanter native-ledger manifest v1",
    );
    let scope = "b".repeat(64);
    let window = |rows| HistoryWindow {
        account_scope_hash: scope.clone(),
        symbol: "SYNTH".into(),
        since_ms: 0,
        through_ms: 20,
        observed_ms: 25,
        completeness: Completeness::Complete,
        rows,
    };
    let evidence = CoverageEvidence {
        journal_data_hash: native.data_hash.clone(),
        account_scope_hash: scope.clone(),
        since_ms: 0,
        through_ms: 20,
        evaluated_ms: 30,
        max_age_ms: 10,
        fees: window(vec![ExecutionFee {
            trade_id: 1,
            side: Side::Buy,
            time_ms: 10,
            amount: Cash(-7),
        }]),
        funding: HistoryWindow {
            account_scope_hash: scope,
            symbol: "SYNTH".into(),
            since_ms: 0,
            through_ms: 20,
            observed_ms: 25,
            completeness: Completeness::Complete,
            rows: vec![FundingCharge {
                id: 9,
                time_ms: 11,
                amount: Cash(-3),
            }],
        },
    };
    (records, native, evidence)
}

#[test]
fn exact_identity_sets_accept_rebates_and_stable_overlapping_pages() {
    let (records, native, mut evidence) = fixture();
    let verified = verify(&records, &native, &evidence).unwrap();
    assert_eq!(
        verified.verify_baseline(&verified.manifest).unwrap(),
        BaselineStatus::Comparable
    );
    let mut unknown = verified.manifest.clone();
    unknown.code_commit = "unknown".into();
    assert!(verified.verify_baseline(&unknown).is_err());
    let mut foreign = verified.manifest.clone();
    foreign.label = native.label.clone();
    assert!(verified.verify_baseline(&foreign).is_err());
    assert_eq!((verified.fee_executions, verified.funding_lines), (1, 1));
    evidence.fees.rows.push(evidence.fees.rows[0]);
    evidence.funding.rows.push(evidence.funding.rows[0]);
    assert_eq!(verified, verify(&records, &native, &evidence).unwrap());
    let mut changed_code = native.clone();
    changed_code.code_commit = "c".repeat(40);
    assert_eq!(
        verified
            .manifest
            .compare(&verify(&records, &changed_code, &evidence).unwrap().manifest),
        BaselineStatus::CodeChanged
    );
    evidence.evaluated_ms += 1;
    assert!(matches!(
        verified
            .manifest
            .compare(&verify(&records, &native, &evidence).unwrap().manifest),
        BaselineStatus::Invalidated { .. }
    ));
}

#[test]
fn missing_surplus_and_same_sum_different_identity_are_not_coverage() {
    let (records, native, evidence) = fixture();
    let mut missing = evidence.clone();
    missing.fees.rows.clear();
    assert!(verify(&records, &native, &missing).is_err());
    let mut wrong = evidence.clone();
    wrong.funding.rows[0].id = 10;
    assert!(verify(&records, &native, &wrong).is_err());
    let mut surplus = evidence.clone();
    surplus.fees.rows.push(ExecutionFee {
        trade_id: 2,
        side: Side::Sell,
        time_ms: 10,
        amount: Cash(0),
    });
    assert!(verify(&records, &native, &surplus).is_err());
    let mut conflicting = evidence.clone();
    conflicting.fees.rows.push(ExecutionFee {
        amount: Cash(8),
        ..evidence.fees.rows[0]
    });
    assert!(verify(&records, &native, &conflicting).is_err());
    let mut changed_time = evidence.clone();
    changed_time.fees.rows[0].time_ms += 1;
    assert!(verify(&records, &native, &changed_time).is_err());
}

#[test]
fn incomplete_stale_foreign_future_and_truncated_sources_are_refused() {
    let (records, native, evidence) = fixture();
    let mutations: [fn(&mut CoverageEvidence); 10] = [
        |e| e.fees.completeness = Completeness::Incomplete("retention unknown".into()),
        |e| e.funding.completeness = Completeness::Incomplete("pagination stopped".into()),
        |e| e.fees.observed_ms = 19,
        |e| e.funding.observed_ms = 31,
        |e| e.max_age_ms = 4,
        |e| e.fees.account_scope_hash = "c".repeat(64),
        |e| e.funding.symbol = "OTHER".into(),
        |e| e.journal_data_hash = "d".repeat(64),
        |e| e.since_ms = 1,
        |e| e.funding.through_ms = 19,
    ];
    for mutate in mutations {
        let mut changed = evidence.clone();
        mutate(&mut changed);
        assert!(verify(&records, &native, &changed).is_err(), "{changed:?}");
    }
    let mut truncated = evidence;
    truncated.through_ms = 9;
    assert!(verify(&records, &native, &truncated).is_err());
}

#[test]
fn empty_history_requires_an_explicit_complete_window_and_native_flat_seed() {
    let (mut records, native, mut evidence) = fixture();
    records.truncate(3);
    evidence.fees.rows.clear();
    evidence.funding.rows.clear();
    assert_eq!(
        verify(&records, &native, &evidence).unwrap().fee_executions,
        0
    );
    evidence.fees.completeness = Completeness::Incomplete("unsupported".into());
    assert!(verify(&records, &native, &evidence).is_err());
    evidence.fees.completeness = Completeness::Complete;
    records.pop();
    assert!(verify(&records, &native, &evidence).is_err());
}

#[test]
fn unknown_and_legacy_economics_cannot_be_zero_filled() {
    let (mut records, native, evidence) = fixture();
    let Record::Execution { fee, .. } = &mut records[3] else {
        unreachable!()
    };
    *fee = Fee::Unreadable;
    assert!(verify(&records, &native, &evidence).is_err());
    let Record::Execution { fill, .. } = records[3].clone() else {
        unreachable!()
    };
    records.push(Record::FeeEvidence {
        at: Nanos(26_000_000),
        source_ms: 10,
        fill,
        amount: Cash(-7),
    });
    assert!(verify(&records, &native, &evidence).is_ok());
    let Record::FeeEvidence { source_ms, .. } = records.last_mut().unwrap() else {
        unreachable!()
    };
    *source_ms = 9;
    assert!(verify(&records, &native, &evidence).is_err());
    let (mut records, native, evidence) = fixture();
    records[4] = Record::Funding {
        at: Nanos(15_000_000),
        settled_ms: 11,
        rate: "0.01".into(),
        mark: "100".into(),
        venue: Cash(-3),
        model: Cash(-3),
        verified: true,
    };
    assert!(verify(&records, &native, &evidence).is_err());
}

#[test]
fn file_replay_binds_coverage_to_the_actual_immutable_snapshot() {
    let (records, _, mut evidence) = fixture();
    let path = std::env::temp_dir().join(format!("oq-coverage-{}.oqj", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut writer = oq_journal::Writer::open(&path, oq_journal::SyncPolicy::EveryRecord).unwrap();
    for record in records {
        writer.append(record.kind(), &record.encode()).unwrap();
    }
    drop(writer);
    let native = ledger::replay_bound(&path, &"a".repeat(40), None).unwrap();
    assert!(!native.accounting.funding_coverage);
    evidence.journal_data_hash = native.manifest.data_hash;
    let covered = ledger::replay_with_history(&path, &"a".repeat(40), &evidence).unwrap();
    assert!(covered.native.accounting.funding_coverage);
    assert_eq!(
        covered.verify_baseline(&covered.coverage.manifest).unwrap(),
        BaselineStatus::Comparable
    );
    assert!(covered.verify_baseline(&covered.native.manifest).is_err());
    assert_eq!(covered.native.accounting.recorded_fees, Cash(-7));
    evidence.journal_data_hash = "0".repeat(64);
    assert!(ledger::replay_with_history(&path, &"a".repeat(40), &evidence).is_err());
    std::fs::remove_file(path).unwrap();
}
