//! Reconstructing a process's own account from what it wrote down.
//!
//! The cutover question these answer is narrow and was unanswerable
//! until adopted positions were journalled: **does the new process agree
//! with the venue about a position it was handed?** `oq-recon` compares
//! the venue against a record and catches the position moving. It cannot
//! catch the process reading a position that did not move, which is what
//! step 5 of the playbook actually exposes.

use oq_journal::{SyncPolicy, Writer};
use oq_live::belief::Belief;
use oq_live::record::{OutcomeTag, Record};
use oq_types::{Nanos, PriceTicks, QtyLots, Side};

fn write(path: &std::path::Path, records: &[Record]) {
    let mut w = Writer::open(path, SyncPolicy::EveryRecord).expect("journal opens");
    for r in records {
        w.append(r.kind(), &r.encode()).expect("append");
    }
    w.sync().expect("sync");
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("oq-belief-{}-{name}.oqj", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("lock"));
    p
}

fn start() -> Record {
    Record::SessionStart {
        prefix: "oq".into(),
        symbol: "BTCUSDT".into(),
        price_scale: 2,
        qty_scale: 3,
    }
}

/// A position taken over at startup is in the belief.
///
/// This is the whole reason the module exists. Before `Reconciled` was
/// written, a run started with `--adopt-existing` produced a journal
/// that reconstructed as flat — short by exactly the position the
/// cutover was carrying, which is the one number step 5 turns on.
#[test]
fn an_adopted_position_is_reconstructed() {
    let p = tmp("adopted");
    write(
        &p,
        &[
            start(),
            Record::Reconciled {
                at: Nanos(1),
                legs: vec![("BTCUSDT".into(), "LONG".into(), 256, 7_144_487)],
            },
        ],
    );
    let b = Belief::from_journal(&p).expect("readable");
    assert!(b.adopted, "the adoption record was not seen");
    assert_eq!(b.position_lots, 256);
    assert_eq!(b.entry_ticks, 7_144_487);
    let r = b.to_record(0);
    assert_eq!(r.legs.len(), 1);
    assert_eq!(r.legs[0].0, "LONG");
    // 256 lots at qty_scale 3, 7_144_487 ticks at price_scale 2.
    assert!((r.legs[0].1 - 0.256).abs() < 1e-9, "{:?}", r.legs[0]);
    assert!((r.legs[0].2 - 71_444.87).abs() < 1e-6, "{:?}", r.legs[0]);
}

/// A journal with no adoption record says so.
///
/// A flat reconstruction means *flat* or means *a position nobody wrote
/// down*, and nothing in the file separates them. Reporting `adopted`
/// lets a caller tell an old journal from an empty account instead of
/// reading the second as the first.
#[test]
fn a_journal_without_an_adoption_record_does_not_claim_flat() {
    let p = tmp("noadopt");
    write(&p, &[start()]);
    let b = Belief::from_journal(&p).expect("readable");
    assert_eq!(b.position_lots, 0);
    assert!(!b.adopted, "there was no Reconciled record to see");
}

/// Fills move the position in the direction of the submission that
/// produced them.
///
/// A fill record names a client id and a quantity; the direction lives
/// in the submission it answers. Getting that lookup wrong reports the
/// opposite position, and it would be entirely plausible-looking.
#[test]
fn fills_are_applied_in_the_direction_of_their_submission() {
    let p = tmp("fills");
    write(
        &p,
        &[
            start(),
            Record::Submitted {
                at: Nanos(1),
                client_id: "oq-1".into(),
                side: Side::Buy,
                limit_price: PriceTicks(0),
                qty: QtyLots(2_000),
                reduce_only: false,
            },
            Record::Outcome {
                at: Nanos(2),
                client_id: "oq-1".into(),
                tag: OutcomeTag::Accepted,
                detail: String::new(),
            },
            Record::Fill {
                at: Nanos(3),
                client_id: "oq-1".into(),
                trade_id: 1,
                qty: "2".into(),
                price: "100.00".into(),
            },
            Record::Submitted {
                at: Nanos(4),
                client_id: "oq-2".into(),
                side: Side::Sell,
                limit_price: PriceTicks(0),
                qty: QtyLots(1_000),
                reduce_only: true,
            },
            Record::Outcome {
                at: Nanos(5),
                client_id: "oq-2".into(),
                tag: OutcomeTag::Accepted,
                detail: String::new(),
            },
            Record::Fill {
                at: Nanos(6),
                client_id: "oq-2".into(),
                trade_id: 2,
                qty: "1".into(),
                price: "150.00".into(),
            },
        ],
    );
    let b = Belief::from_journal(&p).expect("readable");
    assert_eq!(b.position_lots, 1_000, "bought 2, sold 1");
    assert_eq!(b.entry_ticks, 10_000, "a reduction must not move the entry");
    assert!(b.resting.is_empty(), "both orders filled: {:?}", b.resting);
}

/// A refused submission is not resting, and neither is an unresolved
/// one.
///
/// Rejected is easy. Unknown is the one that matters: listing it as
/// resting would assert that an order exists when nobody knows, which is
/// exactly what `Placed::Unknown` exists to refuse to assert. It belongs
/// to `recovery::recover`, whose whole job is unresolved orders.
#[test]
fn only_accepted_and_unfilled_orders_are_resting() {
    let p = tmp("resting");
    let sub = |id: &str, at: i64| Record::Submitted {
        at: Nanos(at),
        client_id: id.into(),
        side: Side::Buy,
        limit_price: PriceTicks(9_000),
        qty: QtyLots(1_000),
        reduce_only: false,
    };
    let out = |id: &str, at: i64, tag: OutcomeTag| Record::Outcome {
        at: Nanos(at),
        client_id: id.into(),
        tag,
        detail: String::new(),
    };
    write(
        &p,
        &[
            start(),
            sub("oq-1", 1),
            out("oq-1", 2, OutcomeTag::Accepted),
            sub("oq-2", 3),
            out("oq-2", 4, OutcomeTag::Rejected),
            sub("oq-3", 5),
            out("oq-3", 6, OutcomeTag::Unknown),
            sub("oq-4", 7),
        ],
    );
    let b = Belief::from_journal(&p).expect("readable");
    assert_eq!(
        b.resting,
        vec!["oq-1".to_string()],
        "only the accepted, unfilled order rests: {:?}",
        b.resting
    );
}

/// The reconstruction and a record written by `oq-recon` compare with
/// no differences when they describe the same account.
///
/// This is the actual cutover check, in one assertion: the record is
/// what the venue said at step 2, and the belief is what the new
/// process thinks at step 5.
#[test]
fn a_correct_belief_agrees_with_the_record_the_venue_produced() {
    let p = tmp("agree");
    write(
        &p,
        &[
            start(),
            Record::Reconciled {
                at: Nanos(1),
                legs: vec![("BTCUSDT".into(), "LONG".into(), 256, 7_144_487)],
            },
        ],
    );
    let belief = Belief::from_journal(&p).expect("readable").to_record(0);

    let venue = oq_gateway::record::Record {
        symbol: "BTCUSDT".into(),
        read_at_ms: 1_700_000_000_000,
        legs: vec![("LONG".to_string(), 0.256, 71_444.87)],
        orders: Vec::new(),
    };
    assert!(
        venue.differences(&belief).is_empty(),
        "a correct belief disagreed with the venue: {:?}",
        venue.differences(&belief)
    );

    // And a wrong one does not pass. A comparison that cannot fail is
    // the same as no comparison, and this is the one being relied on at
    // the point in the procedure where the position is naked.
    let wrong = oq_gateway::record::Record {
        legs: vec![("LONG".to_string(), 0.256, 71_000.00)],
        ..venue
    };
    assert!(!wrong.differences(&belief).is_empty());
}
