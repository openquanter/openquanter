//! A strategy's ids are not the venue's, and losing the association
//! does not fail loudly.
//!
//! A strategy says "cancel 7". Seven is a number it made up; the venue
//! has never heard of it. If the mapping is lost, the cancel is sent
//! for an id nobody recognises or is not sent at all, and the order
//! keeps resting while the strategy believes it is gone. Nothing
//! errors. These tests are about that mapping.

use std::cell::RefCell;

use oq_gateway::{Execution, NewOrder, OrderAck, Placed, PositionSide, VenueError};
use oq_live::{Outcome, Session, SessionConfig, Trader};
use oq_risk::{Limits, RiskGate};
use oq_strategy::{Context, Intent, Strategy};
use oq_types::{Cash, Instrument, Nanos, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side};

/// A venue that accepts everything and gives each order a fresh id.
struct Accepting {
    n: RefCell<u64>,
    cancelled: RefCell<Vec<String>>,
}

impl Accepting {
    fn new() -> Self {
        Self {
            n: RefCell::new(0),
            cancelled: RefCell::new(Vec::new()),
        }
    }
}

impl Execution for Accepting {
    fn place(&self, order: &NewOrder, _i: &Instrument) -> Placed {
        *self.n.borrow_mut() += 1;
        Placed::Accepted(OrderAck {
            venue_id: *self.n.borrow() as i64,
            client_id: order.client_id.clone(),
            status: "NEW".into(),
            executed_qty: "0".into(),
        })
    }
    fn cancel(&self, _symbol: &str, client_id: &str) -> Placed {
        self.cancelled.borrow_mut().push(client_id.to_string());
        Placed::Accepted(OrderAck {
            venue_id: 0,
            client_id: client_id.to_string(),
            status: "CANCELED".into(),
            executed_qty: "0".into(),
        })
    }
    fn order_status(&self, _s: &str, _c: &str) -> Result<Option<OrderAck>, VenueError> {
        Ok(None)
    }
}

/// Emits whatever it is told to, once.
struct Scripted(Vec<Intent>);

impl Strategy for Scripted {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        out.append(&mut self.0);
    }
    fn name(&self) -> &str {
        "scripted"
    }
}

fn ctx() -> Context {
    Context {
        tick: oq_engine::Tick {
            last: PriceTicks(6_000_000),
            ..oq_engine::Tick::default()
        },
        position: QtyLots(0),
        entry: PriceTicks(0),
        short_position: QtyLots(0),
        short_entry: PriceTicks(0),
        equity: Cash(0),
        working: 0,
    }
}

fn trader(intents: Vec<Intent>) -> Trader<Scripted, Accepting> {
    let session = Session::start(
        Accepting::new(),
        RiskGate::new(Limits {
            max_order_qty: QtyLots(100),
            max_position_qty: QtyLots(1000),
            max_order_notional: Cash(1_000_000 * oq_types::CASH_SCALE),
            price_band: Ratio(500_000_000),
            max_working: 10,
            max_rate: 100,
            rate_window: Nanos(1_000_000_000),
        }),
        SessionConfig {
            symbol: "BTCUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "live".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("clean venue");
    Trader::new(Scripted(intents), session)
}

fn limit(id: u64) -> Intent {
    Intent::Limit {
        id: OrderId(id),
        side: Side::Buy,
        price: PriceTicks(6_000_000),
        qty: QtyLots(1),
        offset: Offset::Open,
    }
}

#[test]
fn an_order_is_sent_and_its_two_ids_are_remembered_together() {
    let mut t = trader(vec![limit(7)]);
    let out = t.on_tick(&ctx(), Nanos(0));
    match &out[0] {
        Outcome::Sent { local, client_id } => {
            assert_eq!(*local, OrderId(7), "the strategy's own number");
            assert!(client_id.starts_with("live-"), "the venue's: {client_id}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(t.resting().len(), 1);
}

#[test]
fn a_cancel_reaches_the_venue_under_the_id_the_venue_knows() {
    // The point of the map. The strategy says seven; the venue is told
    // the string it was given when the order was accepted.
    let mut t = trader(vec![limit(7)]);
    let sent = t.on_tick(&ctx(), Nanos(0));
    let Outcome::Sent { client_id, .. } = &sent[0] else {
        panic!("{sent:?}")
    };
    let expected = client_id.clone();

    let mut t2 = trader(vec![limit(7), Intent::Cancel(OrderId(7))]);
    let out = t2.on_tick(&ctx(), Nanos(0));
    match &out[1] {
        Outcome::Cancelled { local, client_id } => {
            assert_eq!(*local, OrderId(7));
            assert_eq!(*client_id, expected);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(t2.session().venue().cancelled.borrow().len(), 1);
}

#[test]
fn cancelling_an_order_this_process_never_sent_is_reported_not_swallowed() {
    // It means the strategy and this process disagree about what is
    // resting, and a strategy acting on "my orders are gone" when they
    // are not will keep sizing against a position about to change.
    let mut t = trader(vec![Intent::Cancel(OrderId(99))]);
    let out = t.on_tick(&ctx(), Nanos(0));
    assert_eq!(out[0], Outcome::UnknownOrder(OrderId(99)));
    assert!(t.session().venue().cancelled.borrow().is_empty());
}

#[test]
fn cancel_all_reaches_every_resting_order() {
    let mut t = trader(vec![limit(1), limit(2), limit(3), Intent::CancelAll]);
    t.on_tick(&ctx(), Nanos(0));
    assert_eq!(
        t.session().venue().cancelled.borrow().len(),
        3,
        "all three, not just the first"
    );
}

#[test]
fn forgetting_an_ended_order_removes_it_from_the_map() {
    // The venue said the order is gone. Keeping the association would
    // make a later cancel refer to something that no longer exists.
    let mut t = trader(vec![limit(7)]);
    let out = t.on_tick(&ctx(), Nanos(0));
    let Outcome::Sent { client_id, .. } = &out[0] else {
        panic!("{out:?}")
    };
    let id = client_id.clone();
    assert_eq!(t.resting().len(), 1);
    t.forget(&id);
    assert!(t.resting().is_empty());
}

#[test]
fn a_close_intent_becomes_a_reduce_only_order() {
    // The strategy's offset is how it says "get me out", and dropping
    // it turns an exit into an entry in the opposite direction.
    let mut t = trader(vec![Intent::Limit {
        id: OrderId(1),
        side: Side::Sell,
        price: PriceTicks(6_000_000),
        qty: QtyLots(1),
        offset: Offset::Close,
    }]);
    let out = t.on_tick(&ctx(), Nanos(0));
    assert!(matches!(out[0], Outcome::Sent { .. }), "{out:?}");
}

#[test]
fn a_refused_order_is_not_remembered_as_resting() {
    // Remembering it would make a later cancel address an order that
    // does not exist, and the venue's refusal of that cancel would be
    // the first anyone heard of the problem.
    let mut t = trader(vec![limit(7)]);
    t.session_mut().gate().kill_switch().trip();
    let out = t.on_tick(&ctx(), Nanos(0));
    assert!(matches!(out[0], Outcome::Refused { .. }), "{out:?}");
    assert!(t.resting().is_empty());
}

// ---------------------------------------------------------------------
// Unanswered submissions
// ---------------------------------------------------------------------

/// A venue that takes the order and then goes quiet: `place` returns
/// `Unknown`, and asking about it does not settle the question either.
///
/// This is the shape of a timeout on a request that may well have been
/// executed — the one case where the account and this process can
/// silently disagree.
struct Silent;

impl Execution for Silent {
    fn place(&self, order: &NewOrder, _i: &Instrument) -> Placed {
        Placed::Unknown(oq_gateway::Unresolved {
            client_id: order.client_id.clone(),
            reason: "timed out waiting for a response".into(),
        })
    }
    fn cancel(&self, _symbol: &str, client_id: &str) -> Placed {
        Placed::Unknown(oq_gateway::Unresolved {
            client_id: client_id.to_string(),
            reason: "timed out".into(),
        })
    }
    fn order_status(&self, _s: &str, _c: &str) -> Result<Option<OrderAck>, VenueError> {
        Err(VenueError::Transport("still nothing".into()))
    }
}

/// Counts what the host claims to know about each submission.
///
/// The log is shared rather than read back through an accessor: adding
/// one to `Trader` would widen the public API for a test's convenience,
/// and nothing outside a test wants it.
struct Listening {
    told: std::rc::Rc<RefCell<Vec<(OrderId, bool)>>>,
    fired: Vec<Intent>,
}

impl Strategy for Listening {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        out.append(&mut self.fired);
    }
    fn on_placed(&mut self, id: OrderId, accepted: bool) {
        self.told.borrow_mut().push((id, accepted));
    }
    fn name(&self) -> &str {
        "listening"
    }
}

type Told = std::rc::Rc<RefCell<Vec<(OrderId, bool)>>>;

fn silent_trader(intents: Vec<Intent>) -> (Trader<Listening, Silent>, Told) {
    let session = Session::start(
        Silent,
        RiskGate::new(Limits {
            max_order_qty: QtyLots(100),
            max_position_qty: QtyLots(1000),
            max_order_notional: Cash(1_000_000 * oq_types::CASH_SCALE),
            price_band: Ratio(500_000_000),
            max_working: 10,
            max_rate: 100,
            rate_window: Nanos(1_000_000_000),
        }),
        SessionConfig {
            symbol: "BTCUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "live".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("clean venue");
    let told: Told = std::rc::Rc::new(RefCell::new(Vec::new()));
    (
        Trader::new(
            Listening {
                told: std::rc::Rc::clone(&told),
                fired: intents,
            },
            session,
        ),
        told,
    )
}

/// An unanswered submission is its own outcome, not a refusal.
///
/// They oblige opposite actions. A refusal is final and the order can
/// be replaced; an unresolved submission may be resting right now, and
/// replacing it is the single move that turns *maybe one order* into
/// *certainly two*. Reporting one as the other is not a labelling
/// problem — it is an instruction to do the wrong thing.
#[test]
fn an_unanswered_submission_is_not_a_refusal() {
    let (mut t, _told) = silent_trader(vec![limit(1)]);
    let outcomes = t.on_tick(&ctx(), Nanos(0));
    assert!(
        matches!(outcomes.as_slice(), [Outcome::Unresolved { local, .. }] if *local == OrderId(1)),
        "expected one Unresolved, got {outcomes:?}"
    );
}

/// And it is not reported to the strategy at all.
///
/// `on_placed`'s contract says so in as many words, but the guard it
/// described could not fire: `Submission::Unresolved` was mapped onto
/// `Outcome::Refused` before `report_placements` ever saw it, so the
/// match arm meant to skip unresolved placements was unreachable and
/// every one of them was reported as `accepted = false` — telling the
/// strategy an order does not exist when it may be resting.
#[test]
fn the_strategy_is_not_told_an_unanswered_order_was_refused() {
    let (mut t, told) = silent_trader(vec![limit(1)]);
    let _ = t.on_tick(&ctx(), Nanos(0));
    assert!(
        told.borrow().is_empty(),
        "the strategy was told {:?} about an order nobody has an answer for",
        told.borrow()
    );
}
