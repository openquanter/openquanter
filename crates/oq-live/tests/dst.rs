//! The whole live process, run against a simulated venue on a clock that
//! moves by hand.
//!
//! Everything below `run_on` is the production code: startup checks, the
//! interlock, the journal, the risk gate, the strategy, the market and
//! account streams, shutdown. Only the world is simulated. A seed names a
//! run exactly, which is what makes a failure found here a failure that
//! can be replayed rather than a flake that can be retried.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use oq_l2feed::venue::Deployment;
use oq_live::run::{RunConfig, run_on};
use oq_live::sim::{Faults, Sim, SimConfig, SimEnv};
use oq_risk::Limits;
use oq_strategy::{Context, Intent, Strategy};
use oq_types::{Cash, Nanos, OrderId, PriceTicks, QtyLots, Ratio, Side};

/// Keeps one bid a few ticks under the market, and once filled one offer
/// a few ticks over it. Enough to rest, fill, cancel and close.
struct Quoter {
    next: u64,
    resting: Option<OrderId>,
    ticks: u64,
}

impl Strategy for Quoter {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.ticks += 1;
        let last = ctx.tick.last.0;
        if last == 0 {
            return;
        }
        // Re-quote every twenty observations, whatever happened.
        if let Some(id) = self.resting
            && self.ticks.is_multiple_of(20)
        {
            out.push(Intent::Cancel(id));
            self.resting = None;
            return;
        }
        if self.resting.is_some() || ctx.working > 0 {
            return;
        }
        self.next += 1;
        let id = OrderId::new(self.next);
        let (side, price) = if ctx.position.0 > 0 {
            (Side::Sell, last + 3)
        } else {
            (Side::Buy, last - 3)
        };
        let mut intent = ctx.limit(id, side, PriceTicks(price), QtyLots(2));
        if side == Side::Sell
            && let Intent::Limit { offset, .. } = &mut intent
        {
            *offset = oq_types::Offset::Close;
        }
        out.push(intent);
        self.resting = Some(id);
    }

    fn on_ended(&mut self, id: OrderId, _ending: oq_strategy::Ending, _out: &mut Vec<Intent>) {
        if self.resting == Some(id) {
            self.resting = None;
        }
    }

    fn name(&self) -> &str {
        "quoter"
    }
}

/// Re-quotes a bid under the market every twenty observations, whatever
/// it believes it has working, so only a halt can stop it from sending.
struct Insistent {
    next: u64,
    last: Option<OrderId>,
    ticks: u64,
}

impl Strategy for Insistent {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.ticks += 1;
        if ctx.tick.last.0 == 0 || !self.ticks.is_multiple_of(20) {
            return;
        }
        if let Some(id) = self.last.take() {
            out.push(Intent::Cancel(id));
        }
        self.next += 1;
        let id = OrderId::new(self.next);
        // Closes what it holds, opens when flat: the position stays
        // inside the limit, so only a halt stops the sending.
        let last = ctx.tick.last.0;
        let intent = if ctx.position.0 > 0 {
            match ctx.limit(id, Side::Sell, PriceTicks(last + 3), QtyLots(2)) {
                Intent::Limit {
                    instrument,
                    id,
                    side,
                    price,
                    qty,
                    ..
                } => Intent::Limit {
                    instrument,
                    id,
                    side,
                    price,
                    qty,
                    offset: oq_types::Offset::Close,
                },
                other => other,
            }
        } else {
            ctx.limit(id, Side::Buy, PriceTicks(last - 3), QtyLots(2))
        };
        out.push(intent);
        self.last = Some(id);
    }

    fn name(&self) -> &str {
        "insistent"
    }
}

/// Rests an opening bid far under the market and never withdraws it,
/// then offers another every ten seconds. Whatever takes its orders off
/// the venue before the run ends, it was not this.
struct Patient {
    next: u64,
    ticks: u64,
}

impl Strategy for Patient {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.ticks += 1;
        let last = ctx.tick.last.0;
        if last == 0 || (self.next > 0 && !self.ticks.is_multiple_of(100)) {
            return;
        }
        self.next += 1;
        out.push(ctx.limit(
            OrderId::new(self.next),
            Side::Buy,
            PriceTicks(last - 1_000),
            QtyLots(1),
        ));
    }

    fn name(&self) -> &str {
        "patient"
    }
}

/// Holds a long and a short at once, the way a hedged ladder does: every
/// twenty observations it withdraws everything and quotes an entry for
/// each flat leg and an exit for each held one.
struct Both {
    next: u64,
    ticks: u64,
    both_held: bool,
}

impl Both {
    fn order(&mut self, ctx: &Context, side: Side, price: i64, offset: oq_types::Offset) -> Intent {
        self.next += 1;
        match ctx.limit(OrderId::new(self.next), side, PriceTicks(price), QtyLots(2)) {
            Intent::Limit {
                instrument,
                id,
                side,
                price,
                qty,
                ..
            } => Intent::Limit {
                instrument,
                id,
                side,
                price,
                qty,
                offset,
            },
            other => other,
        }
    }
}

impl Strategy for Both {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.ticks += 1;
        let last = ctx.tick.last.0;
        if last == 0 || !self.ticks.is_multiple_of(20) {
            return;
        }
        self.both_held |= ctx.position.0 > 0 && ctx.short_position.0 < 0;
        out.push(Intent::CancelAll);
        use oq_types::Offset::{Close, Open};
        let long = if ctx.position.0 > 0 {
            self.order(ctx, Side::Sell, last + 3, Close)
        } else {
            self.order(ctx, Side::Buy, last - 3, Open)
        };
        let short = if ctx.short_position.0 < 0 {
            self.order(ctx, Side::Buy, last - 3, Close)
        } else {
            self.order(ctx, Side::Sell, last + 3, Open)
        };
        out.push(long);
        out.push(short);
    }

    fn waiting_on(&self) -> Vec<(&'static str, i64)> {
        vec![("both_held", i64::from(self.both_held))]
    }

    fn name(&self) -> &str {
        "both"
    }
}

fn dir(tag: &str, seed: u64) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oq-dst-{tag}-{seed}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

/// One simulated run of `minutes`, returning the simulation, the exit
/// code and the journal's bytes.
fn simulate(tag: &str, seed: u64, minutes: i64) -> (Sim, ExitCode, Vec<u8>) {
    simulate_with(tag, seed, minutes, Faults::default())
}

fn simulate_with(tag: &str, seed: u64, minutes: i64, faults: Faults) -> (Sim, ExitCode, Vec<u8>) {
    simulate_strategy(tag, seed, minutes, faults, false)
}

fn simulate_strategy(
    tag: &str,
    seed: u64,
    minutes: i64,
    faults: Faults,
    insistent: bool,
) -> (Sim, ExitCode, Vec<u8>) {
    run_kind(
        tag,
        seed,
        minutes,
        faults,
        if insistent {
            Kind::Insistent
        } else {
            Kind::Quoter
        },
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Quoter,
    Insistent,
    Hedged,
    Patient,
}

fn run_kind(
    tag: &str,
    seed: u64,
    minutes: i64,
    faults: Faults,
    kind: Kind,
) -> (Sim, ExitCode, Vec<u8>) {
    let hedged = kind == Kind::Hedged;
    let root = dir(tag, seed);
    let sim = Sim::new(SimConfig {
        seed,
        symbol: "BTCUSDT".into(),
        start_price: 8_000_000,
        max_step: 4,
        market_every: Duration::from_millis(100),
        idle_step: Duration::from_millis(50),
        balance: 10_000.0,
        state_root: root.clone(),
        faults,
        hedged,
    });
    let journal = root.join("run.oqj");
    let cfg = RunConfig {
        broker_code: None,
        symbol: "BTCUSDT".into(),
        strategy_name: "quoter".into(),
        deployment: Deployment::Testnet,
        minutes,
        warm_minutes: 0,
        window_ms: 1000,
        // Unique per run: the interlock refuses two processes on one
        // prefix, and tests run in parallel.
        id_prefix: format!("d{tag}{seed}"),
        adopt_existing: false,
        journal: Some(journal.display().to_string()),
        limits: Limits {
            max_order_qty: QtyLots(10),
            max_position_qty: QtyLots(20),
            max_order_notional: Cash(1_000_000 * oq_types::CASH_SCALE),
            price_band: Ratio(500_000_000),
            max_working: 5,
            max_rate: 50,
            rate_window: Nanos(1_000_000_000),
        },
    };
    let env = SimEnv::new(&sim);
    let code = match kind {
        Kind::Insistent => run_on(
            sim.account(),
            |_| Insistent {
                next: 0,
                last: None,
                ticks: 0,
            },
            &cfg,
            &env,
        ),
        Kind::Quoter => run_on(
            sim.account(),
            |_| Quoter {
                next: 0,
                resting: None,
                ticks: 0,
            },
            &cfg,
            &env,
        ),
        Kind::Patient => run_on(sim.account(), |_| Patient { next: 0, ticks: 0 }, &cfg, &env),
        Kind::Hedged => run_on(
            sim.account(),
            |_| Both {
                next: 0,
                ticks: 0,
                both_held: false,
            },
            &cfg,
            &env,
        ),
    };
    let bytes = std::fs::read(&journal).expect("a journal was written");
    (sim, code, bytes)
}

#[test]
fn a_clean_run_trades_and_ends_with_nothing_resting() {
    let (sim, code, journal) = simulate("clean", 7, 20);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        sim.elapsed() >= Duration::from_secs(20 * 60),
        "{:?}",
        sim.elapsed()
    );
    assert!(
        sim.placed().len() > 5,
        "it traded: {:?}",
        sim.placed().len()
    );
    assert!(
        sim.resting().is_empty(),
        "shutdown left {:?}",
        sim.resting()
    );
    let mut ids = sim.placed();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), sim.placed().len(), "a client id was sent twice");
    assert!(!journal.is_empty());
}

/// The property everything else rests on: a seed names one run.
#[test]
fn the_same_seed_writes_the_same_journal() {
    // One tag for both: they run one after the other, and a different
    // prefix would be a different run.
    let (_, _, a) = simulate("same", 11, 10);
    let (_, _, b) = simulate("same", 11, 10);
    assert_eq!(a.len(), b.len());
    assert!(a == b, "two runs of one seed wrote different journals");
}

/// And a different seed a different one, or the test above would pass
/// for a run that did nothing.
#[test]
fn a_different_seed_writes_a_different_journal() {
    let (_, _, a) = simulate("seed-a", 21, 5);
    let (_, _, b) = simulate("seed-b", 22, 5);
    assert!(a != b);
}

/// What the journal says the process held is what the venue holds: the
/// journal replays, and it replays to the truth.
#[test]
fn the_journal_rebuilds_the_position_the_venue_holds() {
    let seed = 31;
    let (sim, code, _) = simulate("belief", seed, 15);
    assert_eq!(code, ExitCode::SUCCESS);
    let path = dir_path("belief", seed).join("run.oqj");
    let belief = oq_live::belief::Belief::from_journal(&path).expect("the journal reads");
    assert_eq!(belief.undecodable, 0, "every frame decoded");
    assert_eq!(belief.position_lots, sim.position(), "{belief:?}");
    assert!(belief.resting.is_empty(), "{:?}", belief.resting);
}

fn dir_path(tag: &str, seed: u64) -> PathBuf {
    std::env::temp_dir().join(format!("oq-dst-{tag}-{seed}-{}", std::process::id()))
}

/// The properties a run must keep however the venue misbehaves.
fn assert_invariants(tag: &str, seed: u64, sim: &Sim, code: ExitCode) {
    let path = dir_path(tag, seed).join("run.oqj");
    let belief = oq_live::belief::Belief::from_journal(&path).expect("the journal reads");
    let context = format!("seed {seed}: {belief:?}");
    assert_eq!(code, ExitCode::SUCCESS, "{context}");
    assert_eq!(belief.undecodable, 0, "{context}");
    assert_eq!(
        belief.position_lots,
        sim.position(),
        "the journal and the venue disagree about the position; {context}"
    );
    assert!(
        sim.position().abs() <= 20,
        "past the position limit; {context}"
    );
    assert!(
        sim.resting().is_empty(),
        "shutdown left {:?}; {context}",
        sim.resting()
    );
    let mut ids = sim.placed();
    ids.sort();
    let sent = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), sent, "a client id was sent twice; {context}");
}

/// A venue that drops the account stream, repeats itself, loses answers,
/// refuses for rate and fills in pieces — and a run that stays true
/// through all of it.
#[test]
fn a_misbehaving_venue_leaves_the_journal_true() {
    let faults = Faults {
        stream_drop: 300,
        duplicate: 20_000,
        unknown_placement: 20_000,
        rate_limited: 20_000,
        partial_fill: 200_000,
        silent_fill_after: None,
        journal_fails_after: None,
    };
    for seed in 1..=12 {
        let (sim, code, _) = simulate_with("faults", seed, 10, faults);
        assert_invariants("faults", seed, &sim, code);
    }
}

/// An account that moves with nothing to explain it is state the process
/// does not know, and a process that does not know its state must stop
/// sending orders. The venue fills an order silently five minutes in —
/// no report, and nothing when asked — and from some point after it the
/// venue must receive nothing new.
#[test]
fn an_account_that_moves_unexplained_stops_the_trading() {
    let faults = Faults {
        silent_fill_after: Some(Duration::from_secs(5 * 60)),
        ..Faults::default()
    };
    let (sim, _code, _) = simulate_strategy("silent", 41, 40, faults, true);
    let silent = sim.silent_at().expect("the silent fill happened");
    let last = *sim.placed_at().last().expect("it traded");
    eprintln!("silent fill at {silent:?}, last order sent at {last:?}");
    assert!(
        last <= silent + Duration::from_secs(20 * 60),
        "still sending orders {:?} after the account moved unexplained",
        last - silent
    );
    assert!(
        sim.resting().is_empty(),
        "left resting: {:?}",
        sim.resting()
    );
}

/// Hedge mode: both legs held at once, through every fault, and the
/// journal still agrees with the venue.
#[test]
fn a_hedged_account_stays_true_through_the_faults() {
    let faults = Faults {
        stream_drop: 300,
        duplicate: 20_000,
        unknown_placement: 20_000,
        rate_limited: 20_000,
        partial_fill: 200_000,
        silent_fill_after: None,
        journal_fails_after: None,
    };
    let mut both = 0;
    for seed in 1..=6 {
        let (sim, code, _) = run_kind("hedged", seed, 15, faults, Kind::Hedged);
        assert_invariants("hedged", seed, &sim, code);
        if sim.legs().len() == 2 || sim.max_legs_held() == 2 {
            both += 1;
        }
    }
    assert!(
        both > 0,
        "no run held both legs at once, so hedge mode went untested"
    );
}

/// With several orders resting at once — a cancel-all every twenty
/// observations and a shutdown sweep — a seed still names one run. The
/// order ids were held in a hash map and withdrawn in whatever order it
/// iterated in, which differed between two runs of one seed.
#[test]
fn a_seed_names_one_run_with_many_orders_resting() {
    let (_, _, a) = run_kind("many", 13, 10, Faults::default(), Kind::Hedged);
    let (_, _, b) = run_kind("many", 13, 10, Faults::default(), Kind::Hedged);
    assert!(a == b, "two runs of one seed wrote different journals");
}

/// A journal that stops taking records stops the orders with it.
///
/// Recording before sending is what makes a crash recoverable: the client
/// id on disk is the handle a restart asks the venue with. So once the
/// disk fills, nothing may reach the venue that the journal does not
/// name, and the run halts — withdrawing the opening order resting from
/// before, which the strategy here never would, long before the end of
/// the run would have.
#[test]
fn a_journal_that_fills_up_stops_the_orders_and_halts_the_run() {
    let faults = Faults {
        journal_fails_after: Some(40),
        ..Faults::default()
    };
    let (sim, _code, _) = run_kind("fullj", 3, 3, faults, Kind::Patient);

    let journal = dir_path("fullj", 3).join("run.oqj");
    let replay = oq_journal::Reader::open(&journal)
        .expect("open")
        .replay()
        .expect("what was written reads");
    assert_eq!(
        replay.next_seq, 40,
        "the journal took exactly its 40 records"
    );
    let recorded: Vec<String> = replay
        .since(0)
        .filter_map(|f| oq_live::record::Record::decode(f.kind, &f.payload))
        .filter_map(|r| match r {
            oq_live::record::Record::Submitted { client_id, .. } => Some(client_id),
            _ => None,
        })
        .collect();

    let placed = sim.placed();
    assert!(
        !placed.is_empty(),
        "the first bid went out while the journal worked"
    );
    for id in &placed {
        assert!(
            recorded.contains(id),
            "{id} reached the venue and the journal cannot name it"
        );
    }

    let withdrawn = sim.withdrawn_at();
    let first = &placed[0];
    let at = withdrawn
        .iter()
        .find(|(id, _)| id == first)
        .map(|(_, at)| *at)
        .expect("the resting bid was withdrawn");
    // Ticks close once a second, so the fortieth record lands about forty
    // seconds in; the run ends at three minutes.
    assert!(
        at < Duration::from_secs(90),
        "withdrawn at {at:?}: by the end-of-run shutdown, not by a halt"
    );
    assert!(
        sim.resting().is_empty(),
        "left resting: {:?}",
        sim.resting()
    );
}
