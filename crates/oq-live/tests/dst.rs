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
            && self.ticks % 20 == 0
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
    let code = run_on(
        sim.account(),
        |_| Quoter {
            next: 0,
            resting: None,
            ticks: 0,
        },
        &cfg,
        &env,
    );
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
    };
    for seed in 1..=12 {
        let (sim, code, _) = simulate_with("faults", seed, 10, faults);
        assert_invariants("faults", seed, &sim, code);
    }
}
