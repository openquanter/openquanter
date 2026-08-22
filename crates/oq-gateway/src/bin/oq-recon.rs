//! `oq-recon` — read an account and say whether it matches what you expect.
//!
//! Places no orders and cancels none: the crate it is built from has no
//! code that could. Run it beside a system that is already trading and
//! it reports, every few seconds, whether that system's view of the
//! account is the account.
//!
//! This is the first thing worth building against a live venue and the
//! last thing most projects build. Its findings are evidence about the
//! parts a backtest cannot reach — a fill delivered twice, a cancel that
//! raced an execution, a position adjusted by something nobody sent —
//! and it collects them at no risk.
//!
//! ```text
//! OQ_VENUE_KEY=... OQ_VENUE_SECRET=... \
//!   oq-recon BTCUSDT --expect-long 0.256@71444.87 --interval 10
//! ```
//!
//! Without an expectation it prints what the venue holds and exits,
//! which is how you find out what to expect.
//!
//! ## Exit codes
//!
//! `0` the account matched, `1` it did not, `3` it could not be read, `2`
//! the arguments were wrong. Three outcomes rather than two because "I
//! could not check" is not "I checked and it is fine" — a startup gate
//! that conflates them lets a system begin trading against an account it
//! never verified. Measured against a live testnet from one host, 4 of
//! 124 reads came back partial: this is a routine outcome, not an
//! exotic one.

use std::time::Duration;

use oq_gateway::{
    Binance, Credentials, Expectation, ExpectedLeg, Part, SnapshotBuilder, Tolerance, Watcher,
    reconcile,
};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: oq-recon <SYMBOL> [--expect-long QTY@PRICE] [--expect-short QTY@PRICE]\n\
             \x20                      [--order CLIENT_ID]... [--interval SECS] [--testnet]\n\
             \x20      oq-recon <SYMBOL> --watch [--interval SECS] [--testnet]\n\n\
             Reads the account. Places nothing.\n\n\
             Default is a gate: it compares against the expectation you give and \n\
             exits non-zero on a position that does not match.\n\
             --watch observes instead, reporting what changes between reads and \n\
             never exiting on a difference — a gate that stops at the first one \n\
             sees a single event and then nothing.\n\n\
             Exits 0 matched, 1 diverged, 2 bad arguments, 3 could not read \n\
             the account. 3 is not 0: not checking is not the same as passing."
        );
        return std::process::ExitCode::from(2);
    }

    let symbol = args[0].to_uppercase();
    let mut expected = Expectation::default();
    let mut interval: Option<u64> = None;
    let mut watching = false;
    let mut base = Binance::MAINNET;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--testnet" => base = Binance::TESTNET,
            "--watch" => watching = true,
            "--expect-long" | "--expect-short" => {
                let side = if args[i] == "--expect-long" {
                    "LONG"
                } else {
                    "SHORT"
                };
                let Some(spec) = args.get(i + 1) else {
                    eprintln!("{} needs QTY@PRICE", args[i]);
                    return std::process::ExitCode::from(2);
                };
                match parse_leg(side, spec) {
                    Ok(leg) => expected.legs.push(leg),
                    Err(e) => {
                        eprintln!("{e}");
                        return std::process::ExitCode::from(2);
                    }
                }
                i += 1;
            }
            "--order" => {
                let Some(id) = args.get(i + 1) else {
                    eprintln!("--order needs a client order id");
                    return std::process::ExitCode::from(2);
                };
                expected.working_orders.push(id.clone());
                i += 1;
            }
            "--interval" => {
                let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) else {
                    eprintln!("--interval needs a number of seconds");
                    return std::process::ExitCode::from(2);
                };
                interval = Some(v);
                i += 1;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return std::process::ExitCode::from(2);
            }
        }
        i += 1;
    }

    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };

    let mut venue = Binance::new(base, creds);
    // Retried rather than fatal. A watch is meant to run unattended for
    // weeks, and a single slow response at the moment it starts is not a
    // reason to have no monitoring — measured from the machine this runs
    // on, the same request took 0.7 s, 2.2 s and 4.4 s in succession.
    // A gate still gives up, because a gate that cannot establish the
    // clock cannot sign the reads it exists to make.
    let mut synced = false;
    for attempt in 1..=5 {
        match venue.sync_clock() {
            Ok(offset) => {
                // Reported because a signature failure caused by drift
                // reads as an authentication problem, and the number
                // that would have explained it is this one.
                eprintln!("venue clock offset: {offset} ms");
                if offset < -1_000 {
                    eprintln!(
                        "  the local clock is more than a second ahead of the venue; \
                         Binance allows only 1000 ms in that direction whatever \
                         recvWindow says, so signed requests may be refused"
                    );
                }
                synced = true;
                break;
            }
            Err(e) => {
                eprintln!("could not read the venue clock (attempt {attempt}/5): {e}");
                if attempt < 5 {
                    std::thread::sleep(Duration::from_secs(2 * attempt));
                }
            }
        }
    }
    if !synced {
        // 3, not 1. Nothing was compared, so nothing diverged; the
        // account is unread, which is a different thing to tell a caller.
        eprintln!("giving up on the venue clock; signed reads would be refused");
        return std::process::ExitCode::from(3);
    }

    let describe_only = !watching && expected.legs.is_empty() && expected.working_orders.is_empty();
    let tolerance = Tolerance::default();
    let mut watcher = Watcher::new();

    loop {
        match read(&venue, &symbol) {
            Err(failed) => {
                // A partial read is never diffed. Everything it had not
                // reached would look like it had vanished from the venue.
                watcher.incomplete();
                for (part, why) in &failed {
                    // The reason, not just the name. Unattended for weeks,
                    // the difference between a rate limit and a dropped
                    // link is the difference between backing off and
                    // fixing the network — and it is only in this string.
                    eprintln!("read failed: {} — {why}", part.name());
                }
                eprintln!("incomplete read, nothing compared");
                if interval.is_none() {
                    // Single-shot is the startup-gate mode. Exiting zero
                    // here would report "the account matches" on the
                    // strength of a read that never happened.
                    return std::process::ExitCode::from(3);
                }
            }
            Ok(snapshot) => {
                if watching {
                    let first = watcher.tally.reads == 0;
                    let changes = watcher.observe(&snapshot);
                    if first {
                        println!("{}", describe(&snapshot));
                        println!("watching. every line below is something that changed.");
                    }
                    for c in &changes {
                        println!("  [{}] {}", snapshot.read_at_ms(), c.describe());
                    }
                    // Periodically, so a long quiet stretch still shows
                    // that the watch is alive and reading.
                    if watcher.tally.reads % 60 == 0 {
                        println!("  -- {}", watcher.tally.render());
                    }
                } else if describe_only {
                    println!("{}", describe(&snapshot));
                } else {
                    let outcome = reconcile(&expected, &snapshot, tolerance);
                    print!("{}", outcome.render());
                    if outcome.is_fatal() {
                        return std::process::ExitCode::from(1);
                    }
                }
            }
        }

        let Some(secs) = interval else {
            return std::process::ExitCode::SUCCESS;
        };
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// Read every part, or report which parts failed **and why**.
///
/// A failed read leaves its part unset rather than substituting an empty
/// result: an empty position list and a failed position query are the
/// same value and opposite facts.
///
/// The reason travels with the part. Discarding it — `if let Ok(x)` —
/// costs nothing at the call site and everything at 3am, when the log
/// says a part is missing and cannot say whether the venue refused, the
/// link dropped, or the request was throttled.
fn read(
    venue: &Binance,
    symbol: &str,
) -> Result<oq_gateway::Snapshot, Vec<(oq_gateway::Part, String)>> {
    let mut b = SnapshotBuilder::new(symbol);
    let mut why: Vec<(Part, String)> = Vec::new();

    match venue.account() {
        Ok(a) => b = b.account(a),
        Err(e) => why.push((Part::Account, e.to_string())),
    }
    match venue.positions(symbol) {
        Ok(p) => b = b.positions(p),
        Err(e) => why.push((Part::Positions, e.to_string())),
    }
    match venue.open_orders(symbol) {
        Ok(o) => b = b.open_orders(o),
        Err(e) => why.push((Part::OpenOrders, e.to_string())),
    }

    b.seal().map_err(|missing| explain(&missing, why))
}

/// Pair each missing part with its recorded reason.
///
/// A part can only be missing because its read failed, so every entry
/// should find one. It is still written to survive the case where it does
/// not: a monitor that panics while reporting a failure reports nothing.
fn explain(missing: &[Part], mut why: Vec<(Part, String)>) -> Vec<(Part, String)> {
    missing
        .iter()
        .map(|&part| {
            let reason = why
                .iter()
                .position(|(p, _)| *p == part)
                .map_or_else(|| "no reason recorded".to_string(), |i| why.remove(i).1);
            (part, reason)
        })
        .collect()
}

fn describe(s: &oq_gateway::Snapshot) -> String {
    let mut out = format!(
        "{} at {}\n  wallet {:.2}  unrealized {:.2}  margin balance {:.2}\n",
        s.symbol,
        s.account.read_at_ms,
        s.account.wallet_balance,
        s.account.unrealized,
        s.account.margin_balance,
    );
    if s.positions.is_empty() {
        out.push_str("  no open position\n");
    }
    for p in &s.positions {
        out.push_str(&format!(
            "  {} {} @ {}  (unrealized {:.2})\n",
            p.position_side, p.amount, p.entry_price, p.unrealized
        ));
    }
    if s.open_orders.is_empty() {
        out.push_str("  no resting orders\n");
    }
    for o in &s.open_orders {
        out.push_str(&format!(
            "  order {} ({}) {} {} @ {} filled {}/{} [{}]\n",
            o.client_order_id,
            o.order_id,
            o.side,
            o.position_side,
            o.price,
            o.executed_qty,
            o.orig_qty,
            o.status
        ));
    }
    // The expectation flags that would reproduce this state, so the
    // first run tells you how to invoke the second.
    out.push_str("\nto watch this state:\n  oq-recon ");
    out.push_str(&s.symbol);
    for p in &s.positions {
        let flag = if p.position_side == "SHORT" {
            "--expect-short"
        } else {
            "--expect-long"
        };
        out.push_str(&format!(" {flag} {}@{}", p.amount, p.entry_price));
    }
    for o in &s.open_orders {
        out.push_str(&format!(" --order {}", o.client_order_id));
    }
    out.push_str(" --interval 10\n");
    out
}

/// Parse `QTY@PRICE`.
fn parse_leg(side: &str, spec: &str) -> Result<ExpectedLeg, String> {
    let (qty, price) = spec
        .split_once('@')
        .ok_or_else(|| format!("expected QTY@PRICE, got {spec:?}"))?;
    Ok(ExpectedLeg {
        side: side.to_string(),
        amount: qty
            .trim()
            .parse()
            .map_err(|_| format!("{qty:?} is not a quantity"))?,
        entry_price: price
            .trim()
            .parse()
            .map_err(|_| format!("{price:?} is not a price"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leg_spec_parses() {
        let leg = parse_leg("LONG", "0.256@71444.87").expect("parses");
        assert_eq!(leg.side, "LONG");
        assert!((leg.amount - 0.256).abs() < 1e-12);
        assert!((leg.entry_price - 71_444.87).abs() < 1e-9);
    }

    /// A short leg is written with its sign, because that is how the
    /// venue reports it and a comparison should never turn on a
    /// translation the operator has to remember.
    #[test]
    fn a_short_leg_keeps_its_sign() {
        let leg = parse_leg("SHORT", "-0.004@62820.4").expect("parses");
        assert!((leg.amount + 0.004).abs() < 1e-12);
    }

    /// The reason reaches the operator attached to the part it belongs
    /// to. Four of 124 live reads came back partial; a log that names the
    /// part without the cause turns each of those into an investigation.
    #[test]
    fn every_missing_part_is_reported_with_its_reason() {
        let out = explain(
            &[Part::Account, Part::OpenOrders],
            vec![
                (Part::OpenOrders, "http 429: too many requests".into()),
                (Part::Account, "timed out after 45s".into()),
            ],
        );
        assert_eq!(
            out,
            vec![
                (Part::Account, "timed out after 45s".to_string()),
                (Part::OpenOrders, "http 429: too many requests".to_string()),
            ]
        );
    }

    /// Reporting a failure must not itself fail. If the bookkeeping ever
    /// loses a reason, the part is still named.
    #[test]
    fn a_missing_reason_still_names_the_part() {
        let out = explain(&[Part::Positions], vec![]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Part::Positions);
        assert!(out[0].1.contains("no reason"), "{:?}", out[0].1);
    }

    #[test]
    fn a_malformed_spec_is_refused_with_the_input_in_the_message() {
        let err = parse_leg("LONG", "0.256").expect_err("must fail");
        assert!(err.contains("0.256"), "{err}");
        assert!(parse_leg("LONG", "x@1").is_err());
        assert!(parse_leg("LONG", "1@x").is_err());
    }
}
