//! Journal first, then the core.
//!
//! The sequencer is the only door into the kernel. It numbers an event,
//! appends it durably, and only then applies it. That ordering is not a
//! reliability nicety — it is what makes every other guarantee in the
//! system available:
//!
//! - Anything the core acted on is on disk, so "what happened" is never
//!   a question of log configuration.
//! - Recovery is a replay of the journal, which is the same code path
//!   as a normal run, so the recovery path is exercised continuously
//!   instead of once a year during an incident.
//! - An observer can follow the journal without touching the core, so
//!   monitoring cannot perturb what it monitors.
//!
//! The inverse order — apply, then journal — has a failure window in
//! which the core has acted on an event that no longer exists after a
//! crash. The state would then be unreproducible: a replay would
//! produce something different from what actually ran, and every
//! artifact derived from the journal would be quietly wrong.

use crate::event::Event;
use crate::kernel::{Kernel, Output, State};
use oq_journal::{Reader, SyncPolicy, Writer};
use std::path::Path;

/// A kernel with a journal in front of it.
#[derive(Debug)]
pub struct Sequencer {
    kernel: Kernel,
    writer: Writer,
    applied: u64,
}

impl Sequencer {
    /// Open a journal and attach it to a kernel.
    ///
    /// # Errors
    /// I/O failures, or corruption in the middle of an existing journal.
    pub fn open(
        state: State,
        path: impl AsRef<Path>,
        policy: SyncPolicy,
    ) -> oq_journal::Result<Self> {
        Ok(Self {
            kernel: Kernel::new(state),
            writer: Writer::open(path, policy)?,
            applied: 0,
        })
    }

    /// Number, journal, then apply.
    ///
    /// # Errors
    /// I/O failures from the journal. The event is *not* applied if it
    /// could not be recorded: acting on an event that was not durably
    /// captured is the failure this ordering exists to prevent.
    pub fn submit(&mut self, event: &Event) -> oq_journal::Result<&[Output]> {
        self.writer.append(event.kind(), &event.encode())?;
        self.applied += 1;
        Ok(self.kernel.apply(event))
    }

    /// Flush the journal to the OS.
    ///
    /// # Errors
    /// I/O failures.
    pub fn flush(&mut self) -> oq_journal::Result<()> {
        self.writer.flush()
    }

    /// Flush and fsync.
    ///
    /// # Errors
    /// I/O failures.
    pub fn sync(&mut self) -> oq_journal::Result<()> {
        self.writer.sync()
    }

    #[must_use]
    pub const fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// Events applied since this sequencer was opened.
    #[must_use]
    pub const fn applied(&self) -> u64 {
        self.applied
    }
}

/// Rebuild state by replaying a journal into a fresh kernel.
///
/// This is the recovery path, and it is also how a debugging session
/// reproduces a run exactly. A torn tail is tolerated — the last event
/// a crashed process never finished writing is not part of what
/// happened, and stopping cleanly there is the correct interpretation.
///
/// # Errors
/// I/O failures, corruption before the end of the journal, or a record
/// this build cannot decode.
pub fn replay(state: State, path: impl AsRef<Path>) -> oq_journal::Result<ReplayResult> {
    let reader = Reader::open(path)?;
    let replayed = reader.replay()?;
    let mut kernel = Kernel::new(state);
    let mut outputs = Vec::new();
    let mut undecodable = 0usize;

    for frame in &replayed.frames {
        match Event::decode(frame.kind, &frame.payload) {
            Some(event) => outputs.extend_from_slice(kernel.apply(&event)),
            // Counted rather than skipped silently: a journal this
            // build cannot fully read produces a state that is not the
            // original, and the caller has to be able to see that.
            None => undecodable += 1,
        }
    }

    Ok(ReplayResult {
        kernel,
        outputs,
        events: replayed.frames.len(),
        undecodable,
        stop: replayed.stop,
    })
}

/// What a replay reconstructed.
#[derive(Debug)]
pub struct ReplayResult {
    pub kernel: Kernel,
    pub outputs: Vec<Output>,
    pub events: usize,
    /// Records this build could not decode. Non-zero means the
    /// reconstruction is incomplete.
    pub undecodable: usize,
    pub stop: oq_journal::ReplayStop,
}

impl ReplayResult {
    /// Whether every record was understood.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.undecodable == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::Summary;
    use oq_engine::Tick;
    use oq_margin::{Contract, MarginTier, TierTable};
    use oq_types::{Cash, InstrumentId, Nanos, OrderId, PriceTicks, QtyLots, Ratio, Side, Stamp};

    const BTC: Contract = Contract::new(10_000);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oq-core-{}-{}-{}.journal",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }

    fn table() -> TierTable {
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket")
    }

    fn fresh_state() -> State {
        State::new(InstrumentId::new(1), BTC, table(), Cash::from_units(10_000))
    }

    /// A scenario with fills, funding, and a position that survives:
    /// enough moving parts that an accidental non-determinism would
    /// show up somewhere.
    fn scenario() -> Vec<Event> {
        vec![
            Event::Tick(Tick::trades_only(
                Stamp::synthetic(1),
                1_000_000,
                1_000_000,
                1_000_000,
            )),
            Event::Submit {
                id: OrderId::new(1),
                side: Side::Buy,
                price: Some(PriceTicks(990_000)),
                qty: QtyLots(10),
                stamp: Stamp::synthetic(1),
            },
            Event::Tick(Tick::trades_only(
                Stamp::synthetic(2),
                985_000,
                1_000_000,
                980_000,
            )),
            Event::Funding {
                at: Nanos::from_secs(28_800),
                rate: Ratio::from_ppm(100),
                mark: PriceTicks(985_000),
            },
            Event::Submit {
                id: OrderId::new(2),
                side: Side::Sell,
                price: Some(PriceTicks(1_005_000)),
                qty: QtyLots(10),
                stamp: Stamp::synthetic(3),
            },
            Event::Tick(Tick::quoted(
                Stamp::synthetic(4),
                1_010_000,
                1_012_000,
                1_000_000,
                1_009_000,
                1_011_000,
            )),
            Event::Time(Nanos::from_secs(30_000)),
        ]
    }

    #[test]
    fn a_replay_reproduces_the_run_exactly() {
        // The property the whole architecture is built to provide,
        // asserted rather than assumed.
        let path = temp_path("determinism");
        let mut live_outputs = Vec::new();
        let live_summary: Summary;
        {
            let mut seq = Sequencer::open(fresh_state(), &path, SyncPolicy::EveryRecordNoFsync)
                .expect("open");
            for event in scenario() {
                live_outputs.extend_from_slice(seq.submit(&event).expect("submit"));
            }
            seq.sync().expect("sync");
            live_summary = seq.kernel().summary();
        }

        let replayed = replay(fresh_state(), &path).expect("replay");
        assert!(replayed.is_complete(), "every record must decode");
        assert_eq!(replayed.events, scenario().len());
        assert_eq!(
            replayed.outputs, live_outputs,
            "replay must produce identical outputs"
        );
        assert_eq!(
            replayed.kernel.summary(),
            live_summary,
            "replay must produce identical state"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replaying_twice_gives_the_same_answer() {
        let path = temp_path("twice");
        {
            let mut seq = Sequencer::open(fresh_state(), &path, SyncPolicy::EveryRecordNoFsync)
                .expect("open");
            for event in scenario() {
                seq.submit(&event).expect("submit");
            }
            seq.sync().expect("sync");
        }
        let a = replay(fresh_state(), &path).expect("replay");
        let b = replay(fresh_state(), &path).expect("replay");
        assert_eq!(a.outputs, b.outputs);
        assert_eq!(a.kernel.summary(), b.kernel.summary());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_crash_mid_write_replays_up_to_the_last_whole_event() {
        let path = temp_path("crash");
        {
            let mut seq = Sequencer::open(fresh_state(), &path, SyncPolicy::EveryRecordNoFsync)
                .expect("open");
            for event in scenario() {
                seq.submit(&event).expect("submit");
            }
            seq.sync().expect("sync");
        }
        // Simulate a process that died partway through appending.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"\x4F\x51\x52\x4A\x01\x00").expect("partial");
        }

        let result = replay(fresh_state(), &path).expect("replay tolerates a torn tail");
        assert_eq!(result.events, scenario().len());
        assert!(matches!(
            result.stop,
            oq_journal::ReplayStop::TornTail { .. }
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_journal_records_every_event_the_core_acted_on() {
        let path = temp_path("complete");
        {
            let mut seq = Sequencer::open(fresh_state(), &path, SyncPolicy::EveryRecordNoFsync)
                .expect("open");
            for event in scenario() {
                seq.submit(&event).expect("submit");
            }
            seq.sync().expect("sync");
            assert_eq!(seq.applied(), scenario().len() as u64);
        }
        let frames = Reader::open(&path)
            .expect("open")
            .replay()
            .expect("replay")
            .frames;
        assert_eq!(frames.len(), scenario().len());
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.seq, i as u64, "sequence numbers are dense");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_liquidation_is_reproduced_by_replay() {
        // The path that matters most is also the one a replay must get
        // right: an account that was closed out stays closed out.
        let path = temp_path("liquidation");
        let thin = || State::new(InstrumentId::new(1), BTC, table(), Cash::from_units(100));
        let live_summary;
        {
            let mut seq =
                Sequencer::open(thin(), &path, SyncPolicy::EveryRecordNoFsync).expect("open");
            seq.submit(&Event::Tick(Tick::trades_only(
                Stamp::synthetic(1),
                1_200_000,
                1_200_000,
                1_200_000,
            )))
            .expect("submit");
            seq.submit(&Event::Submit {
                id: OrderId::new(1),
                side: Side::Buy,
                price: Some(PriceTicks(1_200_000)),
                qty: QtyLots(10),
                stamp: Stamp::synthetic(1),
            })
            .expect("submit");
            seq.submit(&Event::Tick(Tick::trades_only(
                Stamp::synthetic(2),
                1_200_000,
                1_200_000,
                1_200_000,
            )))
            .expect("submit");
            // Deep enough to trigger the venue.
            let outs = seq
                .submit(&Event::Tick(Tick::trades_only(
                    Stamp::synthetic(3),
                    1_000_000,
                    1_200_000,
                    1_000_000,
                )))
                .expect("submit")
                .to_vec();
            assert!(
                outs.iter().any(|o| matches!(o, Output::Liquidated { .. })),
                "expected a liquidation, got {outs:?}"
            );
            seq.sync().expect("sync");
            live_summary = seq.kernel().summary();
        }

        let replayed = replay(thin(), &path).expect("replay");
        assert!(
            replayed
                .outputs
                .iter()
                .any(|o| matches!(o, Output::Liquidated { .. })),
            "the liquidation must survive the replay"
        );
        assert_eq!(replayed.kernel.summary(), live_summary);
        std::fs::remove_file(&path).ok();
    }
}
