//! The record exists before the order does.
//!
//! That ordering is the only reason a crash is recoverable. Sending first
//! and recording after leaves, on a crash in between, a live order whose
//! client id was never written — and the client id is the one handle that
//! could ask the venue what happened. Recording first leaves a record
//! with no outcome beside it, which is a placement whose answer never
//! arrived: the same question `Submission` answers after a timeout,
//! asked after a restart.
//!
//! These tests read the journal back rather than trusting the calls, and
//! the venue used here refuses to answer until it has been able to see
//! that the record is already on disk.

use std::cell::RefCell;

use oq_gateway::{Execution, NewOrder, OrderAck, Placed, PositionSide, VenueError};
use oq_journal::{Reader, SyncPolicy, Writer};
use oq_live::record::{OutcomeTag, Record, kind};
use oq_live::{Session, SessionConfig};
use oq_risk::{Limits, ProposedOrder, RiskGate};
use oq_types::{Cash, Instrument, Nanos, PriceTicks, QtyLots, Ratio, Side};

/// A venue that reads the journal at the moment it is asked to place an
/// order, so a test can assert what was already durable by then.
struct Watching {
    journal: std::path::PathBuf,
    /// Kinds present in the journal when `place` was called.
    seen_at_place: RefCell<Vec<u16>>,
}

impl Execution for Watching {
    fn place(&self, order: &NewOrder, _i: &Instrument) -> Placed {
        let kinds = Reader::open(&self.journal)
            .and_then(|r| r.replay())
            .map(|r| r.since(0).map(|f| f.kind).collect::<Vec<_>>())
            .unwrap_or_default();
        *self.seen_at_place.borrow_mut() = kinds;
        Placed::Accepted(OrderAck {
            venue_id: 1,
            client_id: order.client_id.clone(),
            status: "NEW".into(),
            executed_qty: "0".into(),
        })
    }
    fn cancel(&self, _s: &str, c: &str) -> Placed {
        Placed::Accepted(OrderAck {
            venue_id: 0,
            client_id: c.into(),
            status: "CANCELED".into(),
            executed_qty: "0".into(),
        })
    }
    fn order_status(&self, _s: &str, _c: &str) -> Result<Option<OrderAck>, VenueError> {
        Ok(None)
    }
}

fn limits() -> Limits {
    Limits {
        max_order_qty: QtyLots(100),
        max_position_qty: QtyLots(1000),
        max_order_notional: Cash(1_000_000 * oq_types::CASH_SCALE),
        price_band: Ratio(500_000_000),
        max_working: 10,
        max_rate: 100,
        rate_window: Nanos(1_000_000_000),
    }
}

fn buy() -> ProposedOrder {
    ProposedOrder {
        side: Side::Buy,
        limit_price: Some(PriceTicks(6_000_000)),
        qty: QtyLots(1),
        reduce_only: false,
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oq-live-journal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

fn records(path: &std::path::Path) -> Vec<Record> {
    let reader = Reader::open(path).expect("open");
    let replay = reader.replay().expect("replay");
    replay
        .since(0)
        .filter_map(|f| Record::decode(f.kind, &f.payload))
        .collect()
}

#[test]
fn the_order_is_on_disk_before_the_venue_is_called() {
    let path = temp("before.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.submit(buy(), PriceTicks(6_000_000), Nanos(7));

    let seen = s.venue().seen_at_place.borrow().clone();
    assert!(
        seen.contains(&kind::SUBMITTED),
        "the submission must be durable before the venue is called; saw {seen:?}"
    );
    assert!(
        !seen.contains(&kind::OUTCOME),
        "the outcome cannot be known yet; saw {seen:?}"
    );
}

#[test]
fn a_run_writes_its_identity_the_order_and_the_outcome_in_that_order() {
    let path = temp("sequence.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.submit(buy(), PriceTicks(6_000_000), Nanos(7));

    let all = records(&path);
    assert!(
        matches!(all.first(), Some(Record::SessionStart { .. })),
        "{all:?}"
    );
    let submitted = all
        .iter()
        .position(|r| matches!(r, Record::Submitted { .. }));
    let outcome = all.iter().position(|r| matches!(r, Record::Outcome { .. }));
    assert!(submitted < outcome, "submitted before outcome: {all:?}");
    match &all[outcome.expect("an outcome")] {
        Record::Outcome { tag, client_id, .. } => {
            assert_eq!(*tag, OutcomeTag::Accepted);
            assert!(client_id.starts_with("oq-"), "{client_id}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_refusal_is_recorded_and_no_order_is() {
    // The gate said no, so nothing was sent — and a journal that showed a
    // submission here would describe an order that never existed.
    let path = temp("refused.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(Limits::closed()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.submit(buy(), PriceTicks(6_000_000), Nanos(7));

    let all = records(&path);
    assert!(
        all.iter().any(|r| matches!(r, Record::Refused { .. })),
        "the refusal is part of the record: {all:?}"
    );
    assert!(
        !all.iter().any(|r| matches!(r, Record::Submitted { .. })),
        "nothing was sent, so nothing may claim to have been: {all:?}"
    );
}

#[test]
fn a_session_without_a_journal_still_trades() {
    // Journalling is a choice a caller makes, not a precondition. A
    // session that required one would make the audit trail a reason not
    // to trade.
    let path = temp("unused.oqj");
    let venue = Watching {
        journal: path,
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts");
    assert!(s.submit(buy(), PriceTicks(6_000_000), Nanos(7)).is_sent());
}

/// A position taken over at startup is in the record, with its basis.
///
/// This one was written because the record existed and nothing wrote it:
/// `Record::Reconciled` had a kind, an encoder, a decoder, a round-trip
/// test and a line in `oq-replay` that rendered it — and no construction
/// site anywhere in the tree. `--adopt-existing` was therefore the one
/// startup step that left no trace, which is the step a migration is
/// made of. A reader rebuilding what this run believes it holds would
/// have come up short by exactly the migrated positions.
#[test]
fn a_position_taken_over_at_startup_is_recorded_with_its_basis() {
    let path = temp("reconciled.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.record_reconciled(
        Nanos(3),
        vec![("ETHUSDT".into(), "LONG".into(), 160, 250_000)],
    );

    let all = records(&path);
    let at = all
        .iter()
        .position(|r| matches!(r, Record::Reconciled { .. }))
        .unwrap_or_else(|| panic!("the adoption must be in the journal: {all:?}"));
    // After the identity, because a record whose run cannot be named is
    // a record about nothing.
    assert!(
        matches!(all.first(), Some(Record::SessionStart { .. })),
        "{all:?}"
    );
    assert!(at > 0, "the identity comes first: {all:?}");

    match &all[at] {
        Record::Reconciled { legs, .. } => {
            assert_eq!(legs.len(), 1, "{legs:?}");
            // The entry price is the half that makes this usable. Side
            // and size alone say a position exists; without its basis
            // there is no unrealised figure and nothing to compare
            // against the venue.
            assert_eq!(legs[0].3, 250_000, "the basis travels with the leg");
            assert_eq!(legs[0].2, 160);
        }
        other => panic!("{other:?}"),
    }
}

/// Taking over nothing writes nothing.
///
/// A record saying "adopted no positions" and no record at all are the
/// same claim, and only one of them can be mistaken for a run that
/// forgot to look.
#[test]
fn taking_over_nothing_is_not_recorded_as_an_event() {
    let path = temp("reconciled-empty.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.record_reconciled(Nanos(3), Vec::new());

    let all = records(&path);
    assert!(
        !all.iter().any(|r| matches!(r, Record::Reconciled { .. })),
        "nothing was taken over, so nothing may claim to have been: {all:?}"
    );
}

/// What the strategy was waiting for is in the record.
///
/// Every other record in a journal is something that happened. A run
/// that places no orders produces almost none of them, and is therefore
/// the run hardest to explain — which is backwards, because it is also
/// the one most likely to be wrong.
///
/// This was not hypothetical. A twelve-hour run placed nothing, and the
/// reason — a gate whose threshold this deployment never reached, and a
/// warm-up not yet finished — was reachable only by reading the
/// strategy's source. That is the worst tool to reach for while
/// something is going wrong on a venue.
#[test]
fn what_the_strategy_waits_for_is_recorded() {
    let path = temp("waiting.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.record_waiting(
        Nanos(5),
        vec![("bars".into(), 15), ("volume_gate".into(), 0)],
    );

    let all = records(&path);
    match all
        .iter()
        .find(|r| matches!(r, Record::Waiting { .. }))
        .unwrap_or_else(|| panic!("the wait must be in the journal: {all:?}"))
    {
        Record::Waiting { entries, .. } => {
            // By name and value, because "3 conditions" explains nothing.
            assert_eq!(entries.len(), 2, "{entries:?}");
            assert_eq!(entries[0], ("bars".to_string(), 15));
            assert_eq!(entries[1], ("volume_gate".to_string(), 0));
        }
        other => panic!("{other:?}"),
    }
}

/// A strategy that names no conditions writes no record.
///
/// An empty one would claim the question was asked and answered, when
/// it was asked and declined — and a reader counting them would see a
/// run that reported its state throughout and said nothing.
#[test]
fn a_strategy_that_names_nothing_writes_nothing() {
    let path = temp("waiting-empty.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.record_waiting(Nanos(5), Vec::new());

    let all = records(&path);
    assert!(
        !all.iter().any(|r| matches!(r, Record::Waiting { .. })),
        "nothing was named, so nothing may claim to have been: {all:?}"
    );
}

/// A fill is written with the id the strategy knew it by.
///
/// `Record::Fill` had an encoder, a decoder, a round-trip test, a line
/// in `oq-replay` that counted it and a reconstruction in `belief` that
/// read it — and nothing in the tree wrote one. The journal contained
/// no fills at all, so nothing could be replayed from it.
///
/// The order id is the half that makes a replay worth doing. A position
/// can be recovered from the venue; which of a ladder's rungs had
/// filled cannot, because only this strategy ever knew.
#[test]
fn a_fill_is_recorded_with_the_id_the_strategy_knew_it_by() {
    let path = temp("fills.oqj");
    let _ = std::fs::remove_file(&path);
    let venue = Watching {
        journal: path.clone(),
        seen_at_place: RefCell::new(Vec::new()),
    };
    let mut s = Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "ETHUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "oq".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("starts")
    .journalling(Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("writer"));

    s.record_fill(
        &oq_types::Fill {
            stamp: oq_types::Stamp::new(9, 9),
            instrument: oq_types::InstrumentId::new(1),
            order: oq_types::OrderId(7),
            trade: oq_types::TradeId(481_923),
            side: Side::Sell,
            offset: oq_types::Offset::Close,
            price: PriceTicks(6_000_000),
            qty: QtyLots(8),
            liquidity: oq_types::Liquidity::Maker,
        },
        "oq-1",
    );

    let all = records(&path);
    match all
        .iter()
        .find(|r| matches!(r, Record::Fill { .. }))
        .unwrap_or_else(|| panic!("the fill must be in the journal: {all:?}"))
    {
        Record::Fill {
            order,
            side,
            qty,
            price,
            client_id,
            ..
        } => {
            assert_eq!(*order, 7, "the strategy's own id, not the venue's");
            assert_eq!(side, "Sell");
            assert_eq!(client_id, "oq-1");
            // At the instrument's precision, so a reader parses back the
            // number that was booked rather than one off by a factor.
            assert_eq!(qty, "0.008");
            assert_eq!(price, "60000.00");
        }
        other => panic!("{other:?}"),
    }
}
