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

#[test]
fn original_unknown_fee_is_enriched_without_a_second_fill_or_inventory_change() {
    let mut all = records(PositionMode::OneWay);
    let original = replay_records(&all).unwrap();
    let (fill, old_fee) = all
        .iter_mut()
        .find_map(|record| {
            if let Record::Execution { fill, fee, .. } = record {
                let Fee::Reported(old) = *fee else {
                    return None;
                };
                *fee = Fee::Unreadable;
                Some((*fill, old))
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(replay_records(&all).unwrap().missing_fees, 1);
    let evidence = Record::FeeEvidence {
        at: Nanos(fill.stamp.local.0 + 10),
        source_ms: fill.stamp.exch.0 / 1_000_000,
        fill,
        amount: Cash(-7),
    };
    assert_eq!(
        Record::decode(evidence.kind(), &evidence.encode()),
        Some(evidence.clone())
    );
    all.push(evidence.clone());
    let recovered = replay_records(&all).unwrap();
    assert_eq!(recovered.missing_fees, 0);
    assert_eq!(recovered.recovered_fee_executions, 1);
    assert_eq!(recovered.executions, original.executions);
    assert_eq!(recovered.legs, original.legs);
    assert_eq!(recovered.realized, original.realized);
    assert_eq!(
        recovered.recorded_fees,
        original.recorded_fees.sub(old_fee).add(Cash(-7))
    );
    all.push(evidence);
    assert!(
        replay_records(&all)
            .unwrap_err()
            .contains("duplicate fee recovery")
    );
}

#[test]
fn fee_evidence_refuses_altered_fill_known_commission_and_bad_receipt() {
    let base = records(PositionMode::OneWay);
    let fill = base
        .iter()
        .find_map(|record| {
            if let Record::Execution { fill, .. } = record {
                Some(*fill)
            } else {
                None
            }
        })
        .unwrap();
    let receipt = Nanos(fill.stamp.local.0 + 10);
    let mut known = base.clone();
    known.push(Record::FeeEvidence {
        at: receipt,
        source_ms: 0,
        fill,
        amount: Cash(7),
    });
    assert!(
        replay_records(&known)
            .unwrap_err()
            .contains("already reported")
    );
    let mut changed = base.clone();
    let mut altered = fill;
    altered.price.0 += 1;
    changed.push(Record::FeeEvidence {
        at: receipt,
        source_ms: 0,
        fill: altered,
        amount: Cash(7),
    });
    assert!(replay_records(&changed).unwrap_err().contains("bind"));
    let mut future = base;
    future.push(Record::FeeEvidence {
        at: receipt,
        source_ms: receipt.0 / 1_000_000 + 1,
        fill,
        amount: Cash(7),
    });
    assert!(replay_records(&future).unwrap_err().contains("future"));
    let record = Record::FeeEvidence {
        at: receipt,
        source_ms: 0,
        fill,
        amount: Cash(7),
    };
    let bytes = record.encode();
    for len in 0..bytes.len() {
        assert!(Record::decode(record.kind(), &bytes[..len]).is_none());
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(Record::decode(record.kind(), &trailing).is_none());
}

fn funding_state(
    at_ms: i64,
    pending_ms: Vec<i64>,
    schedule_gaps: Vec<oq_live::funding::ScheduleGap>,
    problem: Option<&str>,
) -> Record {
    Record::FundingState {
        at: Nanos(at_ms * 1_000_000),
        obligations: oq_live::funding::FundingObligations {
            pending_ms,
            schedule_gaps,
            problem: problem.map(str::to_string),
        },
    }
}

#[test]
fn durable_pending_can_only_disappear_after_its_settlement_is_on_the_tape() {
    let base = records(PositionMode::OneWay);
    let mut erased = base.clone();
    erased.push(funding_state(7000, vec![7000], vec![], None));
    erased.push(funding_state(8000, vec![], vec![], None));
    assert!(replay_records(&erased).unwrap_err().contains("disappeared"));
    let mut resolved = base;
    resolved.push(funding_state(7000, vec![7000], vec![], None));
    resolved.push(identified_funding(
        7000,
        vec![(1, 7000, Cash(-5))],
        Cash(-5),
    ));
    resolved.push(funding_state(8000, vec![], vec![], None));
    assert!(
        replay_records(&resolved)
            .unwrap()
            .funding_obligations
            .unwrap()
            .pending_ms
            .is_empty()
    );
    resolved.pop();
    assert!(
        replay_records(&resolved)
            .unwrap()
            .funding_obligations
            .unwrap()
            .pending_ms
            .is_empty(),
        "a booked settlement discharges its obligation even if the final state flush was interrupted"
    );
}

#[test]
fn durable_schedule_gaps_and_uncertainty_cannot_be_erased_or_rewritten() {
    use oq_live::funding::ScheduleGap;
    let mut all = records(PositionMode::OneWay);
    all.push(funding_state(
        7000,
        vec![],
        vec![ScheduleGap {
            since_ms: 6000,
            until_ms: None,
        }],
        Some("schedule unknown"),
    ));
    let mut erased = all.clone();
    erased.push(funding_state(
        8000,
        vec![],
        vec![],
        Some("schedule unknown"),
    ));
    assert!(replay_records(&erased).unwrap_err().contains("disappeared"));
    let mut cleared = all.clone();
    cleared.push(funding_state(
        8000,
        vec![],
        vec![ScheduleGap {
            since_ms: 6000,
            until_ms: Some(8000),
        }],
        None,
    ));
    assert!(
        replay_records(&cleared)
            .unwrap_err()
            .contains("silently cleared")
    );
    all.push(funding_state(
        8000,
        vec![],
        vec![ScheduleGap {
            since_ms: 6000,
            until_ms: Some(8000),
        }],
        Some("schedule unknown"),
    ));
    let report = replay_records(&all).unwrap();
    assert_eq!(
        report.funding_obligations.unwrap().schedule_gaps[0].until_ms,
        Some(8000)
    );
    assert!(!report.funding_coverage);
    all.push(funding_state(
        9000,
        vec![],
        vec![ScheduleGap {
            since_ms: 6000,
            until_ms: Some(9000),
        }],
        Some("schedule unknown"),
    ));
    assert!(replay_records(&all).unwrap_err().contains("rewritten"));
}

#[test]
fn obligation_payload_bounds_flags_and_timestamps_are_validated() {
    use oq_live::funding::ScheduleGap;
    let record = funding_state(
        8000,
        vec![7000],
        vec![ScheduleGap {
            since_ms: 7001,
            until_ms: None,
        }],
        Some("awaiting history"),
    );
    let bytes = record.encode();
    assert_eq!(Record::decode(record.kind(), &bytes), Some(record.clone()));
    for len in 0..bytes.len() {
        assert!(Record::decode(record.kind(), &bytes[..len]).is_none());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Record::decode(record.kind(), &trailing).is_none());
    let mut bad_flag = bytes;
    bad_flag[40] = 2;
    assert!(Record::decode(record.kind(), &bad_flag).is_none());
    for record in [
        funding_state(8000, vec![7000, 7000], vec![], None),
        funding_state(8000, vec![9000], vec![], None),
        funding_state(
            8000,
            vec![],
            vec![ScheduleGap {
                since_ms: 7000,
                until_ms: Some(6000),
            }],
            None,
        ),
    ] {
        let mut all = records(PositionMode::OneWay);
        all.push(record);
        assert!(replay_records(&all).is_err());
    }
}

#[test]
fn same_instant_schedule_loss_and_rediscovery_have_a_valid_durable_boundary() {
    let t = 7000;
    let mut watch = oq_live::funding::FundingWatch::new(Instrument::linear(1, 0), Ok(Some(t)));
    watch.on_time(
        t,
        (QtyLots(0), QtyLots(0)),
        (QtyLots(0), QtyLots(0)),
        || None,
    );
    let mut all = records(PositionMode::OneWay);
    all.push(Record::FundingState {
        at: Nanos(t * 1_000_000),
        obligations: watch.obligations(),
    });
    watch.rediscover_schedule(t, Ok(Some(t + 1000)));
    all.push(Record::FundingState {
        at: Nanos(t * 1_000_000),
        obligations: watch.obligations(),
    });
    let report = replay_records(&all).unwrap();
    let obligations = report.funding_obligations.unwrap();
    assert_eq!(
        obligations.schedule_gaps[0],
        oq_live::funding::ScheduleGap {
            since_ms: t,
            until_ms: Some(t)
        }
    );
    assert!(obligations.problem.is_some());
    assert!(!report.funding_coverage);
}

#[test]
fn recovered_cash_is_replayable_but_has_no_invented_model_amount_or_coverage() {
    let mut all = records(PositionMode::OneWay);
    let recovered = Record::FundingRecovered {
        at: Nanos(10_000_000_000),
        since_ms: 7000,
        through_ms: 9000,
        lines: vec![(1, 8000, Cash(-10)), (2, 8500, Cash(3))],
    };
    assert_eq!(
        Record::decode(recovered.kind(), &recovered.encode()),
        Some(recovered.clone())
    );
    all.push(recovered.clone());
    let report = replay_records(&all).unwrap();
    assert_eq!(report.funding, Cash::from_units(-3).add(Cash(-7)));
    assert_eq!(report.funding_identified_lines, 2);
    assert_eq!(report.funding_unmodelled_lines, 2);
    assert!(!report.funding_coverage);
    all.push(identified_funding(
        9000,
        vec![(1, 8000, Cash(-10))],
        Cash(-10),
    ));
    assert!(replay_records(&all).unwrap_err().contains("duplicate"));
    let bytes = recovered.encode();
    for len in 0..bytes.len() {
        assert!(Record::decode(recovered.kind(), &bytes[..len]).is_none());
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(Record::decode(recovered.kind(), &trailing).is_none());
}

#[test]
fn recovery_ranges_and_receipt_boundaries_are_checked() {
    let base = records(PositionMode::OneWay);
    for (since_ms, through_ms, time_ms) in
        [(9000, 7000, 8000), (7000, 11000, 8000), (7000, 9000, 9500)]
    {
        let mut all = base.clone();
        all.push(Record::FundingRecovered {
            at: Nanos(10_000_000_000),
            since_ms,
            through_ms,
            lines: vec![(1, time_ms, Cash(-10))],
        });
        assert!(replay_records(&all).is_err());
    }
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
