//! `oq-belief` — what the process thought it held, from its journal.
//!
//! `oq-recon` reads the venue. This reads the journal. During a
//! position-carrying cutover those are two different questions and only
//! one of them was answerable:
//!
//! ```text
//! oq-recon  BTCUSDT --record  before.txt   # step 2: write the account down
//! oq-recon  BTCUSDT --against before.txt   # step 5: has the venue moved
//! oq-belief run.oqj --against before.txt   # step 5: does the new process agree
//! ```
//!
//! The second catches the position changing under you. The third catches
//! the new process reading it wrong — which is what step 5 actually
//! risks, because it hands a live position to something that has never
//! seen it.
//!
//! Talks to no venue and needs no credentials. It reads one file.
//!
//! ## Exit codes
//!
//! `0` agrees, `1` differs, `2` the arguments were wrong, `3` the
//! journal could not be read. Four rather than two because "I could not
//! read it" must not be reported as "they agree", which is the reading
//! that lets a cutover proceed on a comparison that never happened.

use std::process::ExitCode;

use oq_gateway::record::Record;
use oq_live::belief::Belief;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: oq-belief <JOURNAL> [--against FILE] [--record FILE]\n\n\
                 \x20      --against  compare with a record oq-recon wrote\n\
                 \x20      --record   write this belief in the same format\n\n\
                 With neither, it prints what the journal says and exits.\n";

    let Some(journal) = args.first().filter(|a| !a.starts_with("--")) else {
        eprint!("{usage}");
        return ExitCode::from(2);
    };
    let value = |flag: &str| -> Option<&String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let against = value("--against");
    let record_to = value("--record");
    if against.is_some() && record_to.is_some() {
        eprintln!("--record and --against are separate steps; run one, then the other");
        return ExitCode::from(2);
    }

    let belief = match Belief::from_journal(journal) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("oq-belief: could not read {journal}: {e}");
            return ExitCode::from(3);
        }
    };

    println!("journal          {journal}");
    println!(
        "symbol           {}",
        belief.symbol.as_deref().unwrap_or("(none recorded)")
    );
    println!(
        "position         {} lots, entry {} ticks",
        belief.position_lots, belief.entry_ticks
    );
    println!("resting          {} order(s)", belief.resting.len());
    if !belief.adopted {
        // The distinction a flat reconstruction cannot make on its own.
        println!(
            "adoption         no record — a run before this was journalled that \
             carried a position reconstructs as flat"
        );
    }
    if belief.hedged {
        println!(
            "hedged           both legs were adopted; the netted position above \
             is not what the venue holds"
        );
    }
    if belief.undecodable > 0 {
        println!(
            "undecodable      {} record(s) skipped — the belief above is \
             incomplete by that much",
            belief.undecodable
        );
    }

    // The venue's clock is what oq-recon stamps with, and there is none
    // here. Zero rather than the local clock: a made-up timestamp in a
    // file that gets diffed against a real one is worse than an obvious
    // placeholder, and the comparison ignores it by design.
    let now = belief.to_record(0);

    if let Some(path) = record_to {
        return match std::fs::write(path, now.render()) {
            Ok(()) => {
                println!("recorded to {path}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("could not write {path}: {e}");
                ExitCode::from(3)
            }
        };
    }

    if let Some(path) = against {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("could not read {path}: {e}");
                return ExitCode::from(3);
            }
        };
        let recorded = match Record::parse(&text) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{path} is not a record: {e}");
                return ExitCode::from(3);
            }
        };
        let differences = recorded.differences(&now);
        if differences.is_empty() {
            println!("agrees with {path}");
            return ExitCode::SUCCESS;
        }
        println!("{} difference(s) from {path}:", differences.len());
        for d in &differences {
            println!("  - {d}");
        }
        // The sentence a cutover needs, because the two readings that
        // disagree here are the venue's and this process's, and only one
        // of them is authoritative.
        println!();
        println!(
            "The record is the venue. This is what the process believes. Where they \
             differ, the process is wrong."
        );
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}
