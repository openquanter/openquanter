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
                venue_id: "1".to_string(),
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
        amount_text: String::new(),
        entry_text: String::new(),
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
fn durable_range_is_used_without_changing_the_ownership_prefix() {
    let mut s = session(Recording::accepting(), &[], &[], &[])
        .unwrap_or_else(|e| panic!("{e}"))
        .with_order_id_range(9_000..=9_001)
        .unwrap_or_else(|e| panic!("{e}"));
    s.submit(buy(1), PriceTicks(6_000_000), Nanos(0));
    s.submit(buy(1), PriceTicks(6_000_000), Nanos(1));
    let sent = s.venue().sent.borrow();
    assert_eq!(sent[0].client_id, "live-9000");
    assert_eq!(sent[1].client_id, "live-9001");
    drop(sent);
    assert!(matches!(
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(2)),
        Submission::Rejected(_)
    ));
    assert_eq!(s.venue().sent.borrow().len(), 2);
}

#[test]
fn timed_out_entry_does_not_query_an_old_process_order() {
    struct Historical {
        queried: std::cell::RefCell<Vec<String>>,
    }
    impl Execution for Historical {
        fn place(&self, o: &NewOrder, _: &Instrument) -> Placed {
            Placed::Unknown(oq_gateway::Unresolved {
                client_id: o.client_id.clone(),
                reason: "timeout".into(),
            })
        }
        fn cancel(&self, _: &str, _: &str) -> Placed {
            unreachable!()
        }
        fn order_status(&self, _: &str, id: &str) -> Result<Option<OrderAck>, VenueError> {
            self.queried.borrow_mut().push(id.into());
            Ok((id == "live-1").then(|| OrderAck {
                venue_id: "old".into(),
                client_id: id.into(),
                status: "FILLED".into(),
                executed_qty: "0.006".into(),
            }))
        }
    }
    let mut s = Session::start(
        Historical {
            queried: std::cell::RefCell::new(Vec::new()),
        },
        RiskGate::new(limits()),
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
    .unwrap_or_else(|e| panic!("{e}"))
    .with_order_id_range(10_000..=20_000)
    .unwrap_or_else(|e| panic!("{e}"));
    // Not found under its own id, so still unresolved; what matters here
    // is which id was asked about.
    assert!(matches!(
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)),
        Submission::Unresolved { .. }
    ));
    assert_eq!(*s.venue().queried.borrow(), vec!["live-10000"]);
    assert_eq!(s.book().working(), 0);
}

#[test]
fn declaring_an_invalid_hedge_leg_does_not_make_it_safe_to_adopt() {
    for (side, amount) in [("LONG", -0.002), ("SHORT", 0.002)] {
        let result = session(
            Recording::accepting(),
            &[held("BTCUSDT", side, amount)],
            &[],
            &[Position {
                symbol: "BTCUSDT".into(),
                side: side.into(),
                amount,
            }],
        );
        assert!(matches!(
            result,
            Err(StartupRefusal::InvalidPosition { .. })
        ));
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
/// Not found at once is not never landed.
///
/// The request may still be queued behind the gateway that failed to
/// answer. Reading the venue's not having it yet as a refusal licensed a
/// resend, which is how one order becomes two. It stays unresolved and
/// is asked about again once the request can no longer be accepted.
fn an_unknown_placement_the_venue_does_not_have_yet_stays_unresolved() {
    let venue = Recording::answering(
        Placed::Unknown(oq_gateway::Unresolved {
            client_id: "live-1".into(),
            reason: "timeout".into(),
        }),
        None,
    );
    let mut s = session(venue, &[], &[], &[]).expect("starts");
    match s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)) {
        Submission::Unresolved { client_id, .. } => assert_eq!(client_id, "live-1"),
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
            venue_id: "9".to_string(),
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
    // 0.400 of a contract quoted to three decimal places is 16 lots.
    // The cap here is 10, so an order that would take the account
    // further out must be refused for the position rather than
    // permitted because the gate thought the account was flat.
    let mut s = session(
        Recording::accepting(),
        &[held("BTCUSDT", "BOTH", 0.400)],
        &[],
        &[Position {
            symbol: "BTCUSDT".into(),
            side: "BOTH".into(),
            amount: 0.400,
        }],
    )
    .expect("declared, so it starts");

    assert_eq!(
        s.book().net_lots("BTCUSDT", 3),
        QtyLots(400),
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
        &[held("BTCUSDT", "BOTH", 0.400)],
        &[],
        &[Position {
            symbol: "BTCUSDT".into(),
            side: "BOTH".into(),
            amount: 0.400,
        }],
    )
    .expect("declared");

    match tight.submit(buy(1), PriceTicks(6_000_000), Nanos(0)) {
        Submission::Refused(oq_risk::Breach::PositionWouldExceed { resulting, limit }) => {
            assert_eq!(
                resulting,
                QtyLots(401),
                "400 already held plus the 1 requested"
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

/// On a hedged account, which leg an order names decides whether it is an
/// entry or an exit. The venue refuses `reduceOnly` there, so the leg is
/// the only thing carrying that meaning.
mod hedged_legs {
    use oq_gateway::PositionSide;
    use oq_live::session::leg_for;
    use oq_types::Side;

    #[test]
    fn a_close_names_the_leg_being_closed_not_the_direction_of_the_order() {
        // The defect this replaces: the leg was pinned per session, so a
        // sell-to-close on a long-configured session was sent as an open
        // on the long leg — an exit that increased the position.
        assert_eq!(
            leg_for(PositionSide::Long, Side::Sell, true),
            PositionSide::Long,
            "selling to close closes the long"
        );
        assert_eq!(
            leg_for(PositionSide::Long, Side::Buy, true),
            PositionSide::Short,
            "buying to close closes the short"
        );
    }

    #[test]
    fn an_open_names_the_leg_it_opens() {
        assert_eq!(
            leg_for(PositionSide::Long, Side::Buy, false),
            PositionSide::Long
        );
        assert_eq!(
            leg_for(PositionSide::Long, Side::Sell, false),
            PositionSide::Short
        );
    }

    #[test]
    fn the_configured_leg_does_not_decide_anything_beyond_hedged_or_not() {
        // Configuring Short must give the same answers as configuring
        // Long: the leg comes from the order, not from the session. A
        // session-level leg that still leaked through would make the
        // mapping depend on configuration, which is the bug again in a
        // quieter form.
        for side in [Side::Buy, Side::Sell] {
            for closing in [true, false] {
                assert_eq!(
                    leg_for(PositionSide::Long, side, closing),
                    leg_for(PositionSide::Short, side, closing),
                    "{side:?} closing={closing}"
                );
            }
        }
    }

    #[test]
    fn a_one_way_account_names_no_leg_whatever_the_order_is() {
        for side in [Side::Buy, Side::Sell] {
            for closing in [true, false] {
                assert_eq!(
                    leg_for(PositionSide::OneWay, side, closing),
                    PositionSide::OneWay
                );
            }
        }
    }
}

/// A fill as the account stream reports it.
fn filled(
    client_id: &str,
    trade_id: i64,
    side: &str,
    leg: &str,
    qty: &str,
) -> oq_gateway::OrderUpdate {
    oq_gateway::OrderUpdate {
        symbol: "BTCUSDT".into(),
        client_id: client_id.into(),
        venue_id: "1".into(),
        status: "FILLED".into(),
        last_qty: qty.into(),
        cumulative_qty: qty.into(),
        last_price: "60000".into(),
        side: side.into(),
        position_side: leg.into(),
        maker: true,
        trade_id: Some(trade_id),
        event_ms: 0,
    }
}

fn capped(position_side: PositionSide, cap: i64) -> Session<Recording> {
    Session::start(
        Recording::accepting(),
        RiskGate::new(Limits {
            max_position_qty: QtyLots(cap),
            ..limits()
        }),
        SessionConfig {
            symbol: "BTCUSDT".into(),
            instrument: Instrument::linear(2, 3),
            position_side,
            id_prefix: "live".into(),
        },
        &[],
        &[],
        &[],
    )
    .expect("clean venue")
}

/// The cap follows the fills, not the position at startup.
///
/// A ladder filling rung by rung on a healthy link was checked against
/// the position it started with — the session's own book moved only
/// when the venue's number was adopted — so `max_position_qty` could not
/// fire for as long as the stream stayed up.
#[test]
fn the_position_cap_tightens_as_fills_arrive() {
    let mut s = capped(PositionSide::OneWay, 10);
    assert!(s.submit(buy(8), PriceTicks(6_000_000), Nanos(0)).is_sent());
    s.apply(&filled("live-1", 1, "BUY", "BOTH", "0.008"));
    assert_eq!(s.book().net_lots("BTCUSDT", 3), QtyLots(8));
    match s.submit(buy(3), PriceTicks(6_000_000), Nanos(1)) {
        Submission::Refused(Breach::PositionWouldExceed { resulting, limit }) => {
            assert_eq!((resulting, limit), (QtyLots(11), QtyLots(10)));
        }
        other => panic!("8 held plus 3 is past a cap of 10: {other:?}"),
    }
}

/// A redelivered fill does not move the position twice.
#[test]
fn a_redelivered_fill_moves_the_position_once() {
    let mut s = capped(PositionSide::OneWay, 1000);
    s.apply(&filled("live-1", 1, "BUY", "BOTH", "0.008"));
    s.apply(&filled("live-1", 1, "BUY", "BOTH", "0.008"));
    assert_eq!(s.book().net_lots("BTCUSDT", 3), QtyLots(8));
}

/// Another system's fill on the same symbol is still the account's
/// position, and the cap is on the account.
#[test]
fn another_systems_fill_moves_the_position_the_cap_sees() {
    let mut s = capped(PositionSide::OneWay, 10);
    s.apply(&filled("manual-1", 5, "BUY", "BOTH", "0.009"));
    assert!(matches!(
        s.submit(buy(2), PriceTicks(6_000_000), Nanos(0)),
        Submission::Refused(Breach::PositionWouldExceed { .. })
    ));
}

/// On a hedged account an opening order is capped against its own leg.
///
/// Long 9 and short 9 net to nothing. Capped on the net, both legs could
/// grow in step without bound.
#[test]
fn a_hedged_opening_order_is_capped_against_its_leg_not_the_net() {
    let mut s = capped(PositionSide::Long, 10);
    s.apply(&filled("live-1", 1, "BUY", "LONG", "0.009"));
    s.apply(&filled("live-2", 2, "SELL", "SHORT", "0.009"));
    assert_eq!(s.book().net_lots("BTCUSDT", 3), QtyLots(0));
    assert!(matches!(
        s.submit(buy(2), PriceTicks(6_000_000), Nanos(0)),
        Submission::Refused(Breach::PositionWouldExceed { .. })
    ));
    let sell = ProposedOrder {
        side: Side::Sell,
        ..buy(2)
    };
    assert!(matches!(
        s.submit(sell, PriceTicks(6_000_000), Nanos(1)),
        Submission::Refused(Breach::PositionWouldExceed { .. })
    ));
    // And a leg with room is not refused on the other leg's account.
    assert!(s.submit(buy(1), PriceTicks(6_000_000), Nanos(2)).is_sent());
}

/// An impossible position found by a reconciliation is the caller's to
/// halt on, so the halt withdraws what it should.
///
/// The session used to trip the switch itself. New orders stopped and
/// every resting opening order went on filling, because withdrawing them
/// is the trader's work and a halt taken inside the session never
/// reached it.
#[test]
fn a_reconciliation_that_finds_an_impossible_leg_leaves_the_halt_to_the_caller() {
    let mut s = session(Recording::accepting(), &[], &[], &[]).expect("starts");
    let err = s
        .reconcile(&[held("BTCUSDT", "LONG", -0.5)])
        .expect_err("a long leg cannot be negative");
    assert!(!err.is_empty());
    assert!(
        s.submit(buy(1), PriceTicks(6_000_000), Nanos(0)).is_sent(),
        "the session did not halt on its own; the caller does, and withdraws"
    );
}
