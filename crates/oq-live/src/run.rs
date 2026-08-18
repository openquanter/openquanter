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

use oq_gateway::binance::Binance;
use oq_gateway::exec::{Endpoint, Execution};
use oq_gateway::{Credentials, StreamOutcome, UserEvent, UserStreamReader};
use oq_ingest::Aggregator;
use oq_l2feed::session::{install_signal_handlers, now_ns, shutdown_requested};
use oq_l2feed::venue::Deployment;
use oq_risk::{Limits, RiskGate};
use oq_strategy::Strategy;
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
    pub endpoint: Endpoint,
    pub deployment: Deployment,
    pub minutes: i64,
    pub window_ms: i64,
    pub id_prefix: String,
    pub adopt_existing: bool,
    /// `None` means run without one, which is `--no-journal`.
    pub journal: Option<String>,
    pub limits: Limits,
}

/// Run one strategy against one venue until the clock or a signal ends it.
///
/// The strategy is built by a closure rather than passed in, because the
/// instrument is discovered here — precision and grid come from the
/// deployment being traded, and a strategy that needs them cannot be
/// constructed before this function has asked.
pub fn run<S, F>(make_strategy: F, cfg: &RunConfig) -> ExitCode
where
    S: Strategy,
    F: FnOnce(&Instrument) -> S,
{
    // Bound locally so the body below is the code that was in `main`,
    // unchanged. Rewriting every use to `cfg.x` would have edited eight
    // hundred lines to move them, and a move that edits is not a move.
    let symbol = cfg.symbol.clone();
    let endpoint = cfg.endpoint;
    let deployment = cfg.deployment;
    let minutes = cfg.minutes;
    let window_ns = cfg.window_ms * 1_000_000;

    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oq-trade: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("deployment       {deployment:?}");
    println!("symbol           {symbol}");
    println!("strategy         {}", cfg.strategy_name);

    let mut venue = Binance::at(endpoint, creds);
    if let Err(e) = venue.sync_clock() {
        eprintln!("clock            FAILED: {e}");
        return ExitCode::FAILURE;
    }

    // Market data first: it decides the precision and grid that the
    // order path has to respect, and connecting it before anything is
    // sent means a feed that will not open stops the run before it
    // trades rather than after.
    let (mut market, feed_venue) = match MarketData::open(
        "binance-perp",
        deployment,
        &symbol,
        Duration::from_millis(200),
    ) {
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
    let instrument = match instrument_of(&venue, &symbol) {
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

    let hedged = match venue.is_hedged_account() {
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
    let starting_balance = match venue.account() {
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
    let id_prefix = cfg.id_prefix.clone();
    let config_prefix = id_prefix.clone();

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
        .unwrap_or_else(|| "oq-trade.oqj".to_string());
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
    let mut books = crate::books::Books::new(
        oq_types::InstrumentId::new(1),
        oq_margin::Contract::new(10_000),
        oq_margin::TierTable::example_btcusdt(),
        // The venue's number, not a configured one. Books opened at a
        // balance nobody read would report an account that is not this.
        starting_balance,
    );
    for p in &positions {
        let amount = p.amount;
        if amount != 0.0 {
            #[allow(clippy::cast_possible_truncation)]
            let lots = oq_types::QtyLots(
                (amount.abs() * 10f64.powi(i32::from(instrument.qty_scale))) as i64,
            );
            #[allow(clippy::cast_possible_truncation)]
            let entry = oq_types::PriceTicks(
                (p.entry_price * 10f64.powi(i32::from(instrument.price_scale))) as i64,
            );
            let side = if amount > 0.0 { Side::Buy } else { Side::Sell };
            books.adopt(side, lots, entry, Nanos(now_ns()));
            println!(
                "adopted          {} {} lots at {}",
                if amount > 0.0 { "long" } else { "short" },
                lots.0,
                entry.0
            );
        }
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

    install_signal_handlers();
    let deadline = Instant::now() + Duration::from_secs(60 * u64::try_from(minutes).unwrap_or(5));
    println!("running          until {minutes} minutes elapse or a signal arrives");
    println!();

    let mut ticks = 0_u64;
    let mut sent = 0_u64;
    let mut cancelled = 0_u64;
    let mut last_tick_report = Instant::now();

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
                        for output in books.on_tick(&tick) {
                            // Under venue matching the kernel does not
                            // fill, so anything here is the account
                            // going past its maintenance requirement —
                            // which is worth a line rather than a
                            // silence.
                            println!("books            {output:?}");
                        }
                        let ctx = books.context(tick);
                        for outcome in trader.on_tick(&ctx, now) {
                            match &outcome {
                                Outcome::Sent { .. } => sent += 1,
                                Outcome::Cancelled { .. } => cancelled += 1,
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
                trader.apply(&u);
                if let Some(fill) = fill_of(&u, &instrument) {
                    match books.on_venue_fill(&fill) {
                        crate::books::Booked::Applied(outputs) => {
                            for output in outputs {
                                println!("books            {output:?}");
                            }
                        }
                        // Routine after a reconnect, and worth a line:
                        // a stream repeating itself is a fact about the
                        // link, and silence would hide how often.
                        crate::books::Booked::Duplicate => {
                            println!("books            trade {} already booked", fill.trade.0);
                        }
                        crate::books::Booked::Unidentifiable => {
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
            println!(
                "heartbeat        {ticks} ticks, {} resting",
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
    println!("orders           {sent} placed, {cancelled} withdrawn");
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
    fn latency(&self) -> String;
    fn cancel_all(&mut self, symbol: &str);
    fn close_stream(&self) -> Result<(), oq_gateway::VenueError>;
    fn reconcile(&mut self, symbol: &str);
    fn renew(&self);
    fn halt(&self, why: &str);
}

impl<S: Strategy> TraderLike for Trader<S, Binance> {
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

/// Precision and grid for `symbol`, from this deployment.
fn instrument_of(venue: &Binance, symbol: &str) -> Result<oq_types::Instrument, String> {
    let body = venue.exchange_info(symbol).map_err(|e| e.to_string())?;
    let price_scale = integer_field(&body, "pricePrecision").ok_or("no pricePrecision")?;
    let qty_scale = integer_field(&body, "quantityPrecision").ok_or("no quantityPrecision")?;
    let price_scale = u8::try_from(price_scale).map_err(|_| "implausible price precision")?;
    let qty_scale = u8::try_from(qty_scale).map_err(|_| "implausible quantity precision")?;
    let tick = decimal_field(&body, "tickSize", price_scale).unwrap_or(1);
    let step = decimal_field(&body, "stepSize", qty_scale).unwrap_or(1);
    // The venue also refuses orders below a notional floor, and its
    // message names the floor without naming what the order was worth.
    // Carried on the instrument so a strategy does not learn it by
    // being refused.
    let floor = decimal_field(&body, "notional", 8).unwrap_or(0);
    Ok(oq_types::Instrument::linear(price_scale, qty_scale)
        .with_grid(tick, step)
        .with_min_notional(Cash(floor)))
}

/// An unquoted integer field.
fn integer_field(body: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// A quoted decimal string as an integer count at `scale`.
fn decimal_field(body: &str, key: &str, scale: u8) -> Option<i64> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let text = &rest[..rest.find('"')?];
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let mut digits = String::from(whole);
    let width = usize::from(scale);
    let mut frac = frac.to_string();
    frac.truncate(width);
    while frac.len() < width {
        frac.push('0');
    }
    digits.push_str(&frac);
    digits.parse().ok()
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
fn fill_of(u: &oq_gateway::OrderUpdate, instrument: &Instrument) -> Option<oq_types::Fill> {
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

    let qty = scaled(&u.last_qty, instrument.qty_scale)?;
    if qty <= 0 {
        return None;
    }
    let price = scaled(&u.last_price, instrument.price_scale)?;
    if price <= 0 {
        // A fill with no price is a report this build cannot book, and
        // booking it at zero would price the position at nothing.
        return None;
    }

    Some(oq_types::Fill {
        stamp: oq_types::Stamp::new(now_ns(), now_ns()),
        instrument: oq_types::InstrumentId::new(1),
        order: OrderId(0),
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
