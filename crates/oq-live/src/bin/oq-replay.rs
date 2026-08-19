//! `oq-replay` — read back what a live run decided.
//!
//! ```text
//! oq-replay oq-trade.oqj
//! oq-replay oq-trade.oqj --orders
//! ```
//!
//! The journal is written so a run can be reconstructed. Reconstruction
//! is the eventual point; reading is the immediate one, and until now the
//! only reader was the recovery path, which looks at one question and
//! ignores everything else.
//!
//! What this prints is the run as it happened, in the order it happened,
//! with the two questions a reader actually arrives with: what did it
//! decide, and is anything unaccounted for.

use std::collections::HashMap;
use std::process::ExitCode;

use oq_journal::Reader;
use oq_live::record::{OutcomeTag, Record};

const USAGE: &str = "\
oq-replay — read back what a live run decided

USAGE:
    oq-replay <FILE.oqj> [OPTIONS]

OPTIONS:
    --orders     Only the order lifecycle, without the ticks
    --help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::from(u8::from(args.is_empty()));
    }
    let path = &args[0];
    let orders_only = args.iter().any(|a| a == "--orders");

    let reader = match Reader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("oq-replay: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let replay = match reader.replay() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("oq-replay: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut ticks = 0u64;
    let mut submitted = 0u64;
    let mut refused = 0u64;
    let mut fills = 0u64;
    let mut undecodable = 0u64;
    let mut outcomes: HashMap<String, OutcomeTag> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for frame in replay.since(0) {
        match Record::decode(frame.kind, &frame.payload) {
            Some(Record::SessionStart {
                prefix,
                symbol,
                price_scale,
                qty_scale,
            }) => println!(
                "run              {symbol} as {prefix} (price {price_scale} dp, qty {qty_scale} dp)"
            ),
            Some(Record::Tick {
                at, last, bid, ask, ..
            }) => {
                ticks += 1;
                if !orders_only {
                    println!("tick {}  last {} bid {} ask {}", at.0, last.0, bid.0, ask.0);
                }
            }
            Some(Record::Submitted {
                at,
                client_id,
                side,
                limit_price,
                qty,
                reduce_only,
            }) => {
                submitted += 1;
                if !order.contains(&client_id) {
                    order.push(client_id.clone());
                }
                println!(
                    "sent {}  {client_id} {side:?} {} @ {}{}",
                    at.0,
                    qty.0,
                    limit_price.0,
                    if reduce_only { " reduce-only" } else { "" }
                );
            }
            Some(Record::Outcome {
                at,
                client_id,
                tag,
                detail,
            }) => {
                outcomes.insert(client_id.clone(), tag);
                println!("  → {} {client_id} {tag:?} {detail}", at.0);
            }
            Some(Record::Fill {
                at,
                client_id,
                trade_id,
                qty,
                price,
            }) => {
                fills += 1;
                println!(
                    "fill {at:?}  {client_id} trade {trade_id} {qty} @ {price}",
                    at = at.0
                );
            }
            Some(Record::Refused { at, breach }) => {
                refused += 1;
                println!("refused {}  {breach}", at.0);
            }
            Some(Record::Reconciled { at, legs }) => {
                // The legs individually, not a count. This record is what
                // a migration leaves behind, and "3 legs" does not tell a
                // reader whether the position this run took over is the
                // position the old one handed across.
                println!("reconciled {}  {} leg(s)", at.0, legs.len());
                for (symbol, side, lots, entry) in legs {
                    println!("           {symbol} {side} {lots} lots at {entry}");
                }
            }
            // A record this build does not know, or the torn tail of a
            // process that died mid-write. Counted, because a rising
            // number means the writer is ahead of the reader and that is
            // worth seeing before the numbers below are believed.
            None => undecodable += 1,
        }
    }

    println!();
    println!("ticks            {ticks}");
    println!("orders sent      {submitted}");
    println!("refused by gate  {refused}");
    println!("fills            {fills}");
    if undecodable > 0 {
        println!("unreadable       {undecodable} record(s) this build could not decode");
    }

    // The question the recovery path asks, asked here for a reader rather
    // than for a restart.
    let unaccounted: Vec<&String> = order
        .iter()
        .filter(|id| {
            !matches!(
                outcomes.get(*id),
                Some(OutcomeTag::Accepted | OutcomeTag::Rejected)
            )
        })
        .collect();
    if unaccounted.is_empty() {
        println!("unaccounted for  none");
        ExitCode::SUCCESS
    } else {
        println!("unaccounted for  {}:", unaccounted.len());
        for id in &unaccounted {
            println!("  - {id}");
        }
        println!();
        println!("Each of these was written down and never settled. They may be resting");
        println!("at the venue right now; oq-recon or a restart will say.");
        ExitCode::FAILURE
    }
}
