//! A non-zero independent P&L gap can have a measured zero residual.

use oq_core::{Event, Fees, State};
use oq_gateway::account::ExecutionFee;
use oq_live::{
    Record, ledger,
    ledger_coverage::{Completeness, CoverageEvidence, HistoryWindow},
    shadow::Shadow,
};
use oq_parity::{BaselineStatus, RunManifest};
use oq_types::{
    Cash, Fee, Fill, Instrument, InstrumentId, Liquidity, Nanos, Offset, OrderId, PriceTicks,
    QtyLots, Side, Stamp, TradeId,
};

fn shadow(balance: i64, policy: Option<Fees>) -> Shadow {
    let instrument = Instrument::linear(0, 0);
    let mut state = State::new(
        InstrumentId(1),
        oq_margin::Contract::of(&instrument).unwrap(),
        oq_margin::TierTable::example_btcusdt(),
        Cash::from_units(balance),
    );
    if let Some(fees) = policy {
        state = state.with_fees(fees);
    }
    let mut shadow = Shadow::from_state(state);
    let tick = |at: i64, price| oq_engine::Tick {
        stamp: Stamp::new(at, at),
        last: PriceTicks(price),
        high: PriceTicks(price),
        low: PriceTicks(price),
        bid: PriceTicks(price),
        ask: PriceTicks(price),
        volume: QtyLots(1),
    };
    shadow.on_tick(tick(0, 100));
    for (id, side, price, submitted, filled) in
        [(1, Side::Buy, 100, 1, 2), (2, Side::Sell, 102, 4, 5)]
    {
        shadow.apply(&Event::Submit {
            instrument: Some(InstrumentId(1)),
            id: OrderId(id),
            side,
            price: Some(PriceTicks(price)),
            qty: QtyLots(1),
            offset: if side == Side::Buy {
                Offset::Open
            } else {
                Offset::Close
            },
            stamp: Stamp::new(submitted * 1_000_000, submitted * 1_000_000),
        });
        shadow.on_tick(tick(filled * 1_000_000, price));
        shadow.on_venue_fill(
            OrderId(id),
            side,
            PriceTicks(price),
            QtyLots(1),
            Nanos(filled * 1_000_000),
        );
    }
    shadow
}

#[test]
fn residual_requires_native_coverage_seed_fills_and_independent_totals() {
    let path =
        std::env::temp_dir().join(format!("oq-covered-attribution-{}.oqj", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let instrument = Instrument::linear(0, 0);
    let mut records = vec![
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
            mode: oq_core::PositionMode::OneWay,
        },
        Record::Reconciled {
            at: Nanos(0),
            legs: vec![],
        },
    ];
    let mut rows = Vec::new();
    for (id, side, offset, price, filled) in [
        (1, Side::Buy, Offset::Open, 100, 2),
        (2, Side::Sell, Offset::Close, 102, 5),
    ] {
        records.push(Record::Execution {
            client_id: format!("fixture-{id}"),
            fill: Fill {
                stamp: Stamp::new(filled * 1_000_000, (filled + 1) * 1_000_000),
                instrument: InstrumentId(1),
                order: OrderId(id),
                trade: TradeId(id),
                side,
                offset,
                price: PriceTicks(price),
                qty: QtyLots(1),
                liquidity: Liquidity::Maker,
            },
            fee: Fee::Reported(Cash::from_units(1)),
        });
        rows.push(ExecutionFee {
            trade_id: id,
            side,
            time_ms: filled,
            amount: Cash::from_units(1),
        });
    }
    records.push(Record::FundingState {
        at: Nanos(10_000_000),
        obligations: oq_live::funding::FundingObligations {
            pending_ms: vec![],
            schedule_gaps: vec![],
            problem: None,
        },
    });
    let mut writer =
        oq_journal::Writer::open(&path, oq_journal::SyncPolicy::EveryRecordNoFsync).unwrap();
    for record in records {
        writer.append(record.kind(), &record.encode()).unwrap();
    }
    drop(writer);
    let code = "a".repeat(40);
    let native = ledger::replay_bound(&path, &code, None).unwrap();
    let scope = "b".repeat(64);
    let evidence = CoverageEvidence {
        journal_data_hash: native.manifest.data_hash.clone(),
        account_scope_hash: scope.clone(),
        since_ms: 0,
        through_ms: 10,
        evaluated_ms: 20,
        max_age_ms: 10,
        fees: HistoryWindow {
            account_scope_hash: scope.clone(),
            symbol: "SYNTH".into(),
            since_ms: 0,
            through_ms: 10,
            observed_ms: 20,
            completeness: Completeness::Complete,
            rows,
        },
        funding: HistoryWindow {
            account_scope_hash: scope,
            symbol: "SYNTH".into(),
            since_ms: 0,
            through_ms: 10,
            observed_ms: 20,
            completeness: Completeness::Complete,
            rows: vec![],
        },
    };
    let policy = RunManifest::from_content(
        &code,
        b"symbol",
        b"declared zero rates and limits",
        "fixture policy",
    );
    let mut model = shadow(1000, Some(Fees::none()));
    assert_eq!(model.model_pnl(), Cash::from_units(2));
    let report = ledger::attribute_with_history(
        &path,
        &code,
        &evidence,
        &mut model,
        Ok((Cash::ZERO, Cash::ZERO)),
        &policy,
        Cash::ZERO,
    )
    .unwrap();
    assert_eq!(report.gap, Cash::from_units(-2));
    assert_eq!(report.residual, Some(Cash::ZERO));
    assert!(report.is_complete());
    assert_eq!(
        report.manifest.label,
        "openquanter covered live attribution v1"
    );
    let changed_code = ledger::attribute_with_history(
        &path,
        &"c".repeat(40),
        &evidence,
        &mut model,
        Ok((Cash::ZERO, Cash::ZERO)),
        &policy,
        Cash::ZERO,
    )
    .unwrap();
    assert_eq!(
        report.manifest.compare(&changed_code.manifest),
        BaselineStatus::CodeChanged
    );
    let unknown = ledger::attribute_with_history(
        &path,
        &code,
        &evidence,
        &mut shadow(1000, None),
        Ok((Cash::ZERO, Cash::ZERO)),
        &policy,
        Cash::ZERO,
    )
    .unwrap();
    assert_eq!(unknown.residual, None);
    assert!(
        ledger::attribute_with_history(
            &path,
            &code,
            &evidence,
            &mut shadow(1001, Some(Fees::none())),
            Ok((Cash::ZERO, Cash::ZERO)),
            &policy,
            Cash::ZERO
        )
        .is_err()
    );
    assert!(
        ledger::attribute_with_history(
            &path,
            &code,
            &evidence,
            &mut model,
            Ok((Cash::ZERO, Cash::ZERO)),
            &policy,
            Cash(1)
        )
        .is_err()
    );
    let inconsistent = ledger::attribute_with_history(
        &path,
        &code,
        &evidence,
        &mut model,
        Ok((Cash::ZERO, Cash(1))),
        &policy,
        Cash::ZERO,
    )
    .unwrap();
    assert_eq!(inconsistent.residual, None);
    let mut changed = evidence.clone();
    changed.fees.rows[0].amount = Cash::ZERO;
    assert!(
        ledger::attribute_with_history(
            &path,
            &code,
            &changed,
            &mut model,
            Ok((Cash::ZERO, Cash::ZERO)),
            &policy,
            Cash::ZERO
        )
        .is_err()
    );
    model.on_venue_fill(
        OrderId(99),
        Side::Buy,
        PriceTicks(100),
        QtyLots(1),
        Nanos(6_000_000),
    );
    assert!(
        ledger::attribute_with_history(
            &path,
            &code,
            &evidence,
            &mut model,
            Ok((Cash::ZERO, Cash::ZERO)),
            &policy,
            Cash::ZERO
        )
        .is_err()
    );
    std::fs::remove_file(path).unwrap();
}
