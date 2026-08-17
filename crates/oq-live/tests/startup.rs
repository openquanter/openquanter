//! Starting is where a wrong picture is cheapest to catch.
//!
//! Every test here is about one decision: a process must not begin
//! trading beside something it does not know about. Nothing has been
//! sent yet, nothing is resting, and the operator is present — none of
//! which will be true the next time the discrepancy matters.
//!
//! `Session` deliberately does not implement `Debug`, which is why the
//! refusals below are unwrapped through `err()` rather than
//! `expect_err`. It holds a venue client, a venue client holds
//! credentials, and a derived `Debug` on the way to a log line is how
//! those get printed.

use oq_gateway::{
    Execution, NewOrder, OrderAck, Placed, PositionSide, PositionSnapshot, VenueError,
};
use oq_live::{Position, Session, SessionConfig, StartupRefusal, Submission};
use oq_risk::{Breach, Limits, ProposedOrder, RiskGate};
use oq_types::{Cash, Instrument, Nanos, PriceTicks, QtyLots, Ratio, Side};

/// A venue that records what it was asked and answers as told.
struct Recording {
    answer: Placed,
    status: Option<OrderAck>,
    sent: std::cell::RefCell<Vec<NewOrder>>,
}

impl Recording {
    fn answering(answer: Placed, status: Option<OrderAck>) -> Self {
        Self {
            answer,
            status,
            sent: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn accepting() -> Self {
        Self::answering(
            Placed::Accepted(OrderAck {
                venue_id: 1,
                client_id: "live-1".into(),
                status: "NEW".into(),
                executed_qty: "0".into(),
            }),
            None,
        )
    }
}

impl Execution for Recording {
    fn place(&self, order: &NewOrder, _instrument: &Instrument) -> Placed {
        self.sent.borrow_mut().push(order.clone());
        self.answer.clone()
    }
    fn cancel(&self, _symbol: &str, _client_id: &str) -> Placed {
        self.answer.clone()
    }
    fn order_status(
        &self,
        _symbol: &str,
        _client_id: &str,
    ) -> Result<Option<OrderAck>, VenueError> {
        Ok(self.status.clone())
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

fn held(symbol: &str, side: &str, amount: f64) -> PositionSnapshot {
    PositionSnapshot {
        symbol: symbol.into(),
        position_side: side.into(),
        amount,
        entry_price: 0.0,
        unrealized: 0.0,
    }
}

fn session(
    venue: Recording,
    positions: &[PositionSnapshot],
    orders: &[String],
    expected: &[Position],
) -> Result<Session<Recording>, StartupRefusal> {
    Session::start(
        venue,
        RiskGate::new(limits()),
        SessionConfig {
            symbol: "BTCUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "live".into(),
        },
        positions,
        orders,
        expected,
    )
}

fn buy(qty: i64) -> ProposedOrder {
    ProposedOrder {
        side: Side::Buy,
        limit_price: Some(PriceTicks(6_000_000)),
        qty: QtyLots(qty),
        reduce_only: false,
    }
}

#[test]
fn a_position_nobody_declared_stops_the_process() {
    let e = session(
        Recording::accepting(),
        &[held("BTCUSDT", "BOTH", 1.5)],
        &[],
        &[],
    )
    .err()
    .expect("must refuse");
    match e {
        StartupRefusal::UndeclaredPosition { symbol, amount, .. } => {
            assert_eq!(symbol, "BTCUSDT");
            assert!((amount - 1.5).abs() < f64::EPSILON);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_refusal_says_why_it_matters_and_not_only_what_happened() {
    let e = session(
        Recording::accepting(),
        &[held("BTCUSDT", "BOTH", 1.5)],
        &[],
        &[],
    )
    .err()
    .expect("must refuse");
    let text = e.to_string();
    assert!(
        text.contains("size every order against a picture"),
        "{text}"
    );
}

#[test]
fn a_declared_position_is_adopted_and_the_session_starts() {
    let s = session(
        Recording::accepting(),
        &[held("BTCUSDT", "BOTH", 1.5)],
        &[],
        &[Position {
            symbol: "BTCUSDT".into(),
            side: "BOTH".into(),
            amount: 1.5,
        }],
    )
    .expect("declared, so it starts");
    assert!((s.book().net("BTCUSDT") - 1.5).abs() < f64::EPSILON);
}

#[test]
fn a_closed_leg_reading_as_zero_does_not_block_a_restart() {
    // Venues report a closed leg as zero rather than as an absence.
    // Refusing over one would make every restart a manual step, which
    // trains an operator to pass whatever flag disables the check.
    let s = session(
        Recording::accepting(),
        &[held("BTCUSDT", "LONG", 0.0)],
        &[],
        &[],
    )
    .expect("zero is not a position");
    assert_eq!(s.book().positions().len(), 0);
}

#[test]
fn a_resting_order_nobody_placed_stops_the_process() {
    let e = session(Recording::accepting(), &[], &["someone-elses".into()], &[])
        .err()
        .expect("must refuse");
    assert!(matches!(e, StartupRefusal::UndeclaredOrder { .. }));
}

#[test]
fn every_order_goes_through_the_gate() {
    // The property this crate exists for: no route to the venue skips
    // the check.
    let mut s = session(Recording::accepting(), &[], &[], &[]).expect("starts");
    s.gate().kill_switch().trip();
    assert_eq!(
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)),
        Submission::Refused(Breach::Halted)
    );
    assert!(
        s.venue().sent.borrow().is_empty(),
        "a refused order must not reach the venue at all"
    );
}

#[test]
fn what_the_gate_approved_is_what_gets_sent() {
    // A check that validates one order while another goes out is the
    // failure the permit exists to prevent.
    let mut s = session(Recording::accepting(), &[], &[], &[]).expect("starts");
    let mut o = buy(3);
    o.side = Side::Sell;
    o.limit_price = Some(PriceTicks(6_100_000));
    assert!(matches!(
        s.submit(o, PriceTicks(6_000_000), Nanos(0)),
        Submission::Sent(_)
    ));
    let sent = s.venue().sent.borrow();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].side, Side::Sell);
    assert_eq!(sent[0].qty, QtyLots(3));
    assert_eq!(sent[0].limit_price, Some(PriceTicks(6_100_000)));
}

#[test]
fn an_unknown_placement_the_venue_never_saw_may_be_sent_again() {
    let venue = Recording::answering(
        Placed::Unknown(oq_gateway::Unresolved {
            client_id: "live-1".into(),
            reason: "timeout".into(),
        }),
        None,
    );
    let mut s = session(venue, &[], &[], &[]).expect("starts");
    match s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)) {
        Submission::Rejected(why) => assert!(why.contains("never reached"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unknown_placement_the_venue_does_know_about_counts_as_sent() {
    // Resolved by the id chosen before sending, which is the entire
    // reason that id exists.
    let venue = Recording::answering(
        Placed::Unknown(oq_gateway::Unresolved {
            client_id: "live-1".into(),
            reason: "timeout".into(),
        }),
        Some(OrderAck {
            venue_id: 9,
            client_id: "live-1".into(),
            status: "NEW".into(),
            executed_qty: "0".into(),
        }),
    );
    let mut s = session(venue, &[], &[], &[]).expect("starts");
    assert_eq!(
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)),
        Submission::Sent("live-1".into())
    );
}

#[test]
fn client_ids_do_not_repeat_within_a_run() {
    // A repeated id makes the venue refuse the second order, and the
    // refusal is about a duplicate rather than about anything the
    // strategy did.
    let mut s = session(Recording::accepting(), &[], &[], &[]).expect("starts");
    for i in 0..3 {
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(i));
    }
    let sent = s.venue().sent.borrow();
    let ids: std::collections::HashSet<_> = sent.iter().map(|o| o.client_id.clone()).collect();
    assert_eq!(ids.len(), sent.len(), "every order got its own id");
}

#[test]
fn the_gate_is_shown_the_position_the_venue_confirmed() {
    // A position cap compared against a hardcoded zero can never fire,
    // which makes it decoration. The number the venue reported has to
    // reach the check.
    //
    // 0.016 of a contract quoted to three decimal places is 16 lots.
    // The cap here is 10, so an order that would take the account
    // further out must be refused for the position rather than
    // permitted because the gate thought the account was flat.
    let mut s = session(
        Recording::accepting(),
        &[held("BTCUSDT", "BOTH", 0.016)],
        &[],
        &[Position {
            symbol: "BTCUSDT".into(),
            side: "BOTH".into(),
            amount: 0.016,
        }],
    )
    .expect("declared, so it starts");

    assert_eq!(
        s.book().net_lots("BTCUSDT", 3),
        QtyLots(16),
        "decimal amount to lots"
    );

    // max_position_qty is 1000 in these limits, so tighten it by using
    // a fresh gate through a second session with a smaller cap.
    let mut tight = Session::start(
        Recording::accepting(),
        RiskGate::new(Limits {
            max_position_qty: QtyLots(10),
            ..limits()
        }),
        SessionConfig {
            symbol: "BTCUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side: PositionSide::OneWay,
            id_prefix: "live".into(),
        },
        &[held("BTCUSDT", "BOTH", 0.016)],
        &[],
        &[Position {
            symbol: "BTCUSDT".into(),
            side: "BOTH".into(),
            amount: 0.016,
        }],
    )
    .expect("declared");

    match tight.submit(buy(1), PriceTicks(6_000_000), Nanos(0)) {
        Submission::Refused(oq_risk::Breach::PositionWouldExceed { resulting, limit }) => {
            assert_eq!(
                resulting,
                QtyLots(17),
                "16 already held plus the 1 requested"
            );
            assert_eq!(limit, QtyLots(10));
        }
        other => panic!("the cap must see the real position: {other:?}"),
    }

    // And the same order is permitted when the cap has room, so the
    // check is reading the number rather than refusing everything.
    assert!(s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)).is_sent());
}

#[test]
fn a_flat_account_still_reports_zero_lots() {
    let s = session(Recording::accepting(), &[], &[], &[]).expect("starts");
    assert_eq!(s.book().net_lots("BTCUSDT", 3), QtyLots(0));
}
