//! Synthetic evidence: canonical records, independent cash flow and CLI.

use oq_core::PositionMode;
use oq_journal::{SyncPolicy, Writer};
use oq_live::ledger::{replay, replay_records};
use oq_live::{Record, ledger};
use oq_types::{
    Cash, Fee, Fill, Instrument, InstrumentId, Liquidity, Nanos, Offset, OrderId, PriceTicks,
    QtyLots, Side, Stamp, TradeId,
};
use std::fs;

fn execution(id: u64, side: Side, offset: Offset, qty: i64, price: i64, fee: Fee) -> Record {
    Record::Execution {
        client_id: format!("fixture-{id}"),
        fill: Fill {
            stamp: Stamp::new(id as i64, id as i64 + 2),
            instrument: InstrumentId(1),
            order: OrderId(id),
            trade: TradeId(id),
            side,
            offset,
            price: PriceTicks(price),
            qty: QtyLots(qty),
            liquidity: Liquidity::Maker,
        },
        fee,
    }
}

fn records(mode: PositionMode) -> Vec<Record> {
    let instrument = Instrument::linear(0, 0);
    vec![
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
            mode,
        },
        Record::Reconciled {
            at: Nanos(0),
            legs: vec![],
        },
        execution(
            1,
            Side::Buy,
            Offset::Open,
            1,
            100,
            Fee::Reported(Cash::from_units(1)),
        ),
        execution(
            2,
            Side::Buy,
            Offset::Open,
            2,
            101,
            Fee::Reported(Cash::from_units(1)),
        ),
        Record::Funding {
            at: Nanos(4),
            settled_ms: 4,
            rate: "0.01".into(),
            mark: "100".into(),
            venue: Cash::from_units(-3),
            model: Cash::from_units(-3),
            verified: true,
        },
        execution(
            5,
            Side::Sell,
            Offset::Close,
            3,
            102,
            Fee::Reported(Cash::from_units(1)),
        ),
    ]
}

fn identified_funding(settled_ms: i64, lines: Vec<(i64, i64, Cash)>, venue: Cash) -> Record {
    Record::FundingEvidence {
        at: Nanos(settled_ms * 1_000_000),
        settled_ms,
        rate: "0.0001".into(),
        mark: "100".into(),
        venue,
        model: Cash::ZERO,
        verified: false,
        lines,
    }
}

#[test]
fn original_funding_identities_replay_without_claiming_history_coverage() {
    let mut all = records(PositionMode::OneWay);
    all.push(identified_funding(
        7000,
        vec![(1, 7000, Cash(-8)), (2, 7000, Cash(3))],
        Cash(-5),
    ));
    let report = replay_records(&all).unwrap();
    assert_eq!(
        report.funding,
        Cash::from_units(-3).add(Cash(-5)),
        "legacy -3 currency units plus identified -5 raw cash, each booked once"
    );
    assert_eq!(report.funding_identified_lines, 2);
    assert!(!report.funding_coverage);
    let path = write(&all);
    assert_eq!(replay(&path).unwrap().funding_identified_lines, 2);
    fs::remove_file(path).unwrap();
}

#[test]
fn duplicate_funding_ids_wrong_totals_and_wrong_windows_are_refused() {
    let base = records(PositionMode::OneWay);
    let mut duplicate = base.clone();
    duplicate.push(identified_funding(
        7000,
        vec![(1, 7000, Cash(-5))],
        Cash(-5),
    ));
    duplicate.push(identified_funding(
        8000,
        vec![(1, 7000, Cash(-5))],
        Cash(-5),
    ));
    assert!(
        replay_records(&duplicate)
            .unwrap_err()
            .contains("duplicate funding ledger identity")
    );
    let mut wrong_sum = base.clone();
    wrong_sum.push(identified_funding(
        7000,
        vec![(1, 7000, Cash(-4))],
        Cash(-5),
    ));
    assert!(replay_records(&wrong_sum).unwrap_err().contains("disagree"));
    let mut wrong_time = base;
    wrong_time.push(identified_funding(
        7000,
        vec![(1, 70001, Cash(-5))],
        Cash(-5),
    ));
    assert!(replay_records(&wrong_time).unwrap_err().contains("outside"));
}

#[test]
fn identified_funding_payload_rejects_torn_trailing_and_invalid_count() {
    let record = identified_funding(7000, vec![(1, 7000, Cash(-5))], Cash(-5));
    let encoded = record.encode();
    assert_eq!(
        Record::decode(record.kind(), &encoded),
        Some(record.clone())
    );
    for len in 0..encoded.len() {
        assert!(Record::decode(record.kind(), &encoded[..len]).is_none());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(Record::decode(record.kind(), &trailing).is_none());
    let mut negative_count = encoded;
    let count_at = negative_count.len() - 24 - 8;
    negative_count[count_at..count_at + 8].copy_from_slice(&(-1i64).to_le_bytes());
    assert!(Record::decode(record.kind(), &negative_count).is_none());
}

fn write(records: &[Record]) -> std::path::PathBuf {
    write_with_tail(records, None)
}

fn write_with_tail(records: &[Record], tail: Option<(u16, &[u8])>) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "oq-ledger-{}-{}.oqj",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut writer = Writer::open(&path, SyncPolicy::EveryRecordNoFsync).unwrap();
    for record in records {
        writer.append(record.kind(), &record.encode()).unwrap();
    }
    if let Some((kind, payload)) = tail {
        writer.append(kind, payload).unwrap();
    }
    writer.flush().unwrap();
    path
}

#[test]
fn typed_execution_round_trips_all_evidence_without_reusing_legacy_kind() {
    for (fee, liquidity) in [
        Fee::Reported(Cash::ZERO),
        Fee::Reported(Cash(-19)),
        Fee::Reported(Cash(23)),
        Fee::Unreadable,
        Fee::Unsaid,
    ]
    .into_iter()
    .flat_map(|fee| [Liquidity::Maker, Liquidity::Taker].map(|liquidity| (fee, liquidity)))
    {
        let mut r = execution(7, Side::Sell, Offset::Close, 3, 101, fee);
        if let Record::Execution { fill, .. } = &mut r {
            fill.liquidity = liquidity;
        }
        assert_eq!(Record::decode(r.kind(), &r.encode()), Some(r.clone()));
        assert_ne!(r.kind(), oq_live::record::kind::FILL);
        for n in 0..r.encode().len() {
            assert!(Record::decode(r.kind(), &r.encode()[..n]).is_none());
        }
        let mut tail = r.encode();
        tail.push(0);
        assert!(Record::decode(r.kind(), &tail).is_none());
    }
}

#[test]
fn ledger_context_round_trips_and_rejects_bad_scales_or_cash_multiplier() {
    let mut r = records(PositionMode::Hedge)[1].clone();
    assert_eq!(Record::decode(r.kind(), &r.encode()), Some(r.clone()));
    if let Record::LedgerStart { tick_cash, .. } = &mut r {
        *tick_cash += 1;
    }
    assert!(Record::decode(r.kind(), &r.encode()).is_none());
    let mut all = records(PositionMode::Hedge);
    if let Record::SessionStart { qty_scale, .. } = &mut all[0] {
        *qty_scale = 1;
    }
    assert!(ledger::replay_records(&all).is_err());
}

#[test]
fn native_replay_matches_independent_cash_flow_in_both_modes() {
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        let all = records(mode);
        let report = ledger::replay_records(&all).unwrap();
        // Sale 306 minus entry 302, minus three commissions and funding 3.
        assert_eq!(report.realized, Cash::from_units(4));
        assert_eq!(report.recorded_fees, Cash::from_units(3));
        assert_eq!(report.funding, Cash::from_units(-3));
        assert_eq!(report.recorded_balance, Cash::from_units(998));
        assert_eq!(report.executions, 3);
        assert_eq!(report.missing_fees, 0);
        assert!(
            !report.funding_coverage,
            "a funding amount is not proof of full coverage"
        );
        assert!(
            report
                .legs
                .iter()
                .all(|(_, qty, entry)| *qty == QtyLots::ZERO && *entry == PriceTicks::ZERO)
        );
        let path = write(&all);
        let disk = ledger::replay(&path).unwrap();
        assert_eq!(disk.recorded_balance, report.recorded_balance);
        assert_eq!(disk.legs, report.legs);
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn unknown_fees_are_not_promoted_to_known_zero_and_rebates_survive() {
    for fee in [Fee::Unsaid, Fee::Unreadable] {
        let mut all = records(PositionMode::Hedge);
        if let Record::Execution { fee: f, .. } = &mut all[3] {
            *f = fee;
        }
        let report = ledger::replay_records(&all).unwrap();
        assert_eq!(report.missing_fees, 1);
        assert_eq!(report.recorded_fees, Cash::from_units(2));
    }
    let mut all = records(PositionMode::Hedge);
    if let Record::Execution { fee, .. } = &mut all[3] {
        *fee = Fee::Reported(Cash::from_units(-1));
    }
    let report = ledger::replay_records(&all).unwrap();
    assert_eq!(report.recorded_fees, Cash::from_units(1));
    assert_eq!(report.recorded_balance, Cash::from_units(1000));
}

#[test]
fn missing_adoption_context_and_duplicate_economics_are_refused() {
    let mut all = records(PositionMode::Hedge);
    all.remove(2);
    assert!(
        ledger::replay_records(&all)
            .unwrap_err()
            .contains("initial positions")
    );
    let mut all = records(PositionMode::Hedge);
    all.remove(1);
    assert!(ledger::replay_records(&all).is_err());
    let mut all = records(PositionMode::Hedge);
    all.push(all[3].clone());
    assert!(
        ledger::replay_records(&all)
            .unwrap_err()
            .contains("duplicate")
    );
    let mut all = records(PositionMode::Hedge);
    all.push(all[5].clone());
    assert!(
        ledger::replay_records(&all)
            .unwrap_err()
            .contains("duplicate funding")
    );
}

#[test]
fn latest_session_resets_economics_but_unknown_records_are_not_skipped() {
    let mut all = records(PositionMode::OneWay);
    all.extend(records(PositionMode::Hedge));
    let report = ledger::replay_records(&all).unwrap();
    assert_eq!(report.mode, PositionMode::Hedge);
    assert_eq!(report.executions, 3);
    // Build the undecodable tail under one writer. This test exercises replay,
    // not immediate lock reacquisition while another test spawns a CLI child.
    let path = write_with_tail(&all, Some((65_000, b"unknown economic state")));
    assert!(ledger::replay(&path).unwrap_err().contains("undecodable"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn ledger_cli_reaches_the_native_path_and_reports_uncertainty() {
    let path = write(&records(PositionMode::Hedge));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oq-replay"))
        .arg(&path)
        .arg("--ledger")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("998.00000000"), "{text}");
    assert!(text.contains("unverified"), "{text}");
    let strict = std::process::Command::new(env!("CARGO_BIN_EXE_oq-replay"))
        .arg(&path)
        .args(["--ledger", "--require-coverage"])
        .output()
        .unwrap();
    assert_eq!(
        strict.status.code(),
        Some(2),
        "amounts alone cannot certify coverage"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn live_books_never_synthesize_a_venue_liquidation_or_insurance_transfer() {
    use oq_live::books::{Booked, Books};
    let mut books = Books::new(
        InstrumentId(1),
        oq_margin::Contract::new(1),
        oq_margin::TierTable::example_btcusdt(),
        Cash(10),
        PositionMode::Hedge,
    );
    books.adopt(Side::Buy, QtyLots(5), PriceTicks(100), Nanos(1));
    assert_eq!(
        books.legs().0,
        QtyLots(5),
        "adoption before the first mark is not a liquidation"
    );
    let tick = oq_engine::Tick {
        last: PriceTicks(1),
        high: PriceTicks(1),
        low: PriceTicks(1),
        ..oq_engine::Tick::default()
    };
    books.on_tick(&tick);
    assert_eq!(
        books.legs().0,
        QtyLots(5),
        "a risk projection cannot change confirmed inventory"
    );
    assert_eq!(books.realized_parts().0, Cash::ZERO);
    let Record::Execution { fill, fee, .. } = execution(
        7,
        Side::Sell,
        Offset::Close,
        5,
        1,
        Fee::Reported(Cash::ZERO),
    ) else {
        unreachable!()
    };
    assert!(matches!(
        books.on_venue_fill(&fill, fee),
        Booked::Applied(_)
    ));
    assert_eq!(
        books.legs().0,
        QtyLots::ZERO,
        "the venue's close is authoritative"
    );
    assert_eq!(books.realized_parts().0, Cash(-495));
}

#[test]
fn typed_belief_uses_explicit_mode_offset_and_non_unit_grids() {
    let instrument = Instrument {
        price_tick: 5,
        qty_step: 2,
        ..Instrument::linear(2, 3)
    };
    let all = vec![
        Record::SessionStart {
            prefix: "fixture".into(),
            symbol: "SYNTH".into(),
            price_scale: 2,
            qty_scale: 3,
        },
        Record::LedgerStart {
            at: Nanos(0),
            instrument_id: InstrumentId(1),
            instrument,
            tick_cash: oq_margin::Contract::of(&instrument).unwrap().tick_cash,
            balance: Cash::from_units(1000),
            mode: PositionMode::Hedge,
        },
        Record::Reconciled {
            at: Nanos(0),
            legs: vec![],
        },
        execution(
            1,
            Side::Buy,
            Offset::Open,
            2,
            100,
            Fee::Reported(Cash::ZERO),
        ),
        execution(
            2,
            Side::Sell,
            Offset::Open,
            1,
            101,
            Fee::Reported(Cash::ZERO),
        ),
    ];
    let path = write(&all);
    let belief = oq_live::belief::Belief::from_journal(&path).unwrap();
    assert!(
        belief.hedged,
        "an empty adoption does not erase the declared mode"
    );
    assert_eq!(
        belief.undecodable, 0,
        "typed fills need no guessed submission side"
    );
    assert_eq!(
        belief.legs,
        vec![("LONG".into(), 2, 100), ("SHORT".into(), -1, 101)]
    );
    let record = belief.to_record(0);
    assert_eq!(
        record.legs,
        vec![("LONG".into(), 0.004, 5.0), ("SHORT".into(), -0.002, 5.05)]
    );
    let ledger = ledger::replay(&path).unwrap();
    assert_eq!(ledger.legs[0].1, QtyLots(2));
    assert_eq!(ledger.legs[1].1, QtyLots(-1));
    std::fs::remove_file(path).unwrap();
}
