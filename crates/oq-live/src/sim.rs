//! A venue, a market and a clock in one process, for running the live
//! loop deterministically.
//!
//! [`run_on`](crate::run::run_on) reads the world through an
//! [`Environment`] and the venue through an [`Account`]. This supplies
//! both from one seeded core: a random-walk market published on the
//! venue's own wire format and read by the venue's real parser, an
//! exchange that rests and fills orders against it, and an account stream
//! that reports what the exchange did. Time is a [`ManualClock`] that
//! moves only when the loop has nothing to read, so a run of an hour
//! takes the time its work takes and a seed names one run exactly.
//!
//! What it is for is the property no unit test reaches: the whole loop —
//! startup, adoption, the market and account streams, the strategy, the
//! risk gate, the journal, shutdown — doing the right thing end to end,
//! and doing the same thing every time the same seed is run.
//!
//! The market is deliberately plain: a price that moves a few ticks at a
//! time, a book one level deep on each side, and trades at the price.
//! Nothing here claims to model a venue's matching; it models one well
//! enough that orders rest, fill and cancel the way the loop expects.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use oq_gateway::account::{Account, AccountTrade};
use oq_gateway::broker::IdRules;
use oq_gateway::exec::{
    Events, Execution, Initiator, NewOrder, OrderAck, OrderUpdate, Placed, Reject, UserEvent,
    UserStream, decimal,
};
use oq_gateway::klines::Kline;
use oq_gateway::{AccountSnapshot, OpenOrder, PositionSnapshot, StreamOutcome, VenueError};
use oq_l2feed::session::{Connector, MessageSource};
use oq_l2feed::venue::{Deployment, Venue};
use oq_types::{Instrument, Nanos, Side};

use crate::clock::{Clock, ManualClock};
use crate::env::{Environment, UserEvents};
use crate::feed::{AnyConnector, MarketData, Stream};

/// How a simulated run is set up.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Names the run: the same seed makes the same market and the same
    /// exchange decisions.
    pub seed: u64,
    pub symbol: String,
    /// Where the price starts, in ticks.
    pub start_price: i64,
    /// Largest move between two market events, in ticks.
    pub max_step: i64,
    /// Time between market events.
    pub market_every: Duration,
    /// How far the clock moves when the loop finds nothing to read.
    pub idle_step: Duration,
    /// Starting wallet, in account currency.
    pub balance: f64,
    /// Where durable state goes; a fresh directory per run.
    pub state_root: PathBuf,
}

/// An order resting at the simulated venue.
#[derive(Debug, Clone)]
struct Resting {
    venue_id: u64,
    side: Side,
    price: i64,
    qty: i64,
}

/// An order the venue has finished with, for status queries.
#[derive(Debug, Clone)]
struct Finished {
    venue_id: u64,
    status: &'static str,
    filled: i64,
}

#[derive(Debug)]
struct Core {
    cfg: SimConfig,
    instrument: Instrument,
    clock: ManualClock,
    rng: oq_sim::Rng,
    price: i64,
    next_market: Duration,
    update_id: u64,
    trade_id: i64,
    depth: VecDeque<(Duration, Vec<u8>)>,
    trades: VecDeque<(Duration, Vec<u8>)>,
    resting: BTreeMap<String, Resting>,
    finished: BTreeMap<String, Finished>,
    next_venue_id: u64,
    /// Signed net position, in lots, and its average entry in ticks.
    position: i64,
    entry: f64,
    realized: f64,
    events: VecDeque<UserEvent>,
    /// Every client id ever sent, in order, for the invariants.
    placed: Vec<String>,
}

impl Core {
    fn new(cfg: SimConfig) -> Self {
        let wall = Nanos(1_788_220_800_000_000_000);
        let rng = oq_sim::Rng::new(cfg.seed.max(1));
        Self {
            price: cfg.start_price,
            instrument: Instrument::linear(2, 3),
            clock: ManualClock::starting_at(wall),
            rng,
            next_market: Duration::ZERO,
            update_id: 1_000,
            trade_id: 1,
            depth: VecDeque::new(),
            trades: VecDeque::new(),
            resting: BTreeMap::new(),
            finished: BTreeMap::new(),
            next_venue_id: 1,
            position: 0,
            entry: 0.0,
            realized: 0.0,
            events: VecDeque::new(),
            placed: Vec::new(),
            cfg,
        }
    }

    fn now_ms(&self) -> i64 {
        self.clock.wall().0 / 1_000_000
    }

    fn px(&self, ticks: i64) -> String {
        decimal(ticks, self.instrument.price_scale)
    }

    fn qty(&self, lots: i64) -> String {
        decimal(lots, self.instrument.qty_scale)
    }

    /// Publish every market event due by now.
    fn advance(&mut self) {
        let now = self.clock.elapsed();
        while self.next_market <= now {
            let at = self.next_market;
            let span = u64::try_from(2 * self.cfg.max_step + 1).unwrap_or(1);
            let step = i64::try_from(self.rng.below(span)).unwrap_or(0) - self.cfg.max_step;
            self.price = (self.price + step).max(1);
            let ms = self.clock.wall().0 / 1_000_000
                - i64::try_from((now - at).as_millis()).unwrap_or(0);
            let first = self.update_id + 1;
            self.update_id += 3;
            let depth = format!(
                r#"{{"e":"depthUpdate","E":{ms},"T":{ms},"s":"{}","U":{first},"u":{},"pu":{},"b":[["{}","{}"]],"a":[["{}","{}"]]}}"#,
                self.cfg.symbol,
                self.update_id,
                first - 1,
                self.px(self.price - 1),
                self.qty(5_000),
                self.px(self.price + 1),
                self.qty(5_000),
            );
            self.depth.push_back((at, depth.into_bytes()));
            self.trade_id += 1;
            let trade = format!(
                r#"{{"e":"trade","E":{ms},"T":{ms},"s":"{}","t":{},"p":"{}","q":"{}","X":"MARKET","m":{}}}"#,
                self.cfg.symbol,
                self.trade_id,
                self.px(self.price),
                self.qty(1),
                step < 0,
            );
            self.trades.push_back((at, trade.into_bytes()));
            self.cross(self.price);
            self.next_market += self.cfg.market_every;
        }
    }

    /// Fill every resting order the traded price reached.
    fn cross(&mut self, price: i64) {
        let reached: Vec<String> = self
            .resting
            .iter()
            .filter(|(_, o)| match o.side {
                Side::Buy => price <= o.price,
                Side::Sell => price >= o.price,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in reached {
            if let Some(o) = self.resting.remove(&id) {
                self.fill(&id, o.venue_id, o.side, o.price, o.qty, true);
            }
        }
    }

    fn fill(
        &mut self,
        client_id: &str,
        venue_id: u64,
        side: Side,
        price: i64,
        qty: i64,
        maker: bool,
    ) {
        let signed = if side == Side::Buy { qty } else { -qty };
        let before = self.position;
        let after = before + signed;
        if before == 0 || (before > 0) == (signed > 0) {
            let total = self.entry * before.abs() as f64 + price as f64 * qty as f64;
            self.entry = total / after.abs() as f64;
        } else {
            let closed = qty.min(before.abs());
            let per = if before > 0 {
                price as f64 - self.entry
            } else {
                self.entry - price as f64
            };
            self.realized += per * closed as f64;
            if after != 0 && (after > 0) != (before > 0) {
                self.entry = price as f64;
            }
        }
        self.position = after;
        if self.position == 0 {
            self.entry = 0.0;
        }
        self.trade_id += 1;
        self.finished.insert(
            client_id.to_string(),
            Finished {
                venue_id,
                status: "FILLED",
                filled: qty,
            },
        );
        self.events.push_back(UserEvent::Order(OrderUpdate {
            symbol: self.cfg.symbol.clone(),
            client_id: client_id.to_string(),
            venue_id: venue_id.to_string(),
            status: "FILLED".into(),
            last_qty: self.qty(qty),
            cumulative_qty: self.qty(qty),
            last_price: self.px(price),
            side: if side == Side::Buy { "BUY" } else { "SELL" }.into(),
            position_side: "BOTH".into(),
            maker,
            trade_id: Some(self.trade_id),
            event_ms: self.now_ms(),
            initiator: Initiator::Account,
        }));
    }

    fn update(&self, client_id: &str, venue_id: u64, side: Side, status: &str) -> UserEvent {
        UserEvent::Order(OrderUpdate {
            symbol: self.cfg.symbol.clone(),
            client_id: client_id.to_string(),
            venue_id: venue_id.to_string(),
            status: status.into(),
            last_qty: "0".into(),
            cumulative_qty: "0".into(),
            last_price: "0".into(),
            side: if side == Side::Buy { "BUY" } else { "SELL" }.into(),
            position_side: "BOTH".into(),
            maker: false,
            trade_id: None,
            event_ms: self.now_ms(),
            initiator: Initiator::Account,
        })
    }
}

/// The shared core, as each face of the simulation holds it.
#[derive(Debug, Clone)]
pub struct Sim(Rc<RefCell<Core>>);

impl Sim {
    #[must_use]
    pub fn new(cfg: SimConfig) -> Self {
        Self(Rc::new(RefCell::new(Core::new(cfg))))
    }

    /// The venue face, for [`run_on`](crate::run::run_on).
    #[must_use]
    pub fn account(&self) -> Box<dyn Account> {
        Box::new(SimAccount(self.clone()))
    }

    /// Every client id sent to the venue, in order.
    #[must_use]
    pub fn placed(&self) -> Vec<String> {
        self.0.borrow().placed.clone()
    }

    /// Orders still resting, by client id.
    #[must_use]
    pub fn resting(&self) -> Vec<String> {
        self.0.borrow().resting.keys().cloned().collect()
    }

    /// Net position in lots.
    #[must_use]
    pub fn position(&self) -> i64 {
        self.0.borrow().position
    }

    /// Simulated time elapsed.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.0.borrow().clock.elapsed()
    }
}

/// A clock that reads the core's.
///
/// The environment has to hand out `&dyn Clock` for the whole run while
/// the core is mutably borrowed in between, so the clock the loop sees is
/// this view rather than the core's own field.
#[derive(Debug)]
struct SimClock(Sim);

impl Clock for SimClock {
    fn wall(&self) -> Nanos {
        self.0.0.borrow().clock.wall()
    }

    fn elapsed(&self) -> Duration {
        self.0.0.borrow().clock.elapsed()
    }

    fn sleep(&self, d: Duration) {
        let mut core = self.0.0.borrow_mut();
        core.clock.advance(d);
        core.advance();
    }
}

/// The environment face.
#[derive(Debug)]
pub struct SimEnv {
    sim: Sim,
    clock: SimClock,
}

impl SimEnv {
    #[must_use]
    pub fn new(sim: &Sim) -> Self {
        Self {
            sim: sim.clone(),
            clock: SimClock(sim.clone()),
        }
    }
}

#[derive(Clone, Copy)]
enum Feed {
    Depth,
    Trade,
}

struct FeedSource {
    sim: Sim,
    feed: Feed,
}

impl MessageSource for FeedSource {
    fn next_message(&mut self) -> io::Result<Vec<u8>> {
        let mut core = self.sim.0.borrow_mut();
        let now = core.clock.elapsed();
        let queue = match self.feed {
            Feed::Depth => &mut core.depth,
            Feed::Trade => &mut core.trades,
        };
        match queue.front() {
            Some((at, _)) if *at <= now => {
                Ok(queue.pop_front().map(|(_, b)| b).unwrap_or_default())
            }
            _ => Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing yet")),
        }
    }
}

struct FeedConnector {
    sim: Sim,
    feed: Feed,
}

impl Connector for FeedConnector {
    type Source = FeedSource;

    fn connect(&mut self) -> io::Result<FeedSource> {
        Ok(FeedSource {
            sim: self.sim.clone(),
            feed: self.feed,
        })
    }
}

struct SimUserEvents(Sim);

impl UserEvents for SimUserEvents {
    fn next(&mut self) -> StreamOutcome {
        let mut core = self.0.0.borrow_mut();
        if let Some(e) = core.events.pop_front() {
            return StreamOutcome::Event(e);
        }
        // Nothing to read: this is where time passes.
        let step = core.cfg.idle_step;
        core.clock.advance(step);
        core.advance();
        StreamOutcome::Idle
    }

    fn close(self: Box<Self>) -> Result<(), VenueError> {
        Ok(())
    }
}

/// Parses nothing: the simulated stream hands over events already read.
struct NoWire;

impl Events for NoWire {
    fn read(&self, _message: &str) -> Vec<UserEvent> {
        Vec::new()
    }
}

impl Environment for SimEnv {
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn market_data(
        &self,
        _venue: &str,
        _deployment: Deployment,
        _symbol: &str,
    ) -> Result<(MarketData, Box<dyn Venue>), String> {
        let stream = |name, feed| {
            Stream::over(
                name,
                AnyConnector::new(FeedConnector {
                    sim: self.sim.clone(),
                    feed,
                }),
                Duration::from_secs(30),
            )
        };
        Ok((
            MarketData::from_streams(stream("depth", Feed::Depth), stream("trade", Feed::Trade)),
            Box::new(oq_l2feed::venue::binance::BinancePerp::at(
                Deployment::Testnet,
            )),
        ))
    }

    fn user_events(&self, _stream: &UserStream) -> Result<Box<dyn UserEvents>, VenueError> {
        Ok(Box::new(SimUserEvents(self.sim.clone())))
    }

    fn depth_snapshot(&self, _url: &str, _timeout: Duration) -> io::Result<Vec<u8>> {
        let core = self.sim.0.borrow();
        Ok(format!(
            r#"{{"lastUpdateId":{},"bids":[["{}","{}"]],"asks":[["{}","{}"]]}}"#,
            core.update_id,
            core.px(core.price - 1),
            core.qty(5_000),
            core.px(core.price + 1),
            core.qty(5_000),
        )
        .into_bytes())
    }

    fn state_root(&self) -> Option<PathBuf> {
        Some(self.sim.0.borrow().cfg.state_root.clone())
    }

    fn shutdown_requested(&self) -> bool {
        false
    }
}

/// The venue face.
#[derive(Debug)]
struct SimAccount(Sim);

impl Execution for SimAccount {
    fn place(&self, order: &NewOrder, _instrument: &Instrument) -> Placed {
        let mut core = self.0.0.borrow_mut();
        core.placed.push(order.client_id.clone());
        if core.resting.contains_key(&order.client_id)
            || core.finished.contains_key(&order.client_id)
        {
            return Placed::Rejected(Reject {
                code: Some(-4116),
                message: "ClientOrderId is duplicated".into(),
            });
        }
        let venue_id = core.next_venue_id;
        core.next_venue_id += 1;
        let Some(limit) = order.limit_price else {
            let price = core.price;
            core.fill(
                &order.client_id,
                venue_id,
                order.side,
                price,
                order.qty.0,
                false,
            );
            return accepted(&order.client_id, venue_id, "FILLED");
        };
        let marketable = match order.side {
            Side::Buy => limit.0 >= core.price,
            Side::Sell => limit.0 <= core.price,
        };
        if marketable {
            let price = core.price;
            core.fill(
                &order.client_id,
                venue_id,
                order.side,
                price,
                order.qty.0,
                false,
            );
            return accepted(&order.client_id, venue_id, "FILLED");
        }
        core.resting.insert(
            order.client_id.clone(),
            Resting {
                venue_id,
                side: order.side,
                price: limit.0,
                qty: order.qty.0,
            },
        );
        let e = core.update(&order.client_id, venue_id, order.side, "NEW");
        core.events.push_back(e);
        accepted(&order.client_id, venue_id, "NEW")
    }

    fn cancel(&self, _symbol: &str, client_id: &str) -> Placed {
        let mut core = self.0.0.borrow_mut();
        match core.resting.remove(client_id) {
            Some(o) => {
                core.finished.insert(
                    client_id.to_string(),
                    Finished {
                        venue_id: o.venue_id,
                        status: "CANCELED",
                        filled: 0,
                    },
                );
                let e = core.update(client_id, o.venue_id, o.side, "CANCELED");
                core.events.push_back(e);
                accepted(client_id, o.venue_id, "CANCELED")
            }
            None => Placed::Rejected(Reject {
                code: Some(-2011),
                message: "Unknown order sent.".into(),
            }),
        }
    }

    fn order_status(&self, _symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        let core = self.0.0.borrow();
        Ok(if let Some(o) = core.resting.get(client_id) {
            Some(OrderAck {
                venue_id: o.venue_id.to_string(),
                client_id: client_id.to_string(),
                status: "NEW".into(),
                executed_qty: "0".into(),
            })
        } else {
            core.finished.get(client_id).map(|f| OrderAck {
                venue_id: f.venue_id.to_string(),
                client_id: client_id.to_string(),
                status: f.status.into(),
                executed_qty: core.qty(f.filled),
            })
        })
    }
}

fn accepted(client_id: &str, venue_id: u64, status: &str) -> Placed {
    Placed::Accepted(OrderAck {
        venue_id: venue_id.to_string(),
        client_id: client_id.to_string(),
        status: status.into(),
        executed_qty: "0".into(),
    })
}

impl Account for SimAccount {
    fn id(&self) -> &'static str {
        "binance-perp"
    }

    fn id_rules(&self) -> IdRules {
        IdRules::BINANCE
    }

    fn recent_bars(&self, _symbol: &str, _minutes: usize) -> Result<Vec<Kline>, VenueError> {
        Ok(Vec::new())
    }

    fn sync_clock(&mut self) -> Result<i64, VenueError> {
        Ok(0)
    }

    fn round_trip_ms(&self) -> i64 {
        0
    }

    fn instrument(&self, _symbol: &str) -> Result<Instrument, String> {
        Ok(self.0.0.borrow().instrument)
    }

    fn is_hedged(&self) -> Result<bool, VenueError> {
        Ok(false)
    }

    fn positions(&self, _symbol: &str) -> Result<Vec<PositionSnapshot>, VenueError> {
        let core = self.0.0.borrow();
        if core.position == 0 {
            return Ok(Vec::new());
        }
        let amount_text = decimal(core.position, core.instrument.qty_scale);
        let entry = core.entry / 10f64.powi(i32::from(core.instrument.price_scale));
        Ok(vec![PositionSnapshot {
            symbol: core.cfg.symbol.clone(),
            position_side: "BOTH".into(),
            amount: amount_text.parse().unwrap_or(0.0),
            amount_text,
            entry_text: format!("{entry}"),
            entry_price: entry,
            unrealized: 0.0,
        }])
    }

    fn balances(&self) -> Result<AccountSnapshot, VenueError> {
        let core = self.0.0.borrow();
        let scale = 10f64
            .powi(i32::from(core.instrument.price_scale) + i32::from(core.instrument.qty_scale));
        let wallet = core.cfg.balance + core.realized / scale;
        Ok(AccountSnapshot {
            wallet_balance: wallet,
            unrealized: 0.0,
            margin_balance: wallet,
            read_at_ms: core.now_ms(),
        })
    }

    fn open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>, VenueError> {
        let core = self.0.0.borrow();
        Ok(core
            .resting
            .iter()
            .map(|(id, o)| OpenOrder {
                symbol: core.cfg.symbol.clone(),
                order_id: o.venue_id.to_string(),
                client_order_id: id.clone(),
                side: if o.side == Side::Buy { "BUY" } else { "SELL" }.into(),
                position_side: "BOTH".into(),
                price: o.price as f64 / 10f64.powi(i32::from(core.instrument.price_scale)),
                orig_qty: o.qty as f64 / 10f64.powi(i32::from(core.instrument.qty_scale)),
                executed_qty: 0.0,
                status: "NEW".into(),
            })
            .collect())
    }

    fn trade_history(&self, _symbol: &str) -> Result<Option<Vec<AccountTrade>>, VenueError> {
        Ok(None)
    }

    fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        Ok(UserStream::new(
            "sim://account".into(),
            "sim".into(),
            std::sync::Arc::new(NoWire),
        ))
    }

    fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        Ok(())
    }

    fn close_user_stream(&self) -> Result<(), VenueError> {
        Ok(())
    }
}
