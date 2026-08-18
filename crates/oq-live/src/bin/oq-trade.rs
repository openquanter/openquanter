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

use std::process::ExitCode;

use oq_gateway::exec::Endpoint;
use oq_l2feed::venue::Deployment;
use oq_live::run::{RunConfig, run, smallest_allowed};
use oq_risk::Limits;
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
    --id-prefix <TEXT>     Client order id prefix [default: oq]
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
        println!("{USAGE}");
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
            "oq-trade: --live sends orders to the production venue with real \
             money.\n         Set OQ_ALLOW_LIVE=i-understand to proceed."
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

    let cfg = RunConfig {
        symbol,
        strategy_name: strategy_name.clone(),
        endpoint,
        deployment,
        minutes: number("--minutes", 5),
        window_ms: number("--window-ms", 1000),
        id_prefix: value("--id-prefix").unwrap_or_else(|| "oq".to_string()),
        adopt_existing: args.iter().any(|a| a == "--adopt-existing"),
        journal: if args.iter().any(|a| a == "--no-journal") {
            None
        } else {
            Some(value("--journal").unwrap_or_else(|| "oq-trade.oqj".to_string()))
        },
        limits: Limits {
            max_order_qty: QtyLots(number("--max-qty", 1)),
            max_position_qty: QtyLots(number("--max-position", 1)),
            max_order_notional: Cash(number("--max-notional", 200) * oq_types::CASH_SCALE),
            price_band: Ratio(number("--band-bps", 3000) * 100_000),
            max_working: 4,
            max_rate: 10,
            rate_window: Nanos(60 * 1_000_000_000),
        },
    };

    match strategy_name.as_str() {
        "observe" => run(|_| Observe { ticks: 0 }, &cfg),
        "probe" => run(
            |instrument| Probe {
                placed: false,
                cancelled: false,
                ticks: 0,
                idle_ticks: 30,
                cancelled_at: 0,
                cycles: 0,
                next_id: 1,
                away_bps: 2000,
                instrument: *instrument,
            },
            &cfg,
        ),
        other => {
            eprintln!("oq-trade: unknown strategy {other:?}; known: observe, probe");
            ExitCode::FAILURE
        }
    }
}
