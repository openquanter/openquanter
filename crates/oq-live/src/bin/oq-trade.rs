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

use oq_gateway::Credentials;
use oq_gateway::account::Account;
use oq_gateway::binance::Binance;
use oq_gateway::exec::Endpoint;
use oq_gateway::okx::Okx;
use oq_l2feed::venue::Deployment;
use oq_live::run::{RunConfig, run_on, smallest_allowed};
use oq_risk::Limits;
use oq_strategy::{Context, Intent, Strategy};
use oq_types::{Cash, Instrument, Nanos, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side};

const USAGE: &str = "\
oq-trade — run a strategy against a venue

USAGE:
    OQ_VENUE_KEY=<key> OQ_VENUE_SECRET=<secret> oq-trade [OPTIONS]

OPTIONS:
    --venue <NAME>         binance | aster | okx [default: binance]
                           The symbol is the venue's own spelling:
                           BTCUSDT on the first two, BTC-USDT-SWAP on the
                           third. Aster ships Binance's API under
                           /fapi/v3, so it is the same adapter.
    --symbol <SYMBOL>      Contract [default: BTCUSDT]
    --strategy <NAME>      observe | probe [default: observe]
    --window-ms <MS>       Tick width [default: 1000]
    --minutes <N>          Stop after this long; 0 runs until a signal
                           arrives [default: 5]
    --warm-minutes <N>     Replay this much history first [default: 0]
    --model-maker-fee-ppm <N>  Stated shadow maker fee, signed integer ppm
    --model-taker-fee-ppm <N>  Stated shadow taker fee; both rates required
    --account-scope-hash <SHA256>  Established opaque venue/account scope for coverage
    --max-qty <LOTS>       Largest single order [default: 1]
                           One lot is below the minimum notional on most
                           contracts, so the gate refuses every order at the
                           default. That is the gate working and the default
                           being wrong for the venue — raise it to whatever
                           `oq-order-check` reports as the smallest allowed.
    --max-position <LOTS>  Largest position [default: 1]
    --max-notional <USDT>  Largest order notional [default: 200]
    --band-bps <BPS>       How far a limit may sit from the mark [default: 3000]
    --id-prefix <TEXT>     Ownership prefix: is this order mine [default: oq]
    --broker-code <CODE>   Venue-issued broker or referral code, when you have
                           one. Not the same thing as --id-prefix: that says
                           which orders are this process's, this says who gets
                           paid for the flow. Alphanumeric.
    --journal <PATH>       Where to record decisions [default: oq-trade.oqj]
    --no-journal           Trade without recording. Nothing can be replayed
    --adopt-existing       Start beside a position the venue already holds
    --control-allow-resume Let the control port clear the kill switch; off by default
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
    /// Whether the venue has confirmed the order this cycle exists.
    ///
    /// Separate from `placed`, and the distinction is the whole reason
    /// the placement contract has three outcomes rather than two.
    /// `placed` means *this strategy asked*; this means *the venue said
    /// yes*. An earlier version had only the first and treated it as the
    /// second, so a refused order was still believed to be resting and
    /// the probe went on to cancel one that never existed:
    ///
    /// ```text
    /// refused        OrderId(1): OrderTooLarge { qty: 11, limit: 1 }
    /// unknown order  OrderId(1) — not in this run's map
    /// ```
    ///
    /// A real strategy written that way believes it holds exposure it
    /// does not have. This one is a teaching example, which made it
    /// worse: it was demonstrating the mistake it exists to warn about.
    confirmed: bool,
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
            // Asked, not placed. The venue has not answered, and until
            // it does this strategy knows only that it sent something.
            self.placed = true;
            self.confirmed = false;
            let price = self.instrument.snap_price_down(PriceTicks(
                reference.0 - reference.0 * self.away_bps / 10_000,
            ));
            out.push(Intent::Limit {
                instrument: ctx.instrument,
                id: OrderId(self.next_id),
                side: Side::Buy,
                price,
                qty: self
                    .instrument
                    .snap_qty_up(smallest_allowed(&self.instrument, price)),
                offset: Offset::Open,
            });
        } else if self.confirmed
            && !self.cancelled
            && self.ticks.saturating_sub(self.cancelled_at) > 5
        {
            // Only an order the venue confirmed can be withdrawn.
            // Cancelling one that was refused asks the venue about an id
            // it has never seen, and the answer — "unknown order" — is
            // indistinguishable from a cancel that raced a fill.
            self.cancelled = true;
            self.cancelled_at = self.ticks;
            out.push(Intent::Cancel(OrderId(self.next_id)));
        } else if self.placed
            && !self.confirmed
            && self.ticks.saturating_sub(self.cancelled_at) > 20
        {
            // Asked long ago and never confirmed. Start a new cycle
            // rather than waiting forever — and say so, because a probe
            // that silently retried would report cycles it never
            // completed.
            println!(
                "probe            order {} was never confirmed; new cycle",
                self.next_id
            );
            self.placed = false;
            self.cancelled = false;
            self.next_id += 1;
            self.cancelled_at = self.ticks;
        }
    }

    fn on_fill(&mut self, _f: &oq_types::Fill, _c: &Context, _o: &mut Vec<Intent>) {
        // A fill is the strongest possible confirmation the order
        // existed.
        self.confirmed = true;
    }

    fn on_placed(&mut self, id: OrderId, accepted: bool) {
        if id != OrderId(self.next_id) {
            // An answer about an earlier cycle's order. Ignored rather
            // than applied to the current one, which is the mistake the
            // flag existed to make impossible.
            return;
        }
        if accepted {
            self.confirmed = true;
        } else {
            // Refused. There is nothing at the venue, so the next cycle
            // starts rather than a cancel being sent for an id it has
            // never seen.
            self.placed = false;
            self.confirmed = false;
            self.next_id += 1;
            self.cancelled_at = self.ticks;
        }
    }

    fn name(&self) -> &str {
        "probe"
    }
}

fn model_fee_policy(args: &[String]) -> Result<Option<oq_core::Fees>, String> {
    let read = |name: &str| -> Result<Option<Ratio>, String> {
        let positions: Vec<_> = args
            .iter()
            .enumerate()
            .filter_map(|(i, arg)| (arg == name).then_some(i))
            .collect();
        match positions.as_slice() {
            [] => Ok(None),
            [i] => {
                let ppm = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{name} needs integer ppm"))?
                    .parse::<i64>()
                    .map_err(|_| format!("{name} needs integer ppm"))?;
                Ok(Some(Ratio(ppm.checked_mul(1000).ok_or_else(|| {
                    format!("{name} is outside ratio precision")
                })?)))
            }
            _ => Err(format!("{name} was repeated")),
        }
    };
    match (
        read("--model-maker-fee-ppm")?,
        read("--model-taker-fee-ppm")?,
    ) {
        (None, None) => Ok(None),
        (Some(maker), Some(taker)) => Ok(Some(oq_core::Fees { maker, taker })),
        _ => Err("model maker and taker fees must both be stated".into()),
    }
}

fn account_scope_policy(args: &[String]) -> Result<Option<String>, String> {
    let mut positions = args
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| (arg == "--account-scope-hash").then_some(i));
    let Some(i) = positions.next() else {
        return Ok(None);
    };
    if positions.next().is_some() {
        return Err("--account-scope-hash was repeated".into());
    }
    let hash = args
        .get(i + 1)
        .ok_or("--account-scope-hash needs SHA-256")?;
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("--account-scope-hash needs a 64-digit SHA-256".into());
    }
    Ok(Some(hash.to_ascii_lowercase()))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let model_fees = match model_fee_policy(&args) {
        Ok(fees) => fees,
        Err(why) => {
            eprintln!("oq-trade: {why}");
            return ExitCode::FAILURE;
        }
    };
    let mut environment = oq_live::env::Production::new();
    if let Some(fees) = model_fees {
        environment = environment.with_model_fees(fees);
    }
    match account_scope_policy(&args) {
        Ok(Some(hash)) => environment = environment.with_account_scope_hash(hash),
        Ok(None) => {}
        Err(why) => {
            eprintln!("oq-trade: {why}");
            return ExitCode::FAILURE;
        }
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

    // A venue-issued code, when the operator has one. Separate flag
    // from --id-prefix because they answer different questions; see
    // oq_gateway::broker.
    let broker_code = value("--broker-code");
    let cfg = RunConfig {
        broker_code,
        symbol,
        strategy_name: strategy_name.clone(),
        deployment,
        minutes: number("--minutes", 5),
        // The teaching strategies need no history.
        warm_minutes: usize::try_from(number("--warm-minutes", 0)).unwrap_or(0),
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
        control_allow_resume: args.iter().any(|a| a == "--control-allow-resume"),
    };

    // The venue is built here rather than inside `run`, because which
    // venue to trade is this binary's decision and `run` is generic over
    // it. Credentials never travel further than this line.
    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("credentials      FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Named, not inferred from the symbol. A symbol that parses on two
    // venues would otherwise pick one silently, and the one it picked
    // would be whichever this file happened to check first.
    let venue_name = value("--venue").unwrap_or_else(|| "binance".to_string());
    let venue: Box<dyn Account> = match venue_name.as_str() {
        "binance" => Box::new(Binance::at(endpoint, creds)),
        "aster" => Box::new(Binance::aster(endpoint, creds)),
        "okx" => Box::new(Okx::at(endpoint, creds)),
        other => {
            eprintln!(
                "venue            {other:?} is not a venue this build speaks; \
                 try binance, aster or okx"
            );
            return ExitCode::FAILURE;
        }
    };
    // Which venue, said once, before anything is sent to it. The run's
    // log is read afterwards by someone deciding what happened, and
    // `--symbol BTCUSDT` against the wrong venue fails in a way that
    // looks like a bad contract rather than a bad flag.
    println!("venue            {} ({deployment:?})", venue.id());

    match strategy_name.as_str() {
        "observe" => run_on(venue, |_| Observe { ticks: 0 }, &cfg, &environment),
        "probe" => run_on(
            venue,
            |instrument| Probe {
                confirmed: false,
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
            &environment,
        ),
        other => {
            eprintln!(
                "oq-trade: unknown strategy {other:?}; known: observe, probe. A real \
                 strategy runs through `oq_live::run` — see \
                 `cargo run -p oq-live --example grid_live`, which is a live binary \
                 of about thirty lines."
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(words: &[&str]) -> Result<Option<oq_core::Fees>, String> {
        model_fee_policy(
            &words
                .iter()
                .map(|word| (*word).to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn model_fee_policy_distinguishes_unknown_zero_and_rebate() {
        assert_eq!(policy(&[]).unwrap(), None);
        assert_eq!(
            policy(&["--model-maker-fee-ppm", "0", "--model-taker-fee-ppm", "0"]).unwrap(),
            Some(oq_core::Fees::none())
        );
        let fees = policy(&[
            "--model-maker-fee-ppm",
            "-100",
            "--model-taker-fee-ppm",
            "500",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(fees.maker, Ratio::from_ppm(-100));
        assert_eq!(fees.taker, Ratio::from_ppm(500));
        for words in [
            vec!["--model-maker-fee-ppm", "1"],
            vec!["--model-taker-fee-ppm"],
            vec!["--model-maker-fee-ppm", "x"],
            vec!["--model-maker-fee-ppm", "9223372036854775807"],
            vec!["--model-maker-fee-ppm", "0", "--model-maker-fee-ppm", "0"],
        ] {
            assert!(policy(&words).is_err(), "{words:?}");
        }
    }

    #[test]
    fn account_scope_requires_a_single_opaque_hash() {
        assert_eq!(account_scope_policy(&[]).unwrap(), None);
        let hash = "B".repeat(64);
        assert_eq!(
            account_scope_policy(&["--account-scope-hash".into(), hash.clone()]).unwrap(),
            Some(hash.to_ascii_lowercase())
        );
        for args in [
            vec!["--account-scope-hash".into()],
            vec!["--account-scope-hash".into(), "unknown".into()],
            vec![
                "--account-scope-hash".into(),
                hash.clone(),
                "--account-scope-hash".into(),
                hash,
            ],
        ] {
            assert!(account_scope_policy(&args).is_err());
        }
    }
}
