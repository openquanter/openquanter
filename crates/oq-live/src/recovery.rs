//! What a restarted process has to find out before it trades again.
//!
//! A crash between recording an order and hearing the venue's answer
//! leaves a journal that says an order was sent and does not say what
//! became of it. That is not a corrupt journal — it is the accurate
//! record of a process that stopped mid-question, and it is the reason
//! the record goes down before the order goes out.
//!
//! # The two states that matter, and they are different
//!
//! - A **submission with no outcome**: the process died between writing
//!   and hearing. The order may be resting right now.
//! - A **submission whose outcome was `Unknown`**: the process asked and
//!   the answer did not arrive. Same uncertainty, reached deliberately.
//!
//! Both are answered the same way — ask the venue about the client id —
//! and both must be answered *before* the strategy is given a tick. A
//! process that starts trading beside an order it does not know about is
//! the same failure as one that starts beside a position it does not
//! know about, and the startup check already refuses the second.
//!
//! # What this deliberately does not do
//!
//! It does not rebuild positions from the journal. The venue's own view
//! is authoritative and is already read at startup; a position
//! reconstructed from a record of intentions would be a second opinion
//! competing with the source of truth, and the interesting case — a fill
//! that happened while the process was down — is exactly the one the
//! journal cannot contain.

use std::collections::HashMap;
use std::path::Path;

use oq_journal::Reader;

use crate::record::{OutcomeTag, Record};

/// An order the journal cannot account for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// The id to ask the venue about.
    pub client_id: String,
    /// Why it is unaccounted for.
    pub reason: Unaccounted,
}

/// The two ways an order ends up unaccounted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unaccounted {
    /// Written, and then nothing. The process stopped mid-question.
    NoOutcome,
    /// Asked, and the answer never came.
    OutcomeUnknown,
}

/// What a journal says about the run that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovered {
    /// The id prefix that run used, if it recorded one.
    ///
    /// Worth carrying because a restart that chose a different prefix
    /// would not recognise its own previous orders on the account
    /// stream — it would classify them as another system's.
    pub prefix: Option<String>,
    pub symbol: Option<String>,
    /// Orders that need an answer before trading resumes.
    pub in_flight: Vec<InFlight>,
    /// Records the reader could not decode, usually the torn tail of a
    /// process that died mid-write. Counted rather than treated as
    /// corruption: it is the normal shape of a crash.
    pub undecodable: u64,
}

/// Read a journal and report what it leaves unresolved.
///
/// An **absent** journal recovers as an empty one, which is `oq-journal`'s
/// documented reading: a process starting for the first time has nothing
/// to replay. That distinction belongs to the caller, not here — only the
/// caller knows whether this is a first run or a restart, and a restart
/// finding no journal where it left one is the caller's alarm to raise.
/// What this function will not do is turn an unreadable journal into an
/// empty one, because that would start a process clean beside orders that
/// exist.
///
/// # Errors
/// Anything replaying the journal reports, and any I/O failure other than
/// the file being absent.
pub fn recover(path: impl AsRef<Path>) -> Result<Recovered, oq_journal::JournalError> {
    let reader = Reader::open(path)?;
    let replay = reader.replay()?;

    let mut out = Recovered::default();
    // Submitted ids in the order they were written, and the outcome that
    // arrived for each, if one did.
    let mut order: Vec<String> = Vec::new();
    let mut outcome: HashMap<String, OutcomeTag> = HashMap::new();

    for frame in replay.since(0) {
        match Record::decode(frame.kind, &frame.payload) {
            Some(Record::SessionStart { prefix, symbol, .. }) => {
                out.prefix = Some(prefix);
                out.symbol = Some(symbol);
            }
            Some(Record::Submitted { client_id, .. }) => {
                if !order.contains(&client_id) {
                    order.push(client_id);
                }
            }
            Some(Record::Outcome { client_id, tag, .. }) => {
                // A later outcome replaces an earlier one: resolving an
                // unknown is exactly what a restart is supposed to do,
                // and its record has to be able to say so.
                outcome.insert(client_id, tag);
            }
            Some(_) => {}
            None => out.undecodable += 1,
        }
    }

    for client_id in order {
        match outcome.get(&client_id) {
            None => out.in_flight.push(InFlight {
                client_id,
                reason: Unaccounted::NoOutcome,
            }),
            Some(OutcomeTag::Unknown) => out.in_flight.push(InFlight {
                client_id,
                reason: Unaccounted::OutcomeUnknown,
            }),
            Some(OutcomeTag::Accepted | OutcomeTag::Rejected) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_journal::{SyncPolicy, Writer};
    use oq_types::{Nanos, PriceTicks, QtyLots, Side};

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oq-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    fn write(path: &std::path::Path, records: &[Record]) {
        let mut w = Writer::open(path, SyncPolicy::EveryRecordNoFsync).expect("writer");
        for r in records {
            w.append(r.kind(), &r.encode()).expect("append");
        }
        w.flush().expect("flush");
    }

    fn submitted(id: &str) -> Record {
        Record::Submitted {
            at: Nanos(1),
            client_id: id.into(),
            side: Side::Buy,
            limit_price: PriceTicks(5),
            qty: QtyLots(1),
            reduce_only: false,
        }
    }

    fn outcome(id: &str, tag: OutcomeTag) -> Record {
        Record::Outcome {
            at: Nanos(2),
            client_id: id.into(),
            tag,
            detail: String::new(),
        }
    }

    fn start() -> Record {
        Record::SessionStart {
            prefix: "oq99".into(),
            symbol: "ETHUSDT".into(),
            price_scale: 2,
            qty_scale: 3,
        }
    }

    #[test]
    fn an_order_written_with_no_outcome_is_in_flight() {
        // The crash this whole ordering exists for: the record went down
        // and the process stopped before hearing anything.
        let p = temp("no-outcome.oqj");
        write(&p, &[start(), submitted("oq99-1")]);
        let r = recover(&p).expect("readable");
        assert_eq!(
            r.in_flight,
            vec![InFlight {
                client_id: "oq99-1".into(),
                reason: Unaccounted::NoOutcome
            }]
        );
    }

    #[test]
    fn an_order_whose_outcome_was_unknown_is_in_flight_too() {
        let p = temp("unknown.oqj");
        write(
            &p,
            &[
                start(),
                submitted("oq99-1"),
                outcome("oq99-1", OutcomeTag::Unknown),
            ],
        );
        let r = recover(&p).expect("readable");
        assert_eq!(r.in_flight.len(), 1);
        assert_eq!(r.in_flight[0].reason, Unaccounted::OutcomeUnknown);
    }

    #[test]
    fn a_settled_order_is_not_in_flight() {
        let p = temp("settled.oqj");
        write(
            &p,
            &[
                start(),
                submitted("oq99-1"),
                outcome("oq99-1", OutcomeTag::Accepted),
                submitted("oq99-2"),
                outcome("oq99-2", OutcomeTag::Rejected),
            ],
        );
        assert!(recover(&p).expect("readable").in_flight.is_empty());
    }

    #[test]
    fn an_unknown_that_was_later_resolved_is_settled() {
        // A restart resolving an unknown has to be able to record that it
        // did, or every restart inherits every previous restart's
        // uncertainty forever.
        let p = temp("resolved.oqj");
        write(
            &p,
            &[
                start(),
                submitted("oq99-1"),
                outcome("oq99-1", OutcomeTag::Unknown),
                outcome("oq99-1", OutcomeTag::Accepted),
            ],
        );
        assert!(recover(&p).expect("readable").in_flight.is_empty());
    }

    #[test]
    fn the_prefix_is_recovered_because_a_new_one_would_disown_old_orders() {
        // The account stream is filtered by client id prefix. A restart
        // that chose a fresh prefix would classify its own previous
        // orders as another system's and stop counting them.
        let p = temp("prefix.oqj");
        write(&p, &[start()]);
        let r = recover(&p).expect("readable");
        assert_eq!(r.prefix.as_deref(), Some("oq99"));
        assert_eq!(r.symbol.as_deref(), Some("ETHUSDT"));
    }

    #[test]
    fn in_flight_orders_come_back_in_the_order_they_were_sent() {
        // Resolving oldest first matters: the oldest is the one most
        // likely to have filled while the process was away.
        let p = temp("order.oqj");
        write(
            &p,
            &[
                start(),
                submitted("oq99-1"),
                submitted("oq99-2"),
                submitted("oq99-3"),
            ],
        );
        let ids: Vec<String> = recover(&p)
            .expect("readable")
            .in_flight
            .into_iter()
            .map(|f| f.client_id)
            .collect();
        assert_eq!(ids, vec!["oq99-1", "oq99-2", "oq99-3"]);
    }

    #[test]
    fn an_absent_journal_recovers_as_an_empty_one() {
        // `oq-journal`'s documented reading, and the right one: a first
        // run has nothing to replay and should not have to special-case
        // that. Whether an absence is expected is the caller's question —
        // only it knows whether this is a first run or a restart.
        let r = recover(temp("does-not-exist.oqj")).expect("absent is empty");
        assert!(r.in_flight.is_empty());
        assert_eq!(r.prefix, None);
    }

    #[test]
    fn a_torn_tail_is_counted_rather_than_treated_as_corruption() {
        // The normal shape of a process that died mid-write. Reporting it
        // as damage would make every crash look like data loss.
        let p = temp("torn.oqj");
        write(&p, &[start(), submitted("oq99-1")]);
        let mut bytes = std::fs::read(&p).expect("read");
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&p, &bytes).expect("write");
        let r = recover(&p).expect("a torn tail is readable");
        assert_eq!(r.prefix.as_deref(), Some("oq99"), "what was whole survives");
    }
}
