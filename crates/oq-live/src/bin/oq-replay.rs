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
        let Some(record) = Record::decode(frame.kind, &frame.payload) else {
            // A record this build does not know, or the torn tail of a
            // process that died mid-write. Counted, because a rising
            // number means the writer is ahead of the reader and that is
            // worth seeing before the numbers below are believed.
            undecodable += 1;
            continue;
        };
        match &record {
            Record::Tick { .. } => ticks += 1,
            Record::Submitted { client_id, .. } => {
                submitted += 1;
                if !order.contains(client_id) {
                    order.push(client_id.clone());
                }
            }
            Record::Outcome { client_id, tag, .. } => {
                outcomes.insert(client_id.clone(), *tag);
            }
            Record::Fill { .. } => fills += 1,
            Record::Refused { .. } => refused += 1,
            _ => {}
        }
        if orders_only && matches!(record, Record::Tick { .. }) {
            continue;
        }
        println!("{}", render(&record));
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

/// One record, as a reader sees it.
///
/// Separate from the loop so it can be tested, and it is tested for one
/// property: **every field of every record appears here**. A field that
/// is written and not shown does not exist to the person reading, which
/// is not a theoretical concern — `volume` was recorded from the first
/// version and never printed, and a twelve-hour run that placed no
/// orders was unexplainable until it was. The explanation was in that
/// column: the strategy's entry needed more volume between observations
/// than the deployment ever traded.
fn render(record: &Record) -> String {
    match record {
        Record::SessionStart {
            prefix,
            symbol,
            price_scale,
            qty_scale,
        } => format!(
            "run              {symbol} as {prefix} (price {price_scale} dp, qty {qty_scale} dp)"
        ),
        Record::Tick {
            at,
            last,
            bid,
            ask,
            volume,
        } => format!(
            "tick {}  last {} bid {} ask {} vol {}",
            at.0, last.0, bid.0, ask.0, volume.0
        ),
        Record::Submitted {
            at,
            client_id,
            side,
            limit_price,
            qty,
            reduce_only,
        } => format!(
            "sent {}  {client_id} {side:?} {} @ {}{}",
            at.0,
            qty.0,
            limit_price.0,
            if *reduce_only { " reduce-only" } else { "" }
        ),
        Record::Outcome {
            at,
            client_id,
            tag,
            detail,
        } => format!("  \u{2192} {} {client_id} {tag:?} {detail}", at.0),
        Record::Fill {
            at,
            client_id,
            trade_id,
            qty,
            price,
            order,
            side,
        } => format!(
            "fill {}  {client_id} #{order} {side} trade {trade_id} {qty} @ {price}",
            at.0
        ),
        Record::Refused { at, breach } => format!("refused {}  {breach}", at.0),
        Record::Waiting { at, entries } => {
            let body: Vec<String> = entries.iter().map(|(k, v)| format!("{k} {v}")).collect();
            format!("waiting {}  {}", at.0, body.join(", "))
        }
        Record::Reconciled { at, legs } => {
            // The legs individually, not a count. This record is what a
            // migration leaves behind, and "3 leg(s)" does not tell a
            // reader whether the position this run took over is the
            // position the old one handed across.
            let mut s = format!("reconciled {}  {} leg(s)", at.0, legs.len());
            for (symbol, side, lots, entry) in legs {
                s.push_str(&format!(
                    "\n           {symbol} {side} {lots} lots at {entry}"
                ));
            }
            s
        }
    }
}

#[cfg(test)]
mod readout {
    use super::render;
    use oq_live::record::{OutcomeTag, Record};
    use oq_types::{Nanos, PriceTicks, QtyLots, Side};

    /// Every field of every record reaches the reader.
    ///
    /// Each value below is distinctive, so a field the renderer drops
    /// cannot be matched by another field's value. The check is crude on
    /// purpose: it looks for *omission*, which is the failure that
    /// actually happened, and it needs no agreement about formatting to
    /// find it.
    ///
    /// `volume` was written by the first version of the journal and
    /// never printed. Nothing failed and nothing warned; a run that
    /// placed no orders simply had no explanation, because the
    /// explanation was in the column that was not shown. A record is
    /// only as good as its readout, and this keeps them equal.
    ///
    /// A new field will fail here until it is rendered. That is the
    /// point: adding one is a decision to show it.
    fn shows(record: &Record, values: &[&str]) {
        let out = render(record);
        for v in values {
            assert!(
                out.contains(v),
                "{v:?} is recorded but does not reach the reader.\n  rendered: {out}"
            );
        }
    }

    #[test]
    fn a_session_start_shows_all_of_itself() {
        shows(
            &Record::SessionStart {
                prefix: "zzprefix".into(),
                symbol: "ZZZUSDT".into(),
                price_scale: 7,
                qty_scale: 9,
            },
            &["zzprefix", "ZZZUSDT", "7", "9"],
        );
    }

    /// Including volume. This is the one that was missing.
    #[test]
    fn a_tick_shows_all_of_itself() {
        shows(
            &Record::Tick {
                at: Nanos(1_111),
                last: PriceTicks(2_222),
                bid: PriceTicks(3_333),
                ask: PriceTicks(4_444),
                volume: QtyLots(5_555),
            },
            &["1111", "2222", "3333", "4444", "5555"],
        );
    }

    #[test]
    fn a_submission_shows_all_of_itself() {
        shows(
            &Record::Submitted {
                at: Nanos(1_111),
                client_id: "zz-client".into(),
                side: Side::Sell,
                limit_price: PriceTicks(2_222),
                qty: QtyLots(3_333),
                reduce_only: true,
            },
            &["1111", "zz-client", "Sell", "2222", "3333", "reduce-only"],
        );
    }

    #[test]
    fn an_outcome_shows_all_of_itself() {
        shows(
            &Record::Outcome {
                at: Nanos(1_111),
                client_id: "zz-client".into(),
                tag: OutcomeTag::Rejected,
                detail: "zz-detail".into(),
            },
            &["1111", "zz-client", "Rejected", "zz-detail"],
        );
    }

    #[test]
    fn a_fill_shows_all_of_itself() {
        shows(
            &Record::Fill {
                at: Nanos(1_111),
                client_id: "zz-client".into(),
                trade_id: 2_222,
                qty: "3.333".into(),
                price: "4444.4".into(),
                order: 5_555,
                side: "zzSell".into(),
            },
            &[
                "1111",
                "zz-client",
                "2222",
                "3.333",
                "4444.4",
                "5555",
                "zzSell",
            ],
        );
    }

    #[test]
    fn a_refusal_shows_all_of_itself() {
        shows(
            &Record::Refused {
                at: Nanos(1_111),
                breach: "zz-breach".into(),
            },
            &["1111", "zz-breach"],
        );
    }

    /// The conditions, by name and value.
    ///
    /// This is the record that explains a run in which nothing
    /// happened, so a readout that summarised it — "3 conditions" —
    /// would leave the run exactly as unexplained as having no record
    /// at all.
    #[test]
    fn a_wait_shows_every_condition_it_names() {
        shows(
            &Record::Waiting {
                at: Nanos(1_111),
                entries: vec![("zz_bars".into(), 2_222), ("zz_gate".into(), 3_333)],
            },
            &["1111", "zz_bars", "2222", "zz_gate", "3333"],
        );
    }

    /// Both legs, and the entry basis on each.
    ///
    /// A count alone would pass a weaker check and tell a reader nothing
    /// about whether the position taken over is the position handed
    /// across — the only question this record exists to answer.
    #[test]
    fn an_adoption_shows_every_leg_and_its_basis() {
        shows(
            &Record::Reconciled {
                at: Nanos(1_111),
                legs: vec![
                    ("ZZZUSDT".into(), "LONG".into(), 2_222, 3_333),
                    ("ZZZUSDT".into(), "SHORT".into(), -4_444, 5_555),
                ],
            },
            &[
                "1111", "ZZZUSDT", "LONG", "2222", "3333", "SHORT", "-4444", "5555",
            ],
        );
    }
}
