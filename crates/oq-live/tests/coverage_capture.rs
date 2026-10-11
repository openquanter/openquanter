//! The producer join queries fixed bounds and preserves the original rows.

use oq_gateway::account::{BoundedHistory, ExecutionFee, FundingCharge};
use oq_gateway::exec::{Execution, NewOrder, OrderAck, Placed, UserStream};
use oq_gateway::{Account, VenueError};
use oq_live::{Record, ledger};
use oq_types::{Cash, Instrument, InstrumentId, Nanos};

struct HistoryAccount {
    unsupported: bool,
    surplus: bool,
}
impl Execution for HistoryAccount {
    fn place(&self, _: &NewOrder, _: &Instrument) -> Placed {
        panic!("coverage must not place")
    }
    fn cancel(&self, _: &str, _: &str) -> Placed {
        panic!("coverage must not cancel")
    }
    fn order_status(&self, _: &str, _: &str) -> Result<Option<OrderAck>, VenueError> {
        panic!("coverage must not query orders")
    }
}
impl Account for HistoryAccount {
    fn id(&self) -> &'static str {
        "synthetic"
    }
    fn id_rules(&self) -> oq_gateway::broker::IdRules {
        panic!("not needed")
    }
    fn recent_bars(&self, _: &str, _: usize) -> Result<Vec<oq_gateway::klines::Kline>, VenueError> {
        panic!("not needed")
    }
    fn sync_clock(&mut self) -> Result<i64, VenueError> {
        panic!("not needed")
    }
    fn round_trip_ms(&self) -> i64 {
        0
    }
    fn instrument(&self, _: &str) -> Result<Instrument, String> {
        panic!("not needed")
    }
    fn is_hedged(&self) -> Result<bool, VenueError> {
        panic!("not needed")
    }
    fn positions(&self, _: &str) -> Result<Vec<oq_gateway::PositionSnapshot>, VenueError> {
        panic!("not needed")
    }
    fn balances(&self) -> Result<oq_gateway::AccountSnapshot, VenueError> {
        panic!("not needed")
    }
    fn open_orders(&self, _: &str) -> Result<Vec<oq_gateway::OpenOrder>, VenueError> {
        panic!("not needed")
    }
    fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        panic!("not needed")
    }
    fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        panic!("not needed")
    }
    fn close_user_stream(&self) -> Result<(), VenueError> {
        panic!("not needed")
    }
    fn execution_fees_between(
        &self,
        symbol: &str,
        since_ms: i64,
        through_ms: i64,
    ) -> Result<Option<BoundedHistory<ExecutionFee>>, VenueError> {
        assert_eq!((symbol, since_ms, through_ms), ("SYNTH", 0, 20));
        if self.unsupported {
            return Ok(None);
        }
        Ok(Some(BoundedHistory {
            symbol: symbol.into(),
            since_ms,
            through_ms,
            observed_ms: 25,
            rows: if self.surplus {
                vec![ExecutionFee {
                    trade_id: 1,
                    side: oq_types::Side::Buy,
                    time_ms: 10,
                    amount: Cash::ZERO,
                }]
            } else {
                vec![]
            },
        }))
    }
    fn funding_charged_between(
        &self,
        symbol: &str,
        since_ms: i64,
        through_ms: i64,
    ) -> Result<Option<BoundedHistory<FundingCharge>>, VenueError> {
        assert_eq!((symbol, since_ms, through_ms), ("SYNTH", 0, 20));
        Ok(Some(BoundedHistory {
            symbol: symbol.into(),
            since_ms,
            through_ms,
            observed_ms: 27,
            rows: vec![],
        }))
    }
}

#[test]
fn capture_queries_the_native_boundary_and_refuses_unsupported_surplus_or_stale_sources() {
    let path = std::env::temp_dir().join(format!("oq-capture-history-{}.oqj", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let instrument = Instrument::linear(0, 0);
    let records = [
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
    let mut writer =
        oq_journal::Writer::open(&path, oq_journal::SyncPolicy::EveryRecordNoFsync).unwrap();
    for record in records {
        writer.append(record.kind(), &record.encode()).unwrap();
    }
    drop(writer);
    let clock = oq_live::clock::ManualClock::starting_at(Nanos(30_000_000));
    let account = HistoryAccount {
        unsupported: false,
        surplus: false,
    };
    let scope = "b".repeat(64);
    let (report, evidence) =
        ledger::capture_history(&path, "unknown", &account, &scope, 20, &clock, 10).unwrap();
    assert!(report.native.accounting.funding_coverage);
    assert_eq!(evidence.evaluated_ms, 30);
    assert_eq!(
        (evidence.fees.observed_ms, evidence.funding.observed_ms),
        (25, 27)
    );
    assert_eq!(
        report.coverage,
        ledger::replay_with_history(&path, "unknown", &evidence)
            .unwrap()
            .coverage
    );
    for account in [
        HistoryAccount {
            unsupported: true,
            surplus: false,
        },
        HistoryAccount {
            unsupported: false,
            surplus: true,
        },
    ] {
        assert!(
            ledger::capture_history(&path, "unknown", &account, &scope, 20, &clock, 10).is_err()
        );
    }
    assert!(ledger::capture_history(&path, "unknown", &account, &scope, 20, &clock, 2).is_err());
    std::fs::remove_file(path).unwrap();
}
