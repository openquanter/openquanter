//! `oq-order-check` — prove the order path works, against the testnet.
//!
//! ```text
//! OQ_VENUE_KEY=… OQ_VENUE_SECRET=… oq-order-check --symbol BTCUSDT
//! ```
//!
//! Places one limit order far below the market so it cannot fill,
//! confirms the venue and the user data stream both report it, cancels
//! it, and confirms that too. Every step prints what it proved.
//!
//! # It will not talk to production
//!
//! There is no flag for it. This tool sends orders, and a diagnostic
//! that can send an order with real money behind it is a diagnostic
//! somebody will eventually run against the wrong account at the wrong
//! hour. Reading a live account is what `oq-recon` is for, and it
//! cannot place anything.
//!
//! # What a pass means
//!
//! That the request shape, the signature, the precision, the stream
//! subscription and the cancel path all work against a real venue —
//! the things that cannot be established by a unit test. It does not
//! mean a strategy is safe to run: the risk gate is a separate layer
//! with its own tests, and nothing yet composes the two into a process.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use oq_gateway::binance::Binance;
use oq_gateway::exec::{Endpoint, Execution, NewOrder, Placed, PositionSide};
use oq_gateway::{Credentials, StreamOutcome, UserEvent, UserStreamReader};
use oq_types::{Instrument, PriceTicks, QtyLots, Side, TimeInForce};

const USAGE: &str = "\
oq-order-check — prove the order path against the testnet

USAGE:
    OQ_VENUE_KEY=<key> OQ_VENUE_SECRET=<secret> oq-order-check [OPTIONS]

OPTIONS:
    --symbol <SYMBOL>    Contract to test with [default: BTCUSDT]
    --help

Testnet only, deliberately. See the module documentation.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--live" || a == "--mainnet") {
        eprintln!(
            "oq-order-check: there is no live mode. This tool sends orders, and a\n\
             diagnostic that can send one with real money behind it is one somebody\n\
             will eventually run against the wrong account. Use oq-recon to read a\n\
             live account; it cannot place anything."
        );
        return ExitCode::FAILURE;
    }
    let symbol = args
        .iter()
        .position(|a| a == "--symbol")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "BTCUSDT".to_string());

    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oq-order-check: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut venue = Binance::at(Endpoint::Testnet, creds);
    println!("venue            testnet ({})", Binance::TESTNET);
    println!("symbol           {symbol}");

    // 1. Clock. A signed request is rejected outright if the local
    //    clock has drifted, and the message names the signature rather
    //    than the clock — an afternoon lost to the wrong suspect.
    match venue.sync_clock() {
        Ok(offset) => println!("clock            offset {offset} ms"),
        Err(e) => {
            eprintln!("clock            FAILED: {e}");
            return ExitCode::FAILURE;
        }
    }

    // 2. Precision, from this deployment rather than from a table.
    //    The testnet lists its own contracts and they do not always
    //    match production; a table baked for one is wrong for the other.
    let instrument = match precision_of(&venue, &symbol) {
        Ok(i) => {
            println!(
                "precision        price {} dp (tick {}), qty {} dp (step {})",
                i.price_scale, i.price_tick, i.qty_scale, i.qty_step
            );
            i
        }
        Err(e) => {
            eprintln!("precision        FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mark = match mark_price(&venue, &symbol, instrument.price_scale) {
        Ok(p) => {
            println!(
                "mark             {}",
                oq_gateway::exec::decimal(p.0, instrument.price_scale)
            );
            p
        }
        Err(e) => {
            eprintln!("mark             FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 3. The stream, opened before the order so the order's own arrival
    //    is observable. Opening it afterwards would leave the first
    //    event racing the subscription.
    let stream = match venue.open_user_stream() {
        Ok(s) => {
            println!("stream           opened");
            s
        }
        Err(e) => {
            eprintln!("stream           FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut reader = match UserStreamReader::connect(&stream, Duration::from_secs(2)) {
        Ok(r) => {
            println!("stream           connected");
            r
        }
        Err(e) => {
            eprintln!("stream           FAILED to connect: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 4. An order that cannot fill: twenty percent below the mark, and
    //    large enough to clear the venue's minimum notional. Both
    //    matter — too close fills, too small is refused.
    // Asked, not assumed. A hedged account refuses an order that does
    // not name its leg, and a one-way account refuses one that does.
    let hedged = match venue.is_hedged_account() {
        Ok(h) => {
            println!("position mode    {}", if h { "hedged" } else { "one-way" });
            h
        }
        Err(e) => {
            eprintln!("position mode    FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Twenty percent below the mark, snapped down onto the contract's
    // grid: a price with the right number of decimals is not
    // necessarily a price the venue accepts.
    let price = instrument.snap_price_down(PriceTicks(mark.0 * 8 / 10));
    let qty = instrument.snap_qty_up(min_qty_for_notional(&instrument, price));
    let client_id = format!("oq-check-{}", std::process::id());
    let order = NewOrder {
        symbol: symbol.clone(),
        side: Side::Buy,
        limit_price: Some(price),
        qty,
        tif: TimeInForce::GoodTilCancel,
        client_id: client_id.clone(),
        reduce_only: false,
        position_side: if hedged {
            PositionSide::Long
        } else {
            PositionSide::OneWay
        },
    };
    println!(
        "order            buy {} @ {} (id {client_id})",
        oq_gateway::exec::decimal(qty.0, instrument.qty_scale),
        oq_gateway::exec::decimal(price.0, instrument.price_scale)
    );

    let placed = venue.place(&order, &instrument);
    match &placed {
        Placed::Accepted(a) => println!("place            accepted, venue id {}", a.venue_id),
        Placed::Rejected(r) => {
            eprintln!("place            REJECTED: {:?} {}", r.code, r.message);
            return ExitCode::FAILURE;
        }
        Placed::Unknown(u) => {
            // The path this whole design exists for. Not a failure yet:
            // ask the venue by the id chosen before sending.
            println!("place            unknown ({}) — resolving by id", u.reason);
            match venue.order_status(&symbol, &client_id) {
                Ok(Some(a)) => println!("resolve          the order exists: {}", a.status),
                Ok(None) => {
                    eprintln!("resolve          the order never landed");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("resolve          FAILED: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    // 5. The same order, seen from the other transport. This is the
    //    step a unit test cannot reach: it proves the two halves are
    //    talking about the same account.
    match wait_for(&mut reader, &client_id, Duration::from_secs(10)) {
        Some(status) => println!("stream           saw the order: {status}"),
        None => {
            eprintln!("stream           the order never appeared on the stream");
            eprintln!("                 REST and the socket disagree, which is the one");
            eprintln!("                 outcome worth stopping for.");
            return ExitCode::FAILURE;
        }
    }

    // 6. Cancel, and confirm.
    match venue.cancel(&symbol, &client_id) {
        Placed::Accepted(a) => println!("cancel           accepted, status {}", a.status),
        Placed::Rejected(r) => {
            eprintln!("cancel           REJECTED: {:?} {}", r.code, r.message);
            return ExitCode::FAILURE;
        }
        Placed::Unknown(u) => {
            println!("cancel           unknown ({}) — resolving", u.reason);
            match venue.order_status(&symbol, &client_id) {
                Ok(Some(a)) if a.status == "CANCELED" => println!("resolve          cancelled"),
                Ok(other) => {
                    eprintln!("resolve          still {other:?} — cancel it by hand");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("resolve          FAILED: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let _ = reader.close();
    let _ = venue.close_user_stream();

    println!();
    println!("PASS — signature, precision, order entry, user stream and cancel all work");
    println!("against a real venue. This says nothing about whether a strategy is safe");
    println!("to run: the risk gate is a separate layer, and no process composes them yet.");
    ExitCode::SUCCESS
}

/// Wait for an event naming this order.
fn wait_for(reader: &mut UserStreamReader, client_id: &str, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match reader.next() {
            StreamOutcome::Event(UserEvent::Order(u)) if u.client_id == client_id => {
                return Some(u.status);
            }
            StreamOutcome::Disconnected(why) => {
                eprintln!("stream           disconnected: {why}");
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Read this deployment's precision for `symbol`.
fn precision_of(venue: &Binance, symbol: &str) -> Result<Instrument, String> {
    let body = venue.exchange_info(symbol).map_err(|e| e.to_string())?;
    let price = field_u8(&body, "pricePrecision").ok_or("no pricePrecision")?;
    let qty = field_u8(&body, "quantityPrecision").ok_or("no quantityPrecision")?;
    // How many decimals a price may have and which prices are allowed
    // are separate facts, published separately. Reading only the first
    // produces prices the venue refuses.
    let tick = extract(&body, "tickSize")
        .and_then(|t| parse_fixed(&t, price))
        .map_or(1, |p| p.0);
    let step = extract(&body, "stepSize")
        .and_then(|t| parse_fixed(&t, qty))
        .map_or(1, |p| p.0);
    Ok(Instrument::linear(price, qty).with_grid(tick, step))
}

fn mark_price(venue: &Binance, symbol: &str, scale: u8) -> Result<PriceTicks, String> {
    let body = venue.ticker_price(symbol).map_err(|e| e.to_string())?;
    let text = extract(&body, "price").ok_or("no price")?;
    parse_fixed(&text, scale).ok_or_else(|| format!("unreadable price {text:?}"))
}

/// The smallest quantity whose notional clears a venue minimum.
///
/// Futures venues refuse an order below a floor — commonly 100 units of
/// quote currency — and the refusal names the notional, not the
/// quantity, which is a confusing place to start debugging.
fn min_qty_for_notional(instrument: &Instrument, price: PriceTicks) -> QtyLots {
    const FLOOR_QUOTE: i64 = 150;
    let Some(tick_cash) = instrument.tick_cash() else {
        return QtyLots(1);
    };
    let per_lot = i128::from(price.0) * i128::from(tick_cash);
    if per_lot <= 0 {
        return QtyLots(1);
    }
    let need = i128::from(FLOOR_QUOTE) * i128::from(oq_types::CASH_SCALE);
    // Round up: rounding down lands just under the floor and the
    // venue refuses with a message about notional.
    let lots = (need + per_lot - 1) / per_lot;
    QtyLots(i64::try_from(lots).unwrap_or(1).max(1))
}

fn extract(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn field_u8(body: &str, key: &str) -> Option<u8> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Decimal text to a fixed-point integer at `scale`.
fn parse_fixed(text: &str, scale: u8) -> Option<PriceTicks> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let mut digits = String::from(whole);
    let width = usize::from(scale);
    let mut frac = frac.to_string();
    frac.truncate(width);
    while frac.len() < width {
        frac.push('0');
    }
    digits.push_str(&frac);
    digits.parse().ok().map(PriceTicks)
}
