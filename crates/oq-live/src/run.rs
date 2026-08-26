//! Running a strategy against a venue: the assembly, without the CLI.
//!
//! This is the whole of what `oq-trade` used to do inside `main`. It
//! moved here for one reason: an overlay repository with its own
//! strategy needs the same wiring, and the alternative was copying it.
//!
//! Two implementations that are supposed to agree, with nothing forcing
//! them to, is the shape this project exists to argue against — the
//! predecessor had exactly that and paid for it with a matching defect
//! fixed twice, once in each engine. Copying eight hundred lines of
//! assembly into a private repository would reproduce it, and every
//! live fix afterwards would land in one copy and not the other.
//!
//! What stayed in the binary: argument parsing, the two teaching
//! strategies, and the usage text. What came here: everything from the
//! credentials to the final summary.

use core::time::Duration;
use std::process::ExitCode;
use std::time::Instant;

use oq_gateway::account::Account;
use oq_gateway::exec::Execution;
use oq_gateway::{StreamOutcome, UserEvent, UserStreamReader};
use oq_ingest::Aggregator;
use oq_l2feed::session::{install_signal_handlers, now_ns, shutdown_requested};
use oq_l2feed::venue::Deployment;
use oq_risk::{Limits, RiskGate};
use oq_strategy::{Context, Ending, Strategy};
use oq_types::{Cash, Instrument, Nanos, OrderId, PriceTicks, QtyLots, Side};

use crate::{
    Action, MarketData, Outcome, Position, Session, SessionConfig, Supervisor, Timings, Trader,
};

/// Everything the run needs that the command line used to supply.
///
/// Deliberately plain data: a caller that is not a command line — an
/// overlay binary with its own strategy — should not have to synthesise
/// arguments to get here.
pub struct RunConfig {
    pub symbol: String,
    /// Printed in the banner. The run does not otherwise use it.
    pub strategy_name: String,
    pub deployment: Deployment,
    /// How long to trade for, in minutes.
    ///
    /// **Zero means no deadline**: the run then ends only when a signal
    /// arrives, which is what a process under a supervisor wants. A
    /// negative value is refused rather than interpreted.
    pub minutes: i64,
    /// Minutes of history to warm the strategy with before it trades.
    ///
    /// Zero starts cold, which for an indicator with a window means it
    /// cannot act until that many minutes of live data have arrived —
    /// on a restart, that is time an account spends unmanaged.
    pub warm_minutes: usize,
    pub window_ms: i64,
    pub id_prefix: String,
    pub adopt_existing: bool,
    /// `None` means run without one, which is `--no-journal`.
    pub journal: Option<String>,
    pub limits: Limits,
    /// A venue-issued broker or referral code, when the operator has one.
    ///
    /// Separate from `id_prefix`, which answers *is this order mine*.
    /// This answers *who gets paid for this flow* — see
    /// `oq_gateway::broker` for why conflating them eventually forces a
    /// fork.
    pub broker_code: Option<String>,
}

/// Side, quantity and offset of the intent an outcome answers.
///
/// `Outcome::Sent` carries two ids and nothing else, which is enough to
/// map between the strategy and the venue and not enough to tell the
/// kernel what was sent. `None` for a cancel, which submits nothing.
fn shape_of(
    intents: &[oq_strategy::Intent],
    id: oq_types::OrderId,
) -> Option<(oq_types::Side, oq_types::QtyLots, oq_types::Offset)> {
    intents.iter().find_map(|i| match i {
        oq_strategy::Intent::Limit {
            id: this,
            side,
            qty,
            offset,
            ..
        }
        | oq_strategy::Intent::Market {
            id: this,
            side,
            qty,
            offset,
            ..
        } if *this == id => Some((*side, *qty, *offset)),
        _ => None,
    })
}

/// The name this process was invoked as.
///
/// `run` is called by more than one binary — `oq-trade` and the
/// `grid_live` example, and whatever a reader writes next — so a
/// hard-coded name puts a *different* program's name in front of every
/// diagnostic, which is a bad thing to be reading at the moment
/// something has gone wrong on a venue.
///
/// It also names the journal. Two programs defaulting to the same
/// journal file is worse than cosmetic: the second run's record lands
/// in a file describing the first one's.
fn program() -> String {
    std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        // Argv can be empty and a stem can be non-UTF-8. Neither is
        // worth failing a run over.
        .unwrap_or_else(|| "oq-live".to_string())
}

/// When this run ends, or `None` for one that ends only when it is told
/// to.
///
/// Pure and separate so the arithmetic can be tested without a venue.
///
/// **Zero is the absence of a deadline, not one in the past.** It had no
/// other meaning worth keeping: it named an instant already gone, so the
/// loop exited before its first iteration and the run traded nothing.
/// The loop has always carried the other half of this condition — it
/// stops on a signal — and what was missing was a way to ask for that
/// half alone. Without one, a process meant to run until it is stopped
/// has to name a deadline far enough away to be irrelevant, which is a
/// number chosen to be wrong later rather than now.
///
/// A negative count is refused rather than interpreted. It used to
/// become five minutes, which is neither what was typed nor anything a
/// caller could have meant, and a process about to send orders is the
/// wrong place to resolve a typo quietly.
///
/// A count past what the clock can name is refused for the same reason.
/// The expression this replaces multiplied and added without checking,
/// so such a value ended the process with a panic during startup instead
/// of a message.
fn deadline_from(minutes: i64, now: Instant) -> Result<Option<Instant>, String> {
    match minutes {
        0 => Ok(None),
        m if m < 0 => Err(format!(
            "--minutes {m} is negative; a length of time cannot be one, and 0 \
             is how a run says it should end only on a signal"
        )),
        m => u64::try_from(m)
            .ok()
            .and_then(|minutes| minutes.checked_mul(60))
            .and_then(|secs| now.checked_add(Duration::from_secs(secs)))
            .map(Some)
            .ok_or_else(|| format!("--minutes {m} is further ahead than this clock can name")),
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::deadline_from;
    use std::time::{Duration, Instant};

    /// The case this function exists for. A supervised run has no
    /// useful deadline to name, and every number it could name is one
    /// it will eventually reach for no reason.
    #[test]
    fn zero_is_no_deadline_rather_than_one_already_past() {
        assert_eq!(deadline_from(0, Instant::now()), Ok(None));
    }

    #[test]
    fn a_count_of_minutes_lands_that_many_minutes_ahead() {
        let now = Instant::now();
        assert_eq!(
            deadline_from(90, now),
            Ok(Some(now + Duration::from_secs(90 * 60)))
        );
    }

    /// Refused, and specifically not turned into five minutes: a run
    /// that cannot be honoured as asked is worth a message rather than
    /// a substitute nobody chose.
    #[test]
    fn a_negative_count_is_refused() {
        assert!(deadline_from(-1, Instant::now()).is_err());
    }

    /// This is the input that used to panic during startup.
    #[test]
    fn a_count_past_the_clock_is_refused_and_does_not_panic() {
        assert!(deadline_from(i64::MAX, Instant::now()).is_err());
    }
}

/// Run one strategy against one venue until a deadline or a signal
/// ends it.
///
/// The strategy is built by a closure rather than passed in, because the
/// instrument is discovered here — precision and grid come from the
/// deployment being traded, and a strategy that needs them cannot be
/// constructed before this function has asked.
pub fn run<S, F>(mut venue: Box<dyn Account>, make_strategy: F, cfg: &RunConfig) -> ExitCode
where
    S: Strategy,
    F: FnOnce(&Instrument) -> S,
{
    // Bound locally so the body below is the code that was in `main`,
    // unchanged. Rewriting every use to `cfg.x` would have edited eight
    // hundred lines to move them, and a move that edits is not a move.
    let symbol = cfg.symbol.clone();
    let deployment = cfg.deployment;
    let minutes = cfg.minutes;
    let window_ns = cfg.window_ms * 1_000_000;

    println!("deployment       {deployment:?}");
    println!("symbol           {symbol}");
    println!("strategy         {}", cfg.strategy_name);

    if let Err(e) = venue.sync_clock() {
        eprintln!("clock            FAILED: {e}");
        return ExitCode::FAILURE;
    }

    // Market data first: it decides the precision and grid that the
    // order path has to respect, and connecting it before anything is
    // sent means a feed that will not open stops the run before it
    // trades rather than after.
    let (mut market, feed_venue) =
        match MarketData::open(venue.id(), deployment, &symbol, Duration::from_millis(200)) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("market data      FAILED: {e}");
                return ExitCode::FAILURE;
            }
        };
    // Precision *and* grid come from the deployment being traded, not
    // from the table compiled in. The tables exist so a replay gives
    // the same answer on any machine on any day; the question here is
    // the opposite one — what does this venue accept right now — and
    // the deployments disagree: one of them publishes four decimal
    // places of quantity where the other publishes three.
    let instrument = match venue.instrument(&symbol) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("instrument       FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "instrument       price {} dp (tick {}), qty {} dp (step {})",
        instrument.price_scale, instrument.price_tick, instrument.qty_scale, instrument.qty_step
    );

    let hedged = match venue.is_hedged() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("position mode    FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "position mode    {}",
        if hedged { "hedged" } else { "one-way" }
    );

    let positions = match venue.positions(&symbol) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("positions        FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The venue's own number. Books opened at a configured balance would
    // report an equity curve about a different account.
    let starting_balance = match venue.balances() {
        Ok(a) => {
            println!(
                "balance          {:.2} (wallet, from the venue)",
                a.wallet_balance
            );
            #[allow(clippy::cast_possible_truncation)]
            Cash((a.wallet_balance * oq_types::CASH_SCALE as f64) as i64)
        }
        Err(e) => {
            eprintln!("balance          FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    let resting: Vec<String> = match venue.open_orders(&symbol) {
        Ok(o) => o.into_iter().map(|o| o.client_order_id).collect(),
        Err(e) => {
            eprintln!("open orders      FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The caller's limits, all of them.
    //
    // This rebuilt the struct for a while, copying four fields across
    // and writing two of them itself — a refactor carried the binary's
    // old literals along and wired only the four that had names in the
    // configuration. A caller that derived `max_working` from its ladder
    // got the number 4 instead, and its ninth resting order was refused
    // by a bound nothing had asked for and no message named.
    //
    // A host that edits the limits it was handed is not enforcing a
    // policy, it is having one.
    let limits = cfg.limits;

    // Stable across restarts unless overridden, so a recovered run
    // recognises its own previous orders on the account stream. A prefix
    // derived from the process id would change on every restart and make
    // every prior order look like another system's.
    // The prefix orders actually carry. A broker code goes in front of
    // the ownership prefix, and the ownership check has to match the
    // whole thing — the venue echoes the composed id back, and a check
    // matching only the owner segment would stop recognising this
    // process's own orders. `IdScheme::owned_prefix` is that composition
    // in one place rather than two.
    let scheme = match &cfg.broker_code {
        None => oq_gateway::broker::IdScheme::new(cfg.id_prefix.clone(), venue.id_rules()),
        Some(code) => match oq_gateway::broker::BrokerCode::new(code.clone()) {
            Ok(c) => oq_gateway::broker::IdScheme::new(cfg.id_prefix.clone(), venue.id_rules())
                .map(|s| s.with_broker(c)),
            Err(e) => {
                eprintln!("broker code      REFUSED: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let scheme = match scheme {
        Ok(s) => s,
        Err(e) => {
            eprintln!("id prefix        REFUSED: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.broker_code.is_some() {
        println!(
            "broker code      flow attributed; ids begin {}",
            scheme.owned_prefix()
        );
    }
    let id_prefix = scheme.owned_prefix();
    let config_prefix = id_prefix.clone();

    // Claimed at the earliest point the key is known, and before startup
    // reconciliation rather than after it.
    //
    // After would be too late in a way that hides itself: a second
    // process shares this prefix, so `owns` says the first one's resting
    // orders are *its* orders, and reconciliation would read them as
    // leftovers of a previous run of itself and set about managing them.
    // The refusal has to happen before anything looks at the account.
    //
    // Held for the rest of `run`. The binding matters — `let _ = ...`
    // drops it immediately and releases the lock on the line that takes
    // it.
    let interlock =
        match crate::interlock::Interlock::claim(&format!("{deployment:?}"), &symbol, &id_prefix) {
            Ok(held) => held,
            Err(taken) => {
                eprintln!("interlock        REFUSED: {taken}");
                return ExitCode::FAILURE;
            }
        };
    println!("interlock        held ({})", interlock.path().display());

    let config = SessionConfig {
        symbol: symbol.clone(),
        instrument,
        position_side: if hedged {
            oq_gateway::PositionSide::Long
        } else {
            oq_gateway::PositionSide::OneWay
        },
        id_prefix: id_prefix.clone(),
    };

    // Nothing is declared unless the operator says otherwise, so any
    // position at all stops the run. `--adopt-existing` is that saying:
    // it declares what the venue holds, and the gate is then shown that
    // position rather than a zero.
    let adopt = cfg.adopt_existing;
    let expected: Vec<Position> = if adopt {
        positions
            .iter()
            .filter(|p| p.amount != 0.0)
            .map(|p| Position {
                symbol: p.symbol.clone(),
                side: p.position_side.clone(),
                amount: p.amount,
            })
            .collect()
    } else {
        Vec::new()
    };
    if adopt && !expected.is_empty() {
        for p in &expected {
            println!("adopting         {} {} {}", p.symbol, p.side, p.amount);
        }
    }
    let journal_path = cfg
        .journal
        .clone()
        .unwrap_or_else(|| format!("{}.oqj", program()));
    let no_journal = cfg.journal.is_none();

    let session = match Session::start(
        venue,
        RiskGate::new(limits),
        config,
        &positions,
        &resting,
        &expected,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("startup          REFUSED: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("startup          the venue agrees with what this process expects");

    // The strategy's own books, kept by the kernel a backtest uses.
    //
    // Until this existed the Context below was built from literal zeros,
    // so every strategy that decides by reading `ctx.position` — which is
    // all of them — saw a constant. A strategy that opens when flat
    // opened again on every observation.
    // What a monitoring system reads. Counted here rather than derived
    // afterwards: several of these — a redelivered fill, a report with
    // no trade id — are events that leave no trace in the final state,
    // so a count taken at the end would be zero for all of them.
    let mut metrics = crate::metrics::Snapshot::default();
    let limits = oq_risk::VersionedLimits::new(cfg.limits);
    println!("limits           version {}", limits.version());

    // Derived from the instrument the venue just described, not written
    // down here. A hand-written scale stays plausible while being wrong:
    // prices parse, quantities parse, and every notional is off by a
    // factor nothing reports. The constant this replaces was 10_000 for
    // a contract whose real figure is 100 at four decimal places of
    // quantity and 1000 at three — so exposure was overstated by two
    // orders of magnitude on one deployment and one on the other.
    //
    // It went unnoticed because a second defect hid it: the books netted
    // a hedged account, both legs cancelled, and a flat position is never
    // measured. Fixing the netting without this would have started
    // measuring, at a hundred times the real size.
    let Some(contract) = oq_margin::Contract::of(&instrument) else {
        eprintln!(
            "contract         FAILED: this instrument's tick is worth less than              the smallest cash unit; margin cannot be computed for it"
        );
        return ExitCode::FAILURE;
    };
    println!("contract         {} cash per tick-lot", contract.tick_cash);
    let mut books = crate::books::Books::new(
        oq_types::InstrumentId::new(1),
        contract,
        oq_margin::TierTable::example_btcusdt(),
        // The venue's number, not a configured one. Books opened at a
        // balance nobody read would report an account that is not this.
        starting_balance,
        // As the venue reports it, asked above. A hedged account whose
        // books net is one where two equal legs cancel and everything
        // downstream reads a flat account the venue is charging margin
        // on twice.
        if hedged {
            oq_core::PositionMode::Hedge
        } else {
            oq_core::PositionMode::OneWay
        },
    );

    // The same events, matched by the model instead of by the venue.
    //
    // Built here rather than left to a caller because the whole point is
    // that it sees exactly what the account sees: a shadow fed a
    // different event stream measures the difference between two event
    // streams, which is not the number anybody wanted.
    //
    // It never places anything. `Shadow::apply` returns nothing for that
    // reason — a caller acting on its outputs would be running a second
    // trading system.
    let mut shadow = crate::shadow::Shadow::new(
        oq_types::InstrumentId::new(1),
        oq_margin::Contract::new(10_000),
        oq_margin::TierTable::example_btcusdt(),
        starting_balance,
    );
    // Kept so the adoption can be recorded once the journal exists.
    // Adopting is an in-memory act and has to happen here, before the
    // startup check; recording it has to happen after the writer opens.
    // Conflating the two is what left this step invisible.
    let adopted = adopted_legs(&positions, &instrument, &symbol);
    for (side, lots, entry) in adopted_lots(&positions, &instrument) {
        books.adopt(side, lots, entry, Nanos(now_ns()));
        println!(
            "adopted          {} {} lots at {}",
            if side == Side::Buy { "long" } else { "short" },
            lots.0,
            entry.0
        );
    }

    // Before anything is recorded, read what the last run left. An order
    // this process wrote and never heard about may be resting right now,
    // and starting to trade beside it is the same failure as starting
    // beside an unknown position — which the startup check already
    // refuses.
    // Kept so the strategy can be walked back through its own fills once
    // it exists. A position comes from the venue; a ladder does not —
    // how many rungs had filled is this strategy's own history and only
    // its own journal holds it.
    let mut prior_fills: Vec<(oq_types::Fill, String, String)> = Vec::new();
    if !no_journal && std::path::Path::new(&journal_path).exists() {
        match crate::recover(&journal_path) {
            Ok(prior) => {
                if prior.in_flight.is_empty() {
                    println!("recovery         previous run left nothing unresolved");
                } else {
                    println!(
                        "recovery         {} order(s) unaccounted for in {journal_path}",
                        prior.in_flight.len()
                    );
                    let mut unresolved = 0;
                    for f in &prior.in_flight {
                        match session.venue().order_status(&symbol, &f.client_id) {
                            Ok(Some(a)) => println!(
                                "                 {} exists at the venue: {} ({:?})",
                                f.client_id, a.status, f.reason
                            ),
                            Ok(None) => println!(
                                "                 {} never reached the venue ({:?})",
                                f.client_id, f.reason
                            ),
                            Err(e) => {
                                eprintln!(
                                    "                 {} could not be resolved: {e}",
                                    f.client_id
                                );
                                unresolved += 1;
                            }
                        }
                    }
                    if unresolved > 0 {
                        eprintln!(
                            "recovery         REFUSING to start: {unresolved} order(s) could not be \
                             resolved. Trading beside an order whose state is unknown makes every \
                             limit meaningless."
                        );
                        return ExitCode::FAILURE;
                    }
                }
                prior_fills = prior
                    .fills
                    .iter()
                    .cloned()
                    .zip(prior.fill_decimals.iter().cloned())
                    .map(|(f, (q, p))| (f, q, p))
                    .collect();
                if let Some(prefix) = prior.prefix {
                    if prefix != config_prefix {
                        println!(
                            "recovery         previous prefix was {prefix}, this run uses \
                             {config_prefix}; older orders will read as another system's"
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("recovery         FAILED to read {journal_path}: {e}");
                eprintln!("                 An unreadable journal is not an empty one.");
                return ExitCode::FAILURE;
            }
        }
    }

    // Recording is the default. A run that cannot be replayed cannot be
    // attributed, and attribution is the thing the live path exists to
    // eventually provide — so not recording has to be asked for.
    let session = if no_journal {
        println!("journal          off, by request; nothing here can be replayed");
        session
    } else {
        match oq_journal::Writer::open(&journal_path, oq_journal::SyncPolicy::EveryRecordNoFsync) {
            Ok(w) => {
                println!("journal          {journal_path}");
                session.journalling(w)
            }
            Err(e) => {
                eprintln!("journal          FAILED to open {journal_path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    // The one startup step that carries state across a migration, and
    // until this call it was the one step the journal could not see. A
    // reader rebuilding what this run believes it holds would have come
    // up short by exactly the positions that were migrated.
    let mut session = session;
    session.record_reconciled(Nanos(now_ns()), adopted);

    // Fetched while the session still owns the venue, replayed once the
    // trader exists. A strategy that has not seen its window cannot act,
    // and the first thing it would otherwise do is wait that window out
    // in real time -- on a restart, that is an account left unmanaged
    // for as long as the window is.
    //
    // A failure here is reported and does not stop the run. History
    // makes a strategy ready sooner; it does not make it correct, and
    // refusing to start because a public endpoint would not answer
    // turns a slow start into no start.
    let warm_bars: Option<Vec<oq_gateway::klines::Kline>> = if cfg.warm_minutes == 0 {
        println!("warm-up          off, by request; the strategy starts cold");
        None
    } else {
        match session.venue().recent_bars(&symbol, cfg.warm_minutes) {
            Ok(b) => Some(b),
            Err(e) => {
                println!("warm-up          FAILED: {e}; starting cold");
                None
            }
        }
    };

    let stream = match session.venue().open_user_stream() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("user stream      FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut reader = match UserStreamReader::connect(&stream, Duration::from_millis(200)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("user stream      FAILED to connect: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("user stream      connected");

    let mut agg = match Aggregator::new(window_ns) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("aggregator       FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut supervisor = Supervisor::new(Timings::default());
    let scales = market.scales();
    let event_time = feed_venue.event_time_reader();

    let mut trader = Trader::new(make_strategy(&instrument), session);

    // Replayed through the same books and the same context the live loop
    // builds, so the strategy folds history with the code it already
    // runs rather than a second path that has to agree with the first.
    match warm_bars {
        None => {}
        Some(bars) if bars.is_empty() => {
            println!("warm-up          the venue returned no history; starting cold");
        }
        Some(bars) => {
            let n = bars.len();
            for tick in warm_ticks(&bars) {
                books.on_tick(&tick);
                let ctx = books.context(tick);
                trader.on_history(&ctx);
            }
            println!("warm-up          {n} bar(s) of history replayed");
        }
    }

    // And then its own fills, in order, through a callback that cannot
    // send anything. The strategy folds them with the code it runs live,
    // so it arrives at the state it was in rather than at a summary of
    // it — which is what a snapshot would have given, and a snapshot is
    // a second record of the same thing that can disagree with the
    // first.
    if !prior_fills.is_empty() {
        let n = prior_fills.len();
        for (mut fill, qty, price) in prior_fills {
            let Some(q) = scaled_decimal(&qty, instrument.qty_scale) else {
                continue;
            };
            let Some(p) = scaled_decimal(&price, instrument.price_scale) else {
                continue;
            };
            fill.qty = QtyLots(q);
            fill.price = PriceTicks(p);
            // The context a replayed fill is handed is the books as
            // they stand, with a tick that carries no price: nothing in
            // a replay should be pricing anything, and a stale price
            // offered here would be a number a strategy could act on if
            // it forgot it was replaying.
            let ctx = books.context(oq_engine::Tick::default());
            trader.on_history_fill(&fill, &ctx);
        }
        println!("recovery         {n} of this strategy's own fill(s) replayed");
    }

    // Stamped before the loop so the fee query at the end covers exactly
    // this run. Asking for "everything" would sum a previous run's fees
    // into this one's attribution, which is the sort of number that
    // looks plausible and is somebody else's.
    let started_ms = now_ns() / 1_000_000;
    install_signal_handlers();
    let deadline = match deadline_from(minutes, Instant::now()) {
        Ok(deadline) => deadline,
        Err(why) => {
            eprintln!("running          REFUSED: {why}");
            return ExitCode::FAILURE;
        }
    };
    if deadline.is_some() {
        println!("running          until {minutes} minutes elapse or a signal arrives");
    } else {
        println!("running          until a signal arrives");
    }
    println!();

    let mut ticks = 0_u64;
    let mut sent = 0_u64;
    let mut cancelled = 0_u64;
    // Counted because the difference between them is the number a
    // backtest cannot produce. A summary reporting only what was *asked
    // for* is the same optimism the `on_placed` callback exists to fix,
    // one level up.
    let mut refused = 0_u64;
    let mut unresolved = 0_u64;
    let mut cancel_failed = 0_u64;
    let mut last_tick_report = Instant::now();
    // The last observation, kept so a fill arriving between ticks can be
    // handed a context. A strategy sizing a ladder needs a price, and the
    // most recent one is the honest answer: the alternative is telling it
    // nothing happened until the next tick, by which time the position it
    // is managing has been unmanaged for however long that took.
    let mut last_tick: Option<oq_engine::Tick> = None;

    while deadline.is_none_or(|d| Instant::now() < d) && !shutdown_requested() {
        let now = Nanos(now_ns());

        // Market data, drained rather than sampled.
        //
        // One message per stream per iteration was a rate limit nobody
        // chose. The depth subscription is `@depth@0ms`, which sends on
        // every change; the loop took one of them and then waited on the
        // other stream and the account stream, each with its own read
        // timeout, so an iteration cost about a second. Measured over
        // eighty minutes it consumed 1.08 depth messages a second, and
        // the rest sat in the socket.
        //
        // What that buffer does is visible in the journal now that a
        // tick carries both clocks: the venue's timestamp fell behind
        // the local one by a median of seventeen seconds and a tail of
        // a hundred and forty-three, because the events being processed
        // had arrived that long ago. Then the venue drops a consumer
        // that will not keep up, the backlog is discarded with the
        // connection, and the venue axis jumps forward — which is the
        // hole that three separate readings called a blind window.
        //
        // The budget is a fairness bound, not a rate limit: a firehose
        // on one stream must not starve the account stream, which is
        // where fills arrive.
        for which in [0_u8, 1] {
            for _ in 0..DRAIN_BUDGET {
                let stream = if which == 0 {
                    market.depth()
                } else {
                    market.trade()
                };
                match stream.poll() {
                    Ok(Some(bytes)) => {
                        // Sampled per message, not per pass.
                        //
                        // `now` is taken once at the top of the loop, and
                        // the loop now drains a burst rather than one
                        // message — so a batch shared one local stamp
                        // while the venue's own timestamps advanced
                        // through it. The record then showed the venue's
                        // clock running up to seven seconds *ahead* of
                        // this process, which cannot happen: the venue
                        // and this host agree to within a second, and
                        // the whole point of carrying both clocks is to
                        // tell delivery from timekeeping. A stamp that
                        // is really the pass's start tells neither.
                        let seen = now_ns();
                        let at = event_time(&bytes).unwrap_or(seen);
                        let closed = if which == 0 {
                            feed_venue
                                .parse_depth(&bytes, scales)
                                .ok()
                                .and_then(|u| agg.on_depth(at, seen, &u))
                        } else {
                            feed_venue
                                .parse_trade(&bytes, scales)
                                .and_then(|t| agg.on_trade(at, seen, &t))
                        };
                        if let Some(tick) = closed {
                            ticks += 1;
                            trader.record_tick(&tick);
                            metrics.ticks += 1;
                            shadow.on_tick(tick);
                            for output in books.on_tick(&tick) {
                                // Under venue matching the kernel does not
                                // fill, so anything here is the account
                                // going past its maintenance requirement —
                                // which is worth a line rather than a
                                // silence.
                                println!("books            {output:?}");
                            }
                            last_tick = Some(tick);
                            let ctx = books.context(tick);
                            for outcome in trader.on_tick(&ctx, now) {
                                if let Outcome::Sent { local, .. } = &outcome {
                                    // The kernel is told an order exists.
                                    // Without this `Context::working` is zero
                                    // for every live strategy that reads it,
                                    // and the books hold no order for a fill
                                    // to answer.
                                    if let Some((side, qty, offset)) =
                                        shape_of(trader.submitted(), *local)
                                    {
                                        books.on_submit(*local, side, qty, offset, now);
                                        shadow.apply(&oq_core::Event::Submit {
                                            instrument: None,
                                            id: *local,
                                            side,
                                            price: None,
                                            qty,
                                            offset,
                                            stamp: oq_types::Stamp::new(now.0, now.0),
                                        });
                                    }
                                }
                                match &outcome {
                                    Outcome::Sent { .. } => sent += 1,
                                    Outcome::Cancelled { .. } => cancelled += 1,
                                    Outcome::Refused { .. } => refused += 1,
                                    Outcome::Unresolved { .. } => unresolved += 1,
                                    Outcome::CancelFailed { .. } => cancel_failed += 1,
                                    _ => {}
                                }
                                report(&outcome);
                            }
                        }
                    }
                    // Nothing more waiting: on to the other stream.
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("{} stream lost: {e}", stream.name());
                        break;
                    }
                }
            }
        }

        // The account's own stream.
        match reader.next() {
            StreamOutcome::Event(UserEvent::Order(u)) => {
                println!(
                    "fill/update      {} {} qty {} @ {}",
                    u.client_id, u.status, u.cumulative_qty, u.last_price
                );
                // Before `apply`, which may end the order and forget the
                // association the translation depends on.
                let local = trader.local_id(&u.client_id).unwrap_or(OrderId(0));
                trader.apply(&u);
                let parsed = fill_of(&u, &instrument, local);
                if let Err(why) = &parsed
                    && *why != NOT_A_FILL
                {
                    // A report the venue sent and this process did not
                    // book. Said out loud and counted, because the
                    // consequence is that the position believed here is
                    // smaller than the account's, and silence about that
                    // is indistinguishable from a fill that never
                    // happened.
                    metrics.unbookable_reports += 1;
                    println!(
                        "fill/DROPPED     {} {}: {why} — the position here is now smaller \
                         than the account's",
                        u.client_id, u.last_price
                    );
                }
                if let Ok(fill) = parsed {
                    shadow.on_venue_fill(
                        fill.order,
                        fill.side,
                        fill.price,
                        fill.qty,
                        fill.stamp.exch,
                    );
                    match books.on_venue_fill(&fill) {
                        crate::books::Booked::Applied(outputs) => {
                            for output in outputs {
                                println!("books            {output:?}");
                            }
                            // Written after the books accept it, so the
                            // journal holds what was believed rather
                            // than what arrived: a redelivered fill is
                            // discarded above and must not be recorded
                            // twice, or a replay would count it twice.
                            trader.record_fill(&fill, &u.client_id);
                            // And tell the strategy, which is the whole
                            // reason it has an `on_fill`. Until this line
                            // existed a strategy could open a position
                            // live and never learn that it had: a run
                            // opened two and placed neither a ladder nor
                            // a take-profit against them, because nothing
                            // ever called the callback that would have.
                            //
                            // After the books, so the context the
                            // strategy reads already contains this fill.
                            if let Some(t) = last_tick {
                                let ctx = books.context(t);
                                for outcome in trader.on_fill(&fill, &ctx, now) {
                                    match &outcome {
                                        Outcome::Sent { .. } => sent += 1,
                                        Outcome::Cancelled { .. } => cancelled += 1,
                                        Outcome::Refused { .. } => refused += 1,
                                        Outcome::Unresolved { .. } => unresolved += 1,
                                        Outcome::CancelFailed { .. } => cancel_failed += 1,
                                        Outcome::UnknownOrder(_) => {}
                                    }
                                    report(&outcome);
                                }
                            }
                        }
                        // Routine after a reconnect, and worth a line:
                        // a stream repeating itself is a fact about the
                        // link, and silence would hide how often.
                        crate::books::Booked::Duplicate => {
                            metrics.duplicate_fills += 1;
                            metrics.fills = metrics.fills.saturating_sub(1);
                            println!("books            trade {} already booked", fill.trade.0);
                        }
                        crate::books::Booked::Unidentifiable => {
                            metrics.unidentifiable_fills += 1;
                            metrics.fills = metrics.fills.saturating_sub(1);
                            println!(
                                "books            {} reported a fill with no trade id; \
                                 not booked, because it cannot be deduplicated",
                                u.client_id
                            );
                        }
                    }
                } else if matches!(u.status.as_str(), "CANCELED" | "EXPIRED") {
                    books.on_closed();
                }
                // The end of the order, which is not the same event as
                // its last fill and must not be inferred from one. A
                // limit order fills in pieces; only the venue knows
                // which piece was the last, and this is where it says
                // so.
                //
                // The strategy is told before the association is
                // dropped — `on_ended` does both, in that order — so a
                // strategy can release the order's identity at the same
                // moment the host does, rather than guessing at the
                // first fill and being wrong for every one after it.
                if let Some(ending) = ending_of(&u.status) {
                    match last_tick {
                        Some(t) => {
                            let ctx = books.context(t);
                            for outcome in trader.on_ended(&u.client_id, ending, &ctx, now) {
                                match &outcome {
                                    Outcome::Sent { .. } => sent += 1,
                                    Outcome::Cancelled { .. } => cancelled += 1,
                                    Outcome::Refused { .. } => refused += 1,
                                    Outcome::Unresolved { .. } => unresolved += 1,
                                    Outcome::CancelFailed { .. } => cancel_failed += 1,
                                    Outcome::UnknownOrder(_) => {}
                                }
                                report(&outcome);
                            }
                        }
                        // No tick yet means no context to decide in, and
                        // an order that ended still has to be released
                        // or its id is held for the life of the process.
                        None => trader.forget(&u.client_id),
                    }
                }
            }
            StreamOutcome::Event(event) => {
                for action in supervisor.on_event(&event) {
                    act(&action, &mut trader, &symbol);
                }
            }
            StreamOutcome::Disconnected(why) => {
                metrics.disconnects += 1;
                eprintln!("user stream      lost: {why}");
                for action in supervisor.on_disconnect() {
                    act(&action, &mut trader, &symbol);
                }
                // Reconnect here, where the reader is. `act` cannot: it
                // is handed a trader and a symbol, and a stream is
                // neither — so `Action::Reconnect` reached it and did
                // nothing at all.
                //
                // The cost of that was not a warning. The reader is
                // deliberately not self-healing — the module says so,
                // because a caller has to reconcile before trusting its
                // books again — so a dropped stream stayed dropped, and
                // a run went three hours reporting the loss thousands of
                // times while receiving no account events. It held a
                // position and could not have learned that it closed.
                match trader.venue().open_user_stream() {
                    Ok(fresh) => {
                        match UserStreamReader::connect(&fresh, Duration::from_millis(200)) {
                            Ok(r) => {
                                reader = r;
                                // Said out loud. A log that reports every
                                // loss and no recovery leaves a reader
                                // unable to tell whether it is still down,
                                // which is the question they opened it for.
                                println!("user stream      reconnected");
                            }
                            Err(e) => eprintln!("user stream      reconnect FAILED: {e}"),
                        }
                    }
                    Err(e) => eprintln!("user stream      reopen FAILED: {e}"),
                }
            }
            StreamOutcome::Idle | StreamOutcome::Ignored => {}
        }

        // Upkeep that time makes due, whether or not anything arrived.
        for action in supervisor.due(now) {
            act(&action, &mut trader, &symbol);
        }

        if last_tick_report.elapsed() >= Duration::from_secs(30) {
            last_tick_report = Instant::now();
            // Submissions the venue never answered, asked about again.
            // A round trip, so it belongs on the heartbeat and not on
            // the observation path. Each answer reaches the strategy
            // through `on_placed`, which is where it would have arrived
            // had the venue answered the first time.
            for (local, resting) in trader.chase_unanswered() {
                if resting {
                    println!("resolved         {local:?} is resting after all");
                } else {
                    println!("resolved         {local:?} never landed; it may be sent again");
                }
            }
            // Sampled and recorded together, so what a reader sees in
            // the terminal and what a replay sees in the journal are the
            // same observation rather than two that happen to agree.
            let now_ns = Nanos(now_ns());
            trader.record_waiting(now_ns);
            let waiting = trader.waiting_summary();
            let unanswered = trader.unanswered();
            let pending = if unanswered > 0 {
                format!(", {unanswered} unanswered")
            } else {
                String::new()
            };
            println!(
                "heartbeat        {ticks} ticks, {} resting{pending}{waiting}",
                trader.working()
            );
            // Why the tick count is what it is.
            //
            // A run that stops producing ticks looks the same from
            // outside whether the feed went quiet or the events kept
            // arriving and were folded into a window that never closed.
            // Every number that separates those two was already being
            // counted, and none of it was ever read — so the question
            // could be answered only by measuring gaps in the journal
            // afterwards, and only by someone who already suspected.
            //
            // `ooo` is the one that decides it. An event whose exchange
            // timestamp goes backwards is clamped to the high-water mark
            // and lands in the window already open, so a stream of them
            // advances nothing while the connection looks healthy.
            let c = agg.counts();
            println!(
                "feed             depth {}, trades {}, ooo {}, quiet {}, pre-trade {}",
                c.depth_applied,
                c.trades,
                c.out_of_order,
                c.quiet_windows,
                c.windows_before_first_trade
            );
        }
    }

    println!();
    println!("stopping         cancelling anything still resting");
    trader.cancel_all(&symbol);
    let _ = reader.close();
    let _ = trader.close_stream();

    println!("ticks            {ticks}");

    // What the model would have done with the same observations, and
    // where it differs. Printed even when nothing traded: "no divergence
    // because no trade" is a different statement from "no divergence",
    // and only one of them is evidence.
    // Asked once, at the end, over the window this run covered. A
    // failure is reported and not fatal: the run is over, and refusing
    // to print the rest of the report because one component could not be
    // read would lose the components that could.
    let shadow_fees = shadow.model_fees();
    let venue_fees = match trader.venue().fees_charged(&symbol, started_ms) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fees             could not be read: {e}");
            None
        }
    };
    shadow.finish(Nanos(now_ns()));
    println!();
    let divergences = shadow.divergences();
    let flattering = shadow.flattering();
    println!(
        "shadow           {} divergence(s), {flattering} of them flattering the model",
        divergences.len()
    );
    // Every one, not a sample. A divergence that was summarised away is
    // one nobody attributed, and the count above is the thing this run
    // exists to produce.
    for d in divergences {
        println!(
            "  {}{}",
            if d.flatters_the_model() { "! " } else { "  " },
            d.summary_line()
        );
    }
    let attribution = oq_parity::attribution::attribute(
        // Identified by what this run was, so a report cannot be
        // mistaken for one from a different build or contract. The
        // limits are in the config hash because a limit that fired is
        // part of why the two arms differ.
        oq_parity::RunManifest::from_content(
            option_env!("GIT_COMMIT").unwrap_or("unknown"),
            symbol.as_bytes(),
            format!("{:?}", cfg.limits).as_bytes(),
            format!("{}-{symbol}", cfg.strategy_name),
        ),
        &instrument,
        books.realized_net(),
        shadow.model_pnl(),
        // Funding stays `None`: the venue reports it on an endpoint this
        // adapter does not read, and zero is a measurement nobody took.
        // Fees are asked for, and an adapter that does not report them
        // says so rather than answering zero — either way `attribution`
        // renders the component honestly and the residual carries what
        // is missing.
        &shadow.evidence(None, venue_fees.map(|v| (v, shadow_fees))),
    );
    print!("{}", attribution.render());

    // What a monitoring system would have read, and what it would have
    // woken somebody for. Printed at the end because this build has no
    // scrape endpoint — the snapshot is a value, and where it goes is
    // the operator's choice rather than this crate's dependency.
    metrics.foreign_orders = trader.foreign() as u64;
    println!();
    print!("{}", metrics.render(None));
    let raised = crate::metrics::alerts(&metrics, crate::metrics::AlertRules::default());
    if raised.is_empty() {
        println!();
        println!("alerts           none");
    } else {
        println!();
        for a in &raised {
            println!(
                "alerts           {} {}: {}",
                if a.urgent { "URGENT" } else { "notice" },
                a.name,
                a.detail
            );
        }
    }
    if limits.version() != 1 {
        println!("limits           ended at version {}", limits.version());
    }
    println!("orders           {sent} placed, {refused} refused, {cancelled} withdrawn");
    // Zero is the expected number and is not printed. Above zero means
    // orders this process asked the venue to withdraw were still
    // resting afterwards, which is the state in which two orders close
    // the same position and the second one opens the opposite side.
    if cancel_failed > 0 {
        println!("CANCEL FAILED    {cancel_failed} withdrawal(s) the venue did not accept");
    }
    if unresolved > 0 {
        // Loud, and only when it happened. This is the count that says
        // the account may not be where this summary claims it is, so it
        // does not belong on the same line as the ones that are known.
        println!(
            "UNRESOLVED       {unresolved} submission(s) never got an answer — reconcile \
             against the venue before trusting anything above"
        );
    }
    // The in-process segment only. G6's far boundary is the socket write,
    // which the HTTP client does not expose, so this is not the gate's
    // number and is not labelled as it.
    println!(
        "submit latency   {} (journal flush to client call)",
        trader.latency()
    );
    println!(
        "duplicates       {} redelivered fills discarded",
        trader.duplicates()
    );
    // Above zero means the account is shared. Worth reading here rather
    // than inferring it later from a limit that filled up while this
    // process placed nothing.
    println!(
        "other systems    {} events on this account belonged to something else",
        trader.foreign()
    );
    // Above zero means this process and the account disagree about the
    // position, and the disagreement is this process's fault rather than
    // the venue's. Reported unconditionally: a zero here is the only
    // thing that makes the numbers above believable.
    println!(
        "unbookable       {} report(s) the venue sent and this build could not read",
        metrics.unbookable_reports
    );
    // Connections made and connections dropped for going quiet. The
    // second number is the one that used to be invisible: a stream that
    // stops delivering reads exactly like a quiet market, and the only
    // way to see it was to measure the gaps in the journal afterwards.
    println!(
        "market data      depth {} connection(s), {} silent; trade {} connection(s), {} silent",
        market.depth().connections(),
        market.depth().stalls(),
        market.trade().connections(),
        market.trade().stalls()
    );
    ExitCode::SUCCESS
}

/// The pieces of a [`Trader`] this loop uses, so the strategy type does
/// not have to appear in the loop's own signature.
///
/// `on_tick` and `forget` are deliberately absent: `Trader` has them
/// inherently, and a trait method shadowed by an inherent one is dead
/// code that still compiles.
trait TraderLike {
    fn apply(&mut self, u: &oq_gateway::OrderUpdate) -> bool;
    fn working(&self) -> u32;
    fn duplicates(&self) -> u64;
    fn foreign(&self) -> u64;
    fn record_tick(&mut self, tick: &oq_engine::Tick);
    /// Sample what the strategy is waiting for, and record it.
    fn record_waiting(&mut self, at: Nanos);
    /// A fill the books accepted, for the journal.
    fn record_fill(&mut self, fill: &oq_types::Fill, client_id: &str);
    /// One of this strategy's own fills, replayed. Cannot send.
    fn on_history_fill(&mut self, fill: &oq_types::Fill, ctx: &Context);
    /// One historical observation. Cannot produce intents.
    fn on_history(&mut self, ctx: &Context);
    /// The same conditions, rendered for the terminal.
    fn waiting_summary(&self) -> String;
    fn latency(&self) -> String;
    fn cancel_all(&mut self, symbol: &str);
    fn close_stream(&self) -> Result<(), oq_gateway::VenueError>;
    fn reconcile(&mut self, symbol: &str);
    fn renew(&self);
    fn halt(&self, why: &str);
}

impl<S: Strategy> TraderLike for Trader<S, Box<dyn Account>> {
    fn apply(&mut self, u: &oq_gateway::OrderUpdate) -> bool {
        self.session_mut().apply(u)
    }
    fn working(&self) -> u32 {
        self.session().book().working()
    }
    fn duplicates(&self) -> u64 {
        self.session().book().duplicates()
    }
    fn foreign(&self) -> u64 {
        self.session().book().foreign()
    }
    fn on_history(&mut self, ctx: &Context) {
        self.strategy_mut().on_history(ctx);
    }

    fn on_history_fill(&mut self, fill: &oq_types::Fill, ctx: &Context) {
        self.strategy_mut().on_history_fill(fill, ctx);
    }

    fn waiting_summary(&self) -> String {
        let w = self.strategy().waiting_on();
        if w.is_empty() {
            return String::new();
        }
        let body: Vec<String> = w.iter().map(|(k, v)| format!("{k} {v}")).collect();
        format!(", waiting on {}", body.join(", "))
    }

    fn record_waiting(&mut self, at: Nanos) {
        let entries: Vec<(String, i64)> = self
            .strategy()
            .waiting_on()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        self.session_mut().record_waiting(at, entries);
    }

    fn record_fill(&mut self, fill: &oq_types::Fill, client_id: &str) {
        self.session_mut().record_fill(fill, client_id);
    }

    fn record_tick(&mut self, tick: &oq_engine::Tick) {
        self.session_mut().record_tick(tick);
    }
    fn latency(&self) -> String {
        self.session().submit_latency().summary()
    }
    fn cancel_all(&mut self, _symbol: &str) {
        for id in self
            .resting()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        {
            let _ = self.session().cancel(&id);
        }
    }
    fn close_stream(&self) -> Result<(), oq_gateway::VenueError> {
        self.session().venue().close_user_stream()
    }
    fn reconcile(&mut self, symbol: &str) {
        match self.session().venue().positions(symbol) {
            Ok(p) => self.session_mut().reconcile(&p),
            Err(e) => eprintln!("reconcile        FAILED: {e}"),
        }
    }
    fn renew(&self) {
        if let Err(e) = self.session().venue().keepalive_user_stream() {
            eprintln!("keepalive        FAILED: {e}");
        }
    }
    fn halt(&self, why: &str) {
        eprintln!("HALT             {why}");
        self.session().gate().kill_switch().trip();
    }
}

fn act<T: TraderLike>(action: &Action, trader: &mut T, symbol: &str) {
    match action {
        Action::RenewKey => trader.renew(),
        Action::CheckPositions | Action::Reconcile => trader.reconcile(symbol),
        // Reconnection of the account stream is left to the next
        // iteration's read, which opens one when it finds none. What
        // matters here is that the reconcile that follows it happens.
        // Reconnect is handled where the reader is, in the loop. It
        // cannot be done here: `act` is given a trader and a symbol, and
        // a stream is neither. Left as a no-op rather than removed
        // because the supervisor is right to emit it — the arm below is
        // the acknowledgement that this function is the wrong place, and
        // for a long time it was the whole of the handling.
        Action::Reconnect => {}
        Action::Halt(why) => trader.halt(why),
    }
}

fn report(outcome: &Outcome) {
    match outcome {
        Outcome::Sent { local, client_id } => println!("sent             {local:?} as {client_id}"),
        Outcome::Refused { local, why } => println!("refused          {local:?}: {why}"),
        // Deliberately not printed as a refusal. An operator reading
        // "refused" concludes the order does not exist and that the
        // account is where they left it; here neither is known.
        Outcome::Unresolved {
            local,
            client_id,
            why,
        } => println!(
            "UNRESOLVED       {local:?} ({client_id}): {why} — this order may be resting; \
             do not replace it"
        ),
        Outcome::Cancelled { local, client_id } => {
            println!("cancelled        {local:?} ({client_id})");
        }
        Outcome::CancelFailed {
            local,
            client_id,
            why,
        } => println!(
            "CANCEL FAILED    {local:?} ({client_id}): {why} — this order is still resting"
        ),
        Outcome::UnknownOrder(id) => println!("unknown order    {id:?} — not in this run's map"),
    }
}

/// Messages taken from one market data stream in one pass.
///
/// A bound on fairness rather than on throughput: the loop drains until
/// the stream is empty, and this stops a burst on one stream from
/// starving the other and the account stream behind it.
const DRAIN_BUDGET: usize = 256;

/// The smallest quantity whose notional clears the contract's floor.
///
/// A tenth over it rather than exactly it, because the floor is checked
/// against a price that can move between sizing and arrival, and landing
/// exactly on a minimum is landing under it half the time.
/// The smallest quantity this venue will accept at this price.
///
/// Kept as a free function because callers here read better for it; the
/// rule itself belongs to the contract and lives there, so a strategy
/// can ask the same question without depending on the live host.
#[must_use]
pub fn smallest_allowed(instrument: &Instrument, price: PriceTicks) -> QtyLots {
    instrument.smallest_allowed(price)
}

/// The fill inside an order update, when there is one.
///
/// `None` for an update that reports no new quantity — an
/// acknowledgement, a cancellation, an expiry. This is the one place in
/// the live loop where double-counting is easy: the venue sends an
/// update per state change, several of them carry a cumulative
/// quantity, and booking that cumulative number more than once would
/// build a position out of one trade.
///
/// So the quantity taken is `last_qty`, the amount *this* update filled,
/// and an update whose `last_qty` is zero is not a fill however
/// promising its status looks.
/// An update that reports no traded quantity, which is ordinary.
///
/// Distinguished from a report that could not be read, because the
/// difference is whether anyone should be told: one is every order's
/// acknowledgement and the other means this build and the venue disagree
/// about what a fill looks like.
const NOT_A_FILL: &str = "no traded quantity";

fn fill_of(
    u: &oq_gateway::OrderUpdate,
    instrument: &Instrument,
    order: OrderId,
) -> Result<oq_types::Fill, &'static str> {
    let scaled = |text: &str, scale: u8| -> Option<i64> {
        let (int, frac) = text.split_once('.').unwrap_or((text, ""));
        let mut digits = String::from(int.trim_start_matches('+'));
        let frac: String = frac.chars().take(usize::from(scale)).collect();
        digits.push_str(&frac);
        for _ in frac.len()..usize::from(scale) {
            digits.push('0');
        }
        digits.parse::<i64>().ok()
    };

    let qty = scaled(&u.last_qty, instrument.qty_scale).ok_or("quantity is not a number")?;
    if qty == 0 {
        // Not a fill. A venue reports an order's whole life on this
        // channel — accepted, cancelled, expired — and those carry no
        // traded quantity because nothing traded. Reporting them as
        // reports that could not be read would fire an urgent alert on
        // every order placed, and an alert that fires on ordinary
        // operation is one nobody reads when it means something.
        return Err(NOT_A_FILL);
    }
    if qty < 0 {
        return Err("quantity is negative");
    }
    let price = scaled(&u.last_price, instrument.price_scale).ok_or("price is not a number")?;
    if price <= 0 {
        // A fill with no price is a report this build cannot book, and
        // booking it at zero would price the position at nothing — which
        // is the shape of a real incident: a synthetic zero-price fill
        // poisoned a position's average and every order derived from it.
        return Err("price is not positive");
    }

    Ok(oq_types::Fill {
        stamp: oq_types::Stamp::new(now_ns(), now_ns()),
        instrument: oq_types::InstrumentId::new(1),
        // The strategy's own id, translated from the client id the venue
        // reports against. Zero when this process did not send the order
        // — another system on the same account, or one from a previous
        // run — and a strategy that looks for its own id will correctly
        // find nothing.
        order,
        // The deduplication key. Without it the fill cannot be
        // booked at all, which `Books` enforces rather than trusting.
        trade: oq_types::TradeId(u.trade_id.unwrap_or(0).unsigned_abs()),
        side: if u.side.eq_ignore_ascii_case("BUY") {
            Side::Buy
        } else {
            Side::Sell
        },
        // Which leg the fill belongs to decides whether it opened or
        // closed. A sell on the long leg reduces it; the same sell on
        // the short leg opens. Reading every fill as opening leaves a
        // position that never goes away, which a strategy will keep
        // trying to close: on a live account that ran seven times in
        // forty seconds, leaving a leg at seven times its intended size,
        // and it stopped only because the account stream died.
        //
        // `BOTH` is a one-way account, where the venue nets and the
        // engine can infer the effect from the sign. Opening is the
        // reading there for the reason the comment this replaces gave:
        // a close mistaken for an open overstates the position and the
        // reconciler catches it, while the reverse quietly forgets one
        // that is still there.
        offset: offset_of(&u.position_side, &u.side),
        price: oq_types::PriceTicks(price),
        qty: QtyLots(qty),
        liquidity: if u.maker {
            oq_types::Liquidity::Maker
        } else {
            oq_types::Liquidity::Taker
        },
    })
}

/// Venue positions, converted to this instrument's lots and ticks.
///
/// Pulled out of `run` so the conversion is testable: `run` needs a
/// venue and cannot be called from a test, and the arithmetic here is
/// where a real mistake would hide — a scale applied to the wrong field,
/// or a short adopted as a long.
fn adopted_lots(
    positions: &[oq_gateway::binance::PositionSnapshot],
    instrument: &Instrument,
) -> Vec<(Side, QtyLots, PriceTicks)> {
    positions
        .iter()
        .filter(|p| p.amount != 0.0)
        .map(|p| {
            // From the venue's own digits, not from a float made of
            // them. 0.0058 becomes 57.999999999999993 the moment it is
            // scaled by ten thousand, and a conversion through that
            // float adopts fifty-seven of fifty-eight lots — the one
            // left over held by the account and managed by nobody, and
            // still there after the position closes.
            //
            // Rounding rather than truncating repairs that case and not
            // the class. Reading the digits is exact for every size,
            // rather than for the ones somebody thought to test.
            let lots = QtyLots(
                scaled_decimal(&p.amount_text, instrument.qty_scale)
                    .unwrap_or(0)
                    .abs(),
            );
            let entry =
                PriceTicks(scaled_decimal(&p.entry_text, instrument.price_scale).unwrap_or(0));
            // The leg the venue named, not the sign of the amount. On a
            // hedged account they are different questions, and a venue
            // can report a leg whose amount has a sign the leg should
            // not have — one did, reporting a LONG leg at a negative
            // quantity after a defect elsewhere let sells run past zero.
            // Reading the sign there adopts onto the opposite leg, and a
            // takeover onto the opposite leg opens where it meant to
            // close.
            //
            // `BOTH` is a one-way account: one leg, and the sign is the
            // only thing that can say which way it points.
            let side = if p.position_side.eq_ignore_ascii_case("LONG") {
                Side::Buy
            } else if p.position_side.eq_ignore_ascii_case("SHORT") {
                Side::Sell
            } else if p.amount > 0.0 {
                Side::Buy
            } else {
                Side::Sell
            };
            (side, lots, entry)
        })
        .collect()
}

/// The same positions, in the shape the journal records them.
///
/// Signed lots rather than a side plus a magnitude, because a reader
/// summing a column should get the net without having to know the
/// convention — and the side string is kept beside it so a hedged
/// account's two legs stay distinguishable when they net to zero.
fn adopted_legs(
    positions: &[oq_gateway::binance::PositionSnapshot],
    instrument: &Instrument,
    symbol: &str,
) -> Vec<(String, String, i64, i64)> {
    adopted_lots(positions, instrument)
        .into_iter()
        .map(|(side, lots, entry)| {
            let (name, signed) = if side == Side::Buy {
                ("LONG", lots.0)
            } else {
                ("SHORT", -lots.0)
            };
            (symbol.to_string(), name.to_string(), signed, entry.0)
        })
        .collect()
}

/// The end of an order, as the venue names it.
///
/// `PARTIALLY_FILLED` is the one that matters and the one that is
/// absent: it is a report about an order that is still resting, still
/// the venue's to fill, and still the strategy's to recognise. Reading
/// it as an ending is how a strategy comes to stop recognising the rest
/// of its own order.
///
/// `NEW` is likewise not an ending, and `REJECTED` never reaches here —
/// an order that was refused is answered through `on_placed`, which is
/// the callback for whether it ever existed.
fn ending_of(status: &str) -> Option<Ending> {
    match status {
        "FILLED" => Some(Ending::Filled),
        "CANCELED" | "EXPIRED" => Some(Ending::Cancelled),
        _ => None,
    }
}

#[cfg(test)]
mod endings {
    use super::{Ending, ending_of};

    /// The status that is not an ending.
    ///
    /// A strategy released its entry order on the first partial fill.
    /// The rest of that entry then arrived carrying an id that matched
    /// nothing it held, was dropped, and the position it built was
    /// managed by nobody. Eight hours later the account held three and a
    /// half times what the strategy believed, and the take-profit
    /// resting against it covered a fifth of it.
    #[test]
    fn a_partial_fill_does_not_end_an_order() {
        assert_eq!(ending_of("PARTIALLY_FILLED"), None);
    }

    #[test]
    fn an_acknowledgement_does_not_end_an_order() {
        assert_eq!(ending_of("NEW"), None);
    }

    /// A refusal is answered elsewhere, and answering it twice would
    /// tell a strategy an order it never had has now ended.
    #[test]
    fn a_refusal_is_not_an_ending() {
        assert_eq!(ending_of("REJECTED"), None);
    }

    #[test]
    fn the_size_arrived_or_it_never_will() {
        assert_eq!(ending_of("FILLED"), Some(Ending::Filled));
        assert_eq!(ending_of("CANCELED"), Some(Ending::Cancelled));
        assert_eq!(ending_of("EXPIRED"), Some(Ending::Cancelled));
    }
}

#[cfg(test)]
mod adoption {
    use super::{adopted_legs, adopted_lots};
    use oq_gateway::binance::PositionSnapshot;
    use oq_types::{Instrument, Side};

    fn leg(amount: f64, entry: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: if amount > 0.0 { "LONG" } else { "SHORT" }.into(),
            // The venue's own digits, which is what the conversion reads.
            // Written here at the precision these tests use.
            amount_text: format!("{amount:.4}"),
            entry_text: format!("{entry:.2}"),
            amount,
            entry_price: entry,
            unrealized: 0.0,
        }
    }

    /// Quantity uses the quantity scale and price uses the price scale.
    ///
    /// Swapping them is the mistake that reads as a plausible number:
    /// at 2 and 4 decimal places both conversions produce something in
    /// the right order of magnitude, and the position would simply be
    /// wrong by a hundredfold in one field.
    #[test]
    fn each_field_uses_its_own_scale() {
        let got = adopted_lots(&[leg(0.016, 63_735.2)], &Instrument::linear(2, 4));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.0, 160, "0.016 at four decimal places");
        assert_eq!(got[0].2.0, 6_373_520, "63735.2 at two decimal places");
    }

    /// A short is adopted as a short.
    #[test]
    fn the_side_follows_the_sign() {
        let i = Instrument::linear(2, 4);
        assert_eq!(adopted_lots(&[leg(0.016, 1.0)], &i)[0].0, Side::Buy);
        assert_eq!(adopted_lots(&[leg(-0.016, 1.0)], &i)[0].0, Side::Sell);
    }

    /// On a hedged account the leg the venue named wins over the sign.
    ///
    /// A venue can report a leg whose amount has a sign the leg should
    /// not have — one did, reporting a LONG leg at a negative quantity
    /// after sells were allowed to run past zero. Reading the sign there
    /// adopts the position onto the *opposite* leg, and a takeover onto
    /// the opposite leg is one that opens where it meant to close.
    #[test]
    fn a_named_leg_wins_over_the_sign() {
        let mut long_gone_negative = leg(-0.014, 69_544.2);
        long_gone_negative.position_side = "LONG".into();
        assert_eq!(
            adopted_lots(&[long_gone_negative], &Instrument::linear(2, 4))[0].0,
            Side::Buy,
            "the venue said LONG; the sign is the anomaly, not the answer"
        );
    }

    /// A one-way account has one leg, and only the sign can say which
    /// way it points.
    #[test]
    fn a_netting_account_still_reads_the_sign() {
        let mut netted = leg(-0.016, 1.0);
        netted.position_side = "BOTH".into();
        assert_eq!(
            adopted_lots(&[netted], &Instrument::linear(2, 4))[0].0,
            Side::Sell
        );
    }

    /// A flat leg is not a position, and recording it as one would put a
    /// zero-sized holding in the journal for a reader to explain.
    #[test]
    fn a_flat_leg_is_not_adopted() {
        assert!(adopted_lots(&[leg(0.0, 63_735.2)], &Instrument::linear(2, 4)).is_empty());
    }

    /// The journal's lots are signed, so summing the column gives the net.
    #[test]
    fn the_recorded_lots_carry_their_sign() {
        let legs = adopted_legs(
            &[leg(0.016, 60_000.0), leg(-0.004, 61_000.0)],
            &Instrument::linear(2, 4),
            "BTCUSDT",
        );
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].1, "LONG");
        assert_eq!(legs[0].2, 160);
        assert_eq!(legs[1].1, "SHORT");
        assert_eq!(legs[1].2, -40, "a short is negative in the record");
        let net: i64 = legs.iter().map(|l| l.2).sum();
        assert_eq!(net, 120, "the column sums to the net position");
    }

    /// Both legs of a hedged account survive even when they net to zero.
    #[test]
    fn a_hedged_pair_that_nets_to_zero_is_still_two_legs() {
        let legs = adopted_legs(
            &[leg(0.016, 60_000.0), leg(-0.016, 61_000.0)],
            &Instrument::linear(2, 4),
            "BTCUSDT",
        );
        assert_eq!(
            legs.len(),
            2,
            "a net of zero is not the absence of a position"
        );
        assert_eq!(legs.iter().map(|l| l.2).sum::<i64>(), 0);
        assert_ne!(legs[0].3, legs[1].3, "each leg keeps its own basis");
    }
}

#[cfg(test)]
mod unreadable_reports {
    use super::fill_of;
    use oq_types::{Instrument, OrderId};

    fn update(price: &str, qty: &str) -> oq_gateway::OrderUpdate {
        oq_gateway::OrderUpdate {
            client_id: "oq-1".into(),
            status: "FILLED".into(),
            side: "BUY".into(),
            position_side: "BOTH".into(),
            last_qty: qty.into(),
            last_price: price.into(),
            cumulative_qty: qty.into(),
            trade_id: Some(1),
            maker: false,
            event_ms: 0,
            symbol: "BTCUSDT".into(),
            venue_id: 0,
        }
    }

    /// A zero price is refused, and the refusal says which field.
    ///
    /// This is the shape of a real incident: a synthetic fill carrying
    /// no price poisoned a position's average, and every order derived
    /// from that average went out at a nonsense price. Booking it at
    /// zero prices the position at nothing.
    #[test]
    fn a_zero_price_is_refused_by_name() {
        let e = fill_of(&update("0", "0.001"), &Instrument::linear(2, 3), OrderId(1))
            .expect_err("must refuse");
        assert!(e.contains("price"), "{e}");
    }

    #[test]
    fn a_negative_price_is_refused() {
        assert!(
            fill_of(
                &update("-100.0", "0.001"),
                &Instrument::linear(2, 3),
                OrderId(1)
            )
            .is_err()
        );
    }

    /// An acknowledgement is not a dropped fill.
    ///
    /// A venue reports an order's whole life on this channel: accepted,
    /// cancelled, expired. None of them carries a traded quantity,
    /// because nothing traded. Counting them as reports that could not
    /// be read fires an urgent alert on every order placed — which was
    /// visible on a live run within seconds of the first order.
    #[test]
    fn an_acknowledgement_is_not_a_dropped_fill() {
        let e = fill_of(&update("100.0", "0"), &Instrument::linear(2, 3), OrderId(1))
            .expect_err("not a fill");
        assert_eq!(e, super::NOT_A_FILL, "and it is distinguishable by name");
    }

    /// A negative quantity is not ordinary, and is still reported.
    #[test]
    fn a_negative_quantity_is_refused_by_name() {
        let e = fill_of(
            &update("100.0", "-1"),
            &Instrument::linear(2, 3),
            OrderId(1),
        )
        .expect_err("must refuse");
        assert!(e.contains("negative"), "{e}");
        assert_ne!(e, super::NOT_A_FILL);
    }

    /// Text this build cannot read is refused rather than guessed at.
    #[test]
    fn an_unparseable_price_is_refused() {
        assert!(
            fill_of(
                &update("not a number", "0.001"),
                &Instrument::linear(2, 3),
                OrderId(1)
            )
            .is_err()
        );
    }

    /// And a report that is fine comes through unchanged.
    #[test]
    fn a_readable_report_is_booked() {
        let f = fill_of(
            &update("65432.10", "0.002"),
            &Instrument::linear(2, 3),
            OrderId(7),
        )
        .expect("reads");
        assert_eq!(f.price.0, 6_543_210);
        assert_eq!(f.qty.0, 2);
        assert_eq!(
            f.order,
            OrderId(7),
            "the strategy's own id travels with the fill, or it cannot \
             recognise its own order filling"
        );
    }
}

/// Venue bars as the ticks a strategy would have seen.
///
/// Pulled out of `run` so the conversion is testable: `run` needs a
/// venue and cannot be called from a test, and the arithmetic here is
/// where a real mistake would hide — a per-bar volume left un-accumulated
/// would make every difference a consumer takes come out as the bar's
/// own volume rather than the change since the last observation.
fn warm_ticks(bars: &[oq_gateway::klines::Kline]) -> Vec<oq_engine::Tick> {
    let mut cumulative: i64 = 0;
    bars.iter()
        .map(|b| {
            // Accumulated, because that is the convention a live tick
            // carries: consumers read differences between consecutive
            // observations rather than the absolute value.
            cumulative = cumulative.saturating_add(b.volume);
            let at = Nanos(b.open_ms.saturating_mul(1_000_000));
            oq_engine::Tick {
                // Both stamps are the venue's. A bar fetched in bulk has
                // no arrival time, and inventing one would put a latency
                // in the record that no message experienced.
                stamp: oq_types::Stamp {
                    exch: at,
                    local: at,
                },
                last: PriceTicks(b.close),
                high: PriceTicks(b.high),
                low: PriceTicks(b.low),
                // A bar carries no book.
                bid: PriceTicks(0),
                ask: PriceTicks(0),
                volume: QtyLots(cumulative),
            }
        })
        .collect()
}

#[cfg(test)]
mod warm_up {
    use super::warm_ticks;
    use oq_gateway::klines::Kline;

    fn bar(open_ms: i64, close: i64, volume: i64) -> Kline {
        Kline {
            open_ms,
            high: close + 10,
            low: close - 10,
            close,
            volume,
        }
    }

    /// Volume accumulates, because that is what a live tick carries.
    ///
    /// A consumer reads the difference between consecutive observations.
    /// Feeding per-bar volume would make every difference come out as
    /// the bar's own volume — plausible numbers, and a volume gate that
    /// arms on the wrong thing.
    #[test]
    fn volume_accumulates_across_the_bars() {
        let t = warm_ticks(&[bar(0, 100, 5), bar(60_000, 101, 7), bar(120_000, 102, 3)]);
        assert_eq!(
            t.iter().map(|t| t.volume.0).collect::<Vec<_>>(),
            vec![5, 12, 15]
        );
        // And the differences a consumer takes are the bars' own volumes.
        let deltas: Vec<i64> = t
            .windows(2)
            .map(|w| w[1].volume.0 - w[0].volume.0)
            .collect();
        assert_eq!(deltas, vec![7, 3]);
    }

    /// Milliseconds to nanoseconds, on both stamps.
    #[test]
    fn the_stamp_is_the_venues_on_both_clocks() {
        let t = warm_ticks(&[bar(1_499_040_000_000, 100, 1)]);
        assert_eq!(t[0].stamp.exch.0, 1_499_040_000_000_000_000);
        assert_eq!(
            t[0].stamp.local.0, t[0].stamp.exch.0,
            "a bar fetched in bulk has no arrival time to report"
        );
    }

    /// A bar has no book, and says so rather than guessing one.
    #[test]
    fn there_is_no_book_in_a_bar() {
        let t = warm_ticks(&[bar(0, 100, 1)]);
        assert_eq!((t[0].bid.0, t[0].ask.0), (0, 0));
        assert_eq!(t[0].last.0, 100, "but the traded price is real");
        assert_eq!((t[0].high.0, t[0].low.0), (110, 90));
    }

    #[test]
    fn no_bars_is_no_ticks() {
        assert!(warm_ticks(&[]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::shape_of;
    use oq_strategy::Intent;
    use oq_types::{Offset, OrderId, PriceTicks, QtyLots, Side};

    /// What is **not** covered here, said rather than left to be
    /// assumed: that `run` calls `books.on_submit` and `shadow.apply`
    /// at all. That was the defect — `Books` has tested `working`
    /// since it was written and those tests passed the whole time,
    /// because the unit was always right and nothing invoked it.
    /// Reaching the call site needs a venue, and there is no way to
    /// build one in a test.
    fn market(id: u64, side: Side, qty: i64) -> Intent {
        Intent::Market {
            instrument: oq_types::InstrumentId::new(1),
            id: OrderId(id),
            side,
            qty: QtyLots(qty),
            offset: Offset::Open,
        }
    }

    #[test]
    fn an_outcome_is_matched_back_to_what_was_asked_for() {
        let intents = vec![
            market(1, Side::Buy, 3),
            market(2, Side::Sell, 7),
            Intent::Cancel(OrderId(1)),
        ];
        assert_eq!(
            shape_of(&intents, OrderId(2)),
            Some((Side::Sell, QtyLots(7), Offset::Open))
        );
    }

    /// A limit order carries its shape too, and the price is not part
    /// of it: what the books need from a submission is that an order
    /// exists, and a live order's resting price is the venue's.
    #[test]
    fn a_limit_order_is_matched_by_id_and_not_by_price() {
        let intents = vec![Intent::Limit {
            instrument: oq_types::InstrumentId::new(1),
            id: OrderId(9),
            side: Side::Buy,
            price: PriceTicks(6_000_000),
            qty: QtyLots(2),
            offset: Offset::Close,
        }];
        assert_eq!(
            shape_of(&intents, OrderId(9)),
            Some((Side::Buy, QtyLots(2), Offset::Close))
        );
    }

    /// A cancel submits nothing, and an id nobody asked for is not
    /// invented.
    ///
    /// Returning a default here would tell the kernel an order exists
    /// with a side and a size that came from nowhere, which is worse
    /// than the zero this replaces.
    #[test]
    fn a_cancel_and_an_unknown_id_produce_nothing() {
        let intents = vec![Intent::Cancel(OrderId(4)), Intent::CancelAll];
        assert_eq!(shape_of(&intents, OrderId(4)), None);
        assert_eq!(shape_of(&intents, OrderId(99)), None);
    }
}

/// Whether a fill opened or closed, from the leg it landed on.
///
/// On a hedged account the venue names the leg and the answer follows:
/// a sell reduces the long and opens the short. On a one-way account it
/// says `BOTH`, and the engine reads the effect from the sign instead.
fn offset_of(position_side: &str, side: &str) -> oq_types::Offset {
    let sell = side.eq_ignore_ascii_case("SELL");
    if position_side.eq_ignore_ascii_case("LONG") {
        if sell {
            oq_types::Offset::Close
        } else {
            oq_types::Offset::Open
        }
    } else if position_side.eq_ignore_ascii_case("SHORT") {
        if sell {
            oq_types::Offset::Open
        } else {
            oq_types::Offset::Close
        }
    } else {
        oq_types::Offset::Open
    }
}

#[cfg(test)]
mod which_leg {
    use super::offset_of;
    use oq_types::Offset;

    /// A sell reduces the long leg and opens the short one.
    ///
    /// Reading both as opening is what left a position the books never
    /// let go of, and a strategy trying to close it over and over.
    #[test]
    fn a_sell_closes_the_long_and_opens_the_short() {
        assert_eq!(offset_of("LONG", "SELL"), Offset::Close);
        assert_eq!(offset_of("SHORT", "SELL"), Offset::Open);
    }

    #[test]
    fn a_buy_opens_the_long_and_closes_the_short() {
        assert_eq!(offset_of("LONG", "BUY"), Offset::Open);
        assert_eq!(offset_of("SHORT", "BUY"), Offset::Close);
    }

    /// A one-way account nets, and the engine reads the sign.
    #[test]
    fn a_netting_account_is_read_as_opening() {
        assert_eq!(offset_of("BOTH", "SELL"), Offset::Open);
        assert_eq!(offset_of("BOTH", "BUY"), Offset::Open);
    }

    /// Case is the venue's business, not this build's.
    #[test]
    fn the_venues_spelling_does_not_matter() {
        assert_eq!(offset_of("long", "sell"), Offset::Close);
    }
}

/// A decimal string as an integer count at `scale`.
///
/// The journal writes prices and quantities as text, at the precision
/// the instrument had when they were written. Reading them back needs
/// that precision, which is why this is not done where the records are
/// decoded: a scale guessed there is a position wrong by a factor.
fn scaled_decimal(text: &str, scale: u8) -> Option<i64> {
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    let mut digits = String::from(int.trim_start_matches('+'));
    let frac: String = frac.chars().take(usize::from(scale)).collect();
    digits.push_str(&frac);
    for _ in frac.len()..usize::from(scale) {
        digits.push('0');
    }
    digits.parse::<i64>().ok()
}

#[cfg(test)]
mod adoption_rounding {
    use super::adopted_lots;
    use oq_gateway::binance::PositionSnapshot;
    use oq_types::Instrument;

    fn leg(amount: &str, entry: &str) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: "LONG".into(),
            amount: amount.parse().unwrap_or(0.0),
            amount_text: amount.into(),
            entry_text: entry.into(),
            entry_price: entry.parse().unwrap_or(0.0),
            unrealized: 0.0,
        }
    }

    /// A size that a float cannot hold exactly is still adopted whole.
    ///
    /// The venue reports decimal text and the float that comes back is
    /// short of the number by a hair: 0.0058 at four places is
    /// 57.999999999999993. Truncating adopts fifty-seven of fifty-eight
    /// lots, and the one left over is held by the account and managed by
    /// nobody — still there after the position closes.
    ///
    /// Seen on a live takeover, which reported `adopted long 57 lots`
    /// against a venue holding 0.0058.
    #[test]
    fn a_size_a_float_cannot_hold_exactly_is_adopted_whole() {
        let got = adopted_lots(&[leg("0.0058", "71774.66")], &Instrument::linear(2, 4));
        assert_eq!(got[0].1.0, 58, "0.0058 at four decimal places is 58 lots");
    }

    /// And the entry price the same way.
    #[test]
    fn an_entry_price_is_rounded_and_not_truncated() {
        let got = adopted_lots(&[leg("0.0020", "70097.90")], &Instrument::linear(2, 4));
        assert_eq!(got[0].2.0, 7_009_790);
    }

    /// Every size at this precision, not the ones somebody tested.
    ///
    /// The float path was right for most and wrong for some, which is
    /// the hardest kind of wrong to find. Reading the digits has no
    /// "some".
    #[test]
    fn every_size_at_this_precision_converts_exactly() {
        let i = Instrument::linear(2, 4);
        for lots in 1..=2_000 {
            let text = format!("{}.{:04}", lots / 10_000, lots % 10_000);
            let got = adopted_lots(&[leg(&text, "70097.90")], &i);
            assert_eq!(got[0].1.0, lots, "{text} should be {lots} lots");
        }
    }
}
