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
use oq_strategy::{Context, Strategy};
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

/// Run one strategy against one venue until the clock or a signal ends it.
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

    let limits = Limits {
        max_order_qty: cfg.limits.max_order_qty,
        max_position_qty: cfg.limits.max_position_qty,
        max_order_notional: cfg.limits.max_order_notional,
        // Basis points to parts per billion.
        price_band: cfg.limits.price_band,
        max_working: 4,
        max_rate: 10,
        rate_window: Nanos(60 * 1_000_000_000),
    };

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

    // Stamped before the loop so the fee query at the end covers exactly
    // this run. Asking for "everything" would sum a previous run's fees
    // into this one's attribution, which is the sort of number that
    // looks plausible and is somebody else's.
    let started_ms = now_ns() / 1_000_000;
    install_signal_handlers();
    let deadline = Instant::now() + Duration::from_secs(60 * u64::try_from(minutes).unwrap_or(5));
    println!("running          until {minutes} minutes elapse or a signal arrives");
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
    let mut last_tick_report = Instant::now();
    // The last observation, kept so a fill arriving between ticks can be
    // handed a context. A strategy sizing a ladder needs a price, and the
    // most recent one is the honest answer: the alternative is telling it
    // nothing happened until the next tick, by which time the position it
    // is managing has been unmanaged for however long that took.
    let mut last_tick: Option<oq_engine::Tick> = None;

    while Instant::now() < deadline && !shutdown_requested() {
        let now = Nanos(now_ns());

        // Market data.
        for which in [0_u8, 1] {
            let stream = if which == 0 {
                market.depth()
            } else {
                market.trade()
            };
            match stream.poll() {
                Ok(Some(bytes)) => {
                    let at = event_time(&bytes).unwrap_or(now.0);
                    let closed = if which == 0 {
                        feed_venue
                            .parse_depth(&bytes, scales)
                            .ok()
                            .and_then(|u| agg.on_depth(at, now.0, &u))
                    } else {
                        feed_venue
                            .parse_trade(&bytes, scales)
                            .and_then(|t| agg.on_trade(at, now.0, &t))
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
                                _ => {}
                            }
                            report(&outcome);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("{} stream lost: {e}", stream.name()),
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
                if matches!(u.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED") {
                    trader.forget(&u.client_id);
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
            }
            StreamOutcome::Idle | StreamOutcome::Ignored => {}
        }

        // Upkeep that time makes due, whether or not anything arrived.
        for action in supervisor.due(now) {
            act(&action, &mut trader, &symbol);
        }

        if last_tick_report.elapsed() >= Duration::from_secs(30) {
            last_tick_report = Instant::now();
            // Sampled and recorded together, so what a reader sees in
            // the terminal and what a replay sees in the journal are the
            // same observation rather than two that happen to agree.
            let now_ns = Nanos(now_ns());
            trader.record_waiting(now_ns);
            let waiting = trader.waiting_summary();
            println!(
                "heartbeat        {ticks} ticks, {} resting{waiting}",
                trader.working()
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
        Outcome::Unresolved { local, why } => println!(
            "UNRESOLVED       {local:?}: {why} — this order may be resting; do not replace it"
        ),
        Outcome::Cancelled { local, client_id } => {
            println!("cancelled        {local:?} ({client_id})");
        }
        Outcome::UnknownOrder(id) => println!("unknown order    {id:?} — not in this run's map"),
    }
}

/// The smallest quantity whose notional clears the contract's floor.
///
/// A tenth over it rather than exactly it, because the floor is checked
/// against a price that can move between sizing and arrival, and landing
/// exactly on a minimum is landing under it half the time.
/// The smallest quantity this venue will accept at this price.
///
/// Public because a strategy needs it: the venue enforces a minimum
/// notional as well as a step, and a size that satisfies the step alone
/// is rejected. Discovered on a live run, not in a unit test.
pub fn smallest_allowed(instrument: &Instrument, price: PriceTicks) -> QtyLots {
    if instrument.min_notional.0 <= 0 || price.0 <= 0 {
        return QtyLots(1);
    }
    let Some(tick_cash) = instrument.tick_cash() else {
        return QtyLots(1);
    };
    let per_lot = i128::from(price.0) * i128::from(tick_cash);
    if per_lot <= 0 {
        return QtyLots(1);
    }
    let need = i128::from(instrument.min_notional.0) * 11 / 10;
    let lots = (need + per_lot - 1) / per_lot;
    QtyLots(i64::try_from(lots).unwrap_or(1).max(1))
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
        // The venue tells us what it filled, not whether this process
        // considered it an opening trade. Reduce-only would say so and
        // this build does not read it, so the safe reading is that it
        // opens — a close mistaken for an open overstates the position,
        // which the reconciler then catches, while the reverse would
        // quietly cancel a position that is still there.
        offset: oq_types::Offset::Open,
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
            #[allow(clippy::cast_possible_truncation)]
            let lots =
                QtyLots((p.amount.abs() * 10f64.powi(i32::from(instrument.qty_scale))) as i64);
            #[allow(clippy::cast_possible_truncation)]
            let entry =
                PriceTicks((p.entry_price * 10f64.powi(i32::from(instrument.price_scale))) as i64);
            let side = if p.amount > 0.0 {
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

#[cfg(test)]
mod adoption {
    use super::{adopted_legs, adopted_lots};
    use oq_gateway::binance::PositionSnapshot;
    use oq_types::{Instrument, Side};

    fn leg(amount: f64, entry: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: if amount > 0.0 { "LONG" } else { "SHORT" }.into(),
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
