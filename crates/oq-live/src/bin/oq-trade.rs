//! `oq-trade` — run a strategy against a venue.
//!
//! ```text
//! OQ_VENUE_KEY=… OQ_VENUE_SECRET=… oq-trade --symbol BTCUSDT --minutes 5
//! ```
//!
//! Market data in, ticks out, a strategy's intents through the gate and
//! onto the venue, fills back on the account's own stream. It is the
//! assembly; every part of it is tested on its own elsewhere, and this
//! is where they have to agree.
//!
//! # Testnet unless told otherwise, and told loudly
//!
//! `--live` needs `OQ_ALLOW_LIVE=i-understand` in the environment as
//! well. Two independent gestures, because one is a flag somebody can
//! leave in a shell history and press up-arrow into.
//!
//! # An existing position has to be acknowledged, not assumed
//!
//! By default any open position stops the run, because risk limits
//! computed against a picture that is already wrong are not limits.
//! `--adopt-existing` is how an operator says they know: it declares
//! whatever the venue holds and starts beside it, and the gate is then
//! shown that position rather than a zero. The flag exists so the
//! acknowledgement is a deliberate act; the check exists so that
//! forgetting is not silent.
//!
//! # The strategies here are not strategies
//!
//! `observe` never trades: it proves the whole loop — connection,
//! ticks, stream, reconciliation, upkeep — without exposure. `probe`
//! places one limit order far below the market and cancels it, which
//! exercises the order path against live prices. Neither has an edge
//! and neither pretends to; a framework should not ship something that
//! looks like a trading idea.

use core::time::Duration;
use std::process::ExitCode;
use std::time::Instant;

use oq_gateway::binance::Binance;
use oq_gateway::exec::Endpoint;
use oq_gateway::{Credentials, StreamOutcome, UserEvent, UserStreamReader};
use oq_ingest::Aggregator;
use oq_l2feed::session::{install_signal_handlers, now_ns, shutdown_requested};
use oq_l2feed::venue::Deployment;
use oq_live::{
    Action, MarketData, Outcome, Position, Session, SessionConfig, Supervisor, Timings, Trader,
};
use oq_risk::{Limits, RiskGate};
use oq_strategy::{Context, Intent, Strategy};
use oq_types::{Cash, Instrument, Nanos, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side};

const USAGE: &str = "\
oq-trade — run a strategy against a venue

USAGE:
    OQ_VENUE_KEY=<key> OQ_VENUE_SECRET=<secret> oq-trade [OPTIONS]

OPTIONS:
    --symbol <SYMBOL>      Contract [default: BTCUSDT]
    --strategy <NAME>      observe | probe [default: observe]
    --window-ms <MS>       Tick width [default: 1000]
    --minutes <N>          Stop after this long [default: 5]
    --max-qty <LOTS>       Largest single order [default: 1]
    --max-position <LOTS>  Largest position [default: 1]
    --max-notional <USDT>  Largest order notional [default: 200]
    --band-bps <BPS>       How far a limit may sit from the mark [default: 3000]
    --journal <PATH>       Where to record decisions [default: oq-trade.oqj]
    --no-journal           Trade without recording. Nothing can be replayed
    --adopt-existing       Start beside a position the venue already holds
    --live                 Trade with real money; needs OQ_ALLOW_LIVE=i-understand
    --help
";

/// Never trades. Proves the loop.
struct Observe {
    ticks: u64,
}

impl Strategy for Observe {
    fn on_tick(&mut self, _ctx: &Context, _out: &mut Vec<Intent>) {
        self.ticks += 1;
    }
    fn name(&self) -> &str {
        "observe"
    }
}

/// Places an order far from the market, cancels it, waits, repeats.
///
/// Cyclic on purpose. A single pass proves the order path once; a long
/// run of single passes proves it once and then watches an idle socket
/// for an hour. Repeating exercises placement, the account stream, the
/// cancel path and the id map continuously, and because every order rests
/// far below the market and is withdrawn, the account never carries a
/// position from it.
struct Probe {
    placed: bool,
    cancelled: bool,
    ticks: u64,
    /// Ticks to wait after a cancel before placing again.
    idle_ticks: u64,
    /// Tick count at the last cancel.
    cancelled_at: u64,
    /// Completed cycles, for the closing report.
    cycles: u64,
    /// Strategy-side id, bumped per cycle so a stale confirmation cannot
    /// be mistaken for the current order's.
    next_id: u64,
    /// How far below the market to rest, in parts per ten thousand.
    away_bps: i64,
    /// The contract, for sizing against its floor and its grid.
    ///
    /// A probe that hardcodes one lot is a probe that gets refused for
    /// notional on every contract whose lot is small, which is most of
    /// them. Measured: 0.001 of one contract was worth about three units
    /// of quote against a floor of twenty.
    instrument: Instrument,
}

impl Strategy for Probe {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.ticks += 1;
        let reference = if ctx.tick.bid.0 > 0 {
            ctx.tick.bid
        } else {
            ctx.tick.last
        };
        if reference.0 <= 0 {
            return;
        }
        // Rest a while after a cancel, so the run is a sequence of
        // cycles rather than a tight loop against the venue's rate
        // limit.
        if self.cancelled && self.ticks.saturating_sub(self.cancelled_at) >= self.idle_ticks {
            self.placed = false;
            self.cancelled = false;
            self.cycles += 1;
            self.next_id += 1;
        }
        if !self.placed {
            self.placed = true;
            let price = self.instrument.snap_price_down(PriceTicks(
                reference.0 - reference.0 * self.away_bps / 10_000,
            ));
            out.push(Intent::Limit {
                id: OrderId(self.next_id),
                side: Side::Buy,
                price,
                qty: self
                    .instrument
                    .snap_qty_up(smallest_allowed(&self.instrument, price)),
                offset: Offset::Open,
            });
        } else if !self.cancelled && self.ticks.saturating_sub(self.cancelled_at) > 5 {
            self.cancelled = true;
            self.cancelled_at = self.ticks;
            out.push(Intent::Cancel(OrderId(self.next_id)));
        }
    }
    fn name(&self) -> &str {
        "probe"
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let number = |flag: &str, default: i64| -> i64 {
        value(flag).and_then(|v| v.parse().ok()).unwrap_or(default)
    };

    let live = args.iter().any(|a| a == "--live");
    if live && std::env::var("OQ_ALLOW_LIVE").as_deref() != Ok("i-understand") {
        eprintln!(
            "oq-trade: --live also needs OQ_ALLOW_LIVE=i-understand in the environment.\n\
             Two gestures rather than one, because a flag is something you can press\n\
             up-arrow into."
        );
        return ExitCode::FAILURE;
    }
    let (endpoint, deployment) = if live {
        (Endpoint::Live, Deployment::Live)
    } else {
        (Endpoint::Testnet, Deployment::Testnet)
    };

    let symbol = value("--symbol").unwrap_or_else(|| "BTCUSDT".to_string());
    let strategy_name = value("--strategy").unwrap_or_else(|| "observe".to_string());
    let window_ns = number("--window-ms", 1000) * 1_000_000;
    let minutes = number("--minutes", 5);

    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oq-trade: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("deployment       {deployment:?}");
    println!("symbol           {symbol}");
    println!("strategy         {strategy_name}");

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
    let resting: Vec<String> = match venue.open_orders(&symbol) {
        Ok(o) => o.into_iter().map(|o| o.client_order_id).collect(),
        Err(e) => {
            eprintln!("open orders      FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    let limits = Limits {
        max_order_qty: QtyLots(number("--max-qty", 1)),
        max_position_qty: QtyLots(number("--max-position", 1)),
        max_order_notional: Cash(number("--max-notional", 200) * oq_types::CASH_SCALE),
        // Basis points to parts per billion.
        price_band: Ratio(number("--band-bps", 3000) * 100_000),
        max_working: 4,
        max_rate: 10,
        rate_window: Nanos(60 * 1_000_000_000),
    };

    let config = SessionConfig {
        symbol: symbol.clone(),
        instrument,
        position_side: if hedged {
            oq_gateway::PositionSide::Long
        } else {
            oq_gateway::PositionSide::OneWay
        },
        id_prefix: format!("oq{}", std::process::id()),
    };

    // Nothing is declared unless the operator says otherwise, so any
    // position at all stops the run. `--adopt-existing` is that saying:
    // it declares what the venue holds, and the gate is then shown that
    // position rather than a zero.
    let adopt = args.iter().any(|a| a == "--adopt-existing");
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
    let journal_path = value("--journal").unwrap_or_else(|| "oq-trade.oqj".to_string());
    let no_journal = args.iter().any(|a| a == "--no-journal");

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

    let mut trader: Box<dyn TraderLike> = match strategy_name.as_str() {
        "observe" => Box::new(Trader::new(Observe { ticks: 0 }, session)),
        "probe" => Box::new(Trader::new(
            Probe {
                placed: false,
                cancelled: false,
                ticks: 0,
                idle_ticks: 30,
                cancelled_at: 0,
                cycles: 0,
                next_id: 1,
                away_bps: 2000,
                instrument,
            },
            session,
        )),
        other => {
            eprintln!("oq-trade: unknown strategy {other:?}; known: observe, probe");
            return ExitCode::FAILURE;
        }
    };

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
                        let ctx = Context {
                            tick,
                            position: QtyLots(0),
                            entry: PriceTicks(0),
                            short_position: QtyLots(0),
                            short_entry: PriceTicks(0),
                            equity: Cash(0),
                            working: trader.working() as usize,
                        };
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
trait TraderLike {
    fn on_tick(&mut self, ctx: &Context, now: Nanos) -> Vec<Outcome>;
    fn apply(&mut self, u: &oq_gateway::OrderUpdate) -> bool;
    fn forget(&mut self, client_id: &str);
    fn working(&self) -> u32;
    fn duplicates(&self) -> u64;
    fn foreign(&self) -> u64;
    fn record_tick(&mut self, tick: &oq_engine::Tick);
    fn cancel_all(&mut self, symbol: &str);
    fn close_stream(&self) -> Result<(), oq_gateway::VenueError>;
    fn reconcile(&mut self, symbol: &str);
    fn renew(&self);
    fn halt(&self, why: &str);
}

impl<S: Strategy> TraderLike for Trader<S, Binance> {
    fn on_tick(&mut self, ctx: &Context, now: Nanos) -> Vec<Outcome> {
        Trader::on_tick(self, ctx, now)
    }
    fn apply(&mut self, u: &oq_gateway::OrderUpdate) -> bool {
        self.session_mut().apply(u)
    }
    fn forget(&mut self, client_id: &str) {
        Trader::forget(self, client_id);
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

fn act(action: &Action, trader: &mut Box<dyn TraderLike>, symbol: &str) {
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
fn smallest_allowed(instrument: &Instrument, price: PriceTicks) -> QtyLots {
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
