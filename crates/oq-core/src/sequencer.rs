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
use crate::sink::EventSink;
use oq_journal::{Reader, SyncPolicy, Writer};
use std::path::Path;

/// A kernel with a journal in front of it.
///
/// Generic over the sink so that the ordering guarantee — record, then
/// apply — can be tested by making the sink fail. An invariant that
/// only exists in documentation is one a refactor can remove silently.
#[derive(Debug)]
pub struct Sequencer<S: EventSink = Writer> {
    kernel: Kernel,
    sink: S,
    applied: u64,
}

impl<S: EventSink> Sequencer<S> {
    /// Attach an arbitrary sink to a fresh kernel.
    #[must_use]
    pub fn with_sink(state: State, sink: S) -> Self {
        Self {
            kernel: Kernel::new(state),
            sink,
            applied: 0,
        }
    }

    /// The sink, for inspection.
    #[must_use]
    pub const fn sink(&self) -> &S {
        &self.sink
    }
}

impl Sequencer<Writer> {
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
            sink: Writer::open(path, policy)?,
            applied: 0,
        })
    }
}

impl<S: EventSink> Sequencer<S> {
    /// Number, journal, then apply.
    ///
    /// # Errors
    /// I/O failures from the journal. The event is *not* applied if it
    /// could not be recorded: acting on an event that was not durably
    /// captured is the failure this ordering exists to prevent.
    pub fn submit(&mut self, event: &Event) -> oq_journal::Result<&[Output]> {
        // The `?` is the guarantee: a sink failure returns before the
        // kernel is touched, so a rejected event leaves no trace in
        // state. Reordering these two lines is the failure this crate
        // exists to prevent, and `a_failed_append_does_not_reach_the_kernel`
        // is what stops it.
        self.sink.append(event.kind(), &event.encode())?;
        self.applied += 1;
        Ok(self.kernel.apply(event))
    }

    /// Flush the journal to the OS.
    ///
    /// # Errors
    /// I/O failures.
    pub fn flush(&mut self) -> oq_journal::Result<()> {
        self.sink.flush()
    }

    /// Flush and fsync.
    ///
    /// # Errors
    /// I/O failures.
    pub fn sync(&mut self) -> oq_journal::Result<()> {
        self.sink.sync()
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
    let result = replay_tolerating_unknown(state, path)?;
    if result.undecodable > 0 {
        // Unknown state is fatal. A reconstruction that skipped records
        // is not the run that happened, and returning it as success
        // would let a caller act on a state nobody produced. Callers
        // that genuinely want a partial view ask for it by name.
        return Err(oq_journal::JournalError::Corrupt {
            at_offset: 0,
            cause: oq_journal::FrameError::UnknownVersion { found: u16::MAX },
        });
    }
    Ok(result)
}

/// Replay, accepting records this build cannot decode.
///
/// For forensics on a journal written by a newer build, where a partial
/// reconstruction is better than none. Never for recovery: the returned
/// state is not the state that ran, and
/// [`ReplayResult::is_complete`] is the only thing distinguishing them.
///
/// # Errors
/// I/O failures, or corruption before the end of the journal.
pub fn replay_tolerating_unknown(
    state: State,
    path: impl AsRef<Path>,
) -> oq_journal::Result<ReplayResult> {
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
                offset: oq_types::Offset::Open,
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
                offset: oq_types::Offset::Open,
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
            // Deliberately left resting at the end of the scenario. An
            // earlier version of this test filled every order, so the
            // book was empty in both arms and a replay that rebuilt it
            // incorrectly would still have passed.
            Event::Submit {
                id: OrderId::new(3),
                side: Side::Buy,
                price: Some(PriceTicks(500_000)),
                qty: QtyLots(4),
                stamp: Stamp::synthetic(5),
                offset: oq_types::Offset::Open,
            },
            // And one that is cancelled, so the working set is exercised
            // in both directions.
            Event::Submit {
                id: OrderId::new(4),
                side: Side::Sell,
                price: Some(PriceTicks(2_000_000)),
                qty: QtyLots(1),
                stamp: Stamp::synthetic(5),
                offset: oq_types::Offset::Open,
            },
            Event::Cancel {
                id: OrderId::new(4),
                stamp: Stamp::synthetic(6),
            },
        ]
    }

    /// SplitMix64: a small, well-distributed generator with no lattice
    /// structure at fixed strides. Used so the property tests draw
    /// varied scenarios without a dependency, and so a failure
    /// reproduces from its seed alone.
    struct SplitMix64(u64);

    impl SplitMix64 {
        const fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A pseudo-random but reproducible event sequence.
    fn generated_scenario(seed: u64, len: usize) -> Vec<Event> {
        let mut rng = SplitMix64::new(seed);
        let mut events = Vec::with_capacity(len);
        let mut price: i64 = 1_000_000;
        let mut next_id = 1u64;
        for i in 0..len {
            let step = rng.below(20_001) as i64 - 10_000;
            price = (price + step).max(1_000);
            let spread = rng.below(5_000) as i64;
            events.push(Event::Tick(Tick::trades_only(
                Stamp::synthetic(i as i64),
                price,
                price + spread,
                (price - spread).max(1),
            )));
            match rng.below(10) {
                0..=3 => {
                    next_id += 1;
                    let side = if rng.below(2) == 0 {
                        Side::Buy
                    } else {
                        Side::Sell
                    };
                    let offset = rng.below(50_000) as i64;
                    let limit = if side == Side::Buy {
                        (price - offset).max(1)
                    } else {
                        price + offset
                    };
                    events.push(Event::Submit {
                        id: OrderId::new(next_id),
                        side,
                        price: Some(PriceTicks(limit)),
                        qty: QtyLots(1 + rng.below(8) as i64),
                        stamp: Stamp::synthetic(i as i64),
                        offset: oq_types::Offset::Open,
                    });
                }
                4 => {
                    events.push(Event::Cancel {
                        id: OrderId::new(1 + rng.below(next_id.max(1))),
                        stamp: Stamp::synthetic(i as i64),
                    });
                }
                5 => {
                    events.push(Event::Funding {
                        at: Nanos(i as i64),
                        rate: Ratio::from_ppm(rng.below(400) as i64 - 200),
                        mark: PriceTicks(price),
                    });
                }
                _ => {}
            }
        }
        events
    }

    #[test]
    fn a_replay_reproduces_the_run_exactly() {
        // The property the whole architecture is built to provide,
        // asserted rather than assumed.
        let path = temp_path("determinism");
        let mut live_outputs = Vec::new();
        let live_fingerprint: crate::Fingerprint;
        {
            let mut seq = Sequencer::open(fresh_state(), &path, SyncPolicy::EveryRecordNoFsync)
                .expect("open");
            for event in scenario() {
                live_outputs.extend_from_slice(seq.submit(&event).expect("submit"));
            }
            seq.sync().expect("sync");
            live_fingerprint = seq.kernel().fingerprint();
        }
        assert!(
            !live_fingerprint.book.is_empty(),
            "the scenario must leave orders resting, or this test cannot \
             see whether the book was reproduced at all"
        );

        let replayed = replay(fresh_state(), &path).expect("replay");
        assert!(replayed.is_complete(), "every record must decode");
        assert_eq!(replayed.events, scenario().len());
        assert_eq!(
            replayed.outputs, live_outputs,
            "replay must produce identical outputs"
        );
        // The whole state, not the account projection: a replay that
        // rebuilt the order book incorrectly must fail here.
        assert_eq!(
            replayed.kernel.fingerprint(),
            live_fingerprint,
            "replay must produce identical state, book included"
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
        assert_eq!(a.kernel.fingerprint(), b.kernel.fingerprint());
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
        let live_summary: crate::Fingerprint;
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
                offset: oq_types::Offset::Open,
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
            live_summary = seq.kernel().fingerprint();
        }

        let replayed = replay(thin(), &path).expect("replay");
        assert!(
            replayed
                .outputs
                .iter()
                .any(|o| matches!(o, Output::Liquidated { .. })),
            "the liquidation must survive the replay"
        );
        assert_eq!(replayed.kernel.fingerprint(), live_summary);
        std::fs::remove_file(&path).ok();
    }

    /// A sink that fails on demand, so the ordering guarantee can be
    /// tested instead of merely documented.
    struct FailingSink {
        fail_after: usize,
        appended: usize,
    }

    impl crate::sink::EventSink for FailingSink {
        fn append(&mut self, _kind: u16, _payload: &[u8]) -> oq_journal::Result<u64> {
            if self.appended >= self.fail_after {
                return Err(oq_journal::JournalError::Io(std::io::Error::other(
                    "injected append failure",
                )));
            }
            let seq = self.appended as u64;
            self.appended += 1;
            Ok(seq)
        }
        fn flush(&mut self) -> oq_journal::Result<()> {
            Ok(())
        }
        fn sync(&mut self) -> oq_journal::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_append_does_not_reach_the_kernel() {
        // The central claim of this crate, as an assertion. An
        // implementation that applied first and journalled second would
        // pass every other test in this file and fail this one.
        let mut seq = Sequencer::with_sink(
            fresh_state(),
            FailingSink {
                fail_after: 2,
                appended: 0,
            },
        );
        let events = scenario();
        seq.submit(&events[0]).expect("first append succeeds");
        seq.submit(&events[1]).expect("second append succeeds");

        let before = seq.kernel().fingerprint();
        let applied_before = seq.applied();

        let err = seq.submit(&events[2]).expect_err("third append must fail");
        assert!(matches!(err, oq_journal::JournalError::Io(_)));
        assert_eq!(
            seq.applied(),
            applied_before,
            "a rejected event must not count as applied"
        );
        assert_eq!(
            seq.kernel().fingerprint(),
            before,
            "a rejected event must leave no trace in state"
        );
    }

    #[test]
    fn an_undecodable_record_is_an_error_not_a_partial_answer() {
        let path = temp_path("undecodable");
        {
            let mut w =
                oq_journal::Writer::open(&path, SyncPolicy::EveryRecordNoFsync).expect("open");
            let ev = Event::Time(Nanos(1));
            w.append(ev.kind(), &ev.encode()).expect("append");
            // A record kind this build does not know.
            w.append(60_000, b"from a newer build").expect("append");
            w.sync().expect("sync");
        }

        assert!(
            replay(fresh_state(), &path).is_err(),
            "recovery must refuse a reconstruction it knows is incomplete"
        );

        let tolerated =
            replay_tolerating_unknown(fresh_state(), &path).expect("forensic replay succeeds");
        assert_eq!(tolerated.undecodable, 1);
        assert!(!tolerated.is_complete());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_reproduces_generated_scenarios() {
        // Determinism as a property over many scenarios rather than one
        // hand-written sequence. A failure reproduces from its seed.
        for seed in 0..24u64 {
            let path = temp_path(&format!("prop-{seed}"));
            let events = generated_scenario(seed, 120);
            let live_outputs;
            let live_fingerprint;
            {
                let mut seq =
                    Sequencer::open(fresh_state(), &path, SyncPolicy::Never).expect("open");
                let mut outs = Vec::new();
                for event in &events {
                    outs.extend_from_slice(seq.submit(event).expect("submit"));
                }
                seq.sync().expect("sync");
                live_outputs = outs;
                live_fingerprint = seq.kernel().fingerprint();
            }

            let replayed = replay(fresh_state(), &path).expect("replay");
            assert_eq!(
                replayed.outputs, live_outputs,
                "seed {seed}: outputs diverged on replay"
            );
            assert_eq!(
                replayed.kernel.fingerprint(),
                live_fingerprint,
                "seed {seed}: state diverged on replay"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn generated_scenarios_actually_exercise_the_engine() {
        // A property test over scenarios that never trade would pass
        // for the wrong reason.
        let mut fills = 0usize;
        let mut resting = 0usize;
        for seed in 0..24u64 {
            let mut seq = Sequencer::with_sink(fresh_state(), crate::MemorySink::new());
            for event in &generated_scenario(seed, 120) {
                fills += seq
                    .submit(event)
                    .expect("submit")
                    .iter()
                    .filter(|o| matches!(o, Output::Filled(_)))
                    .count();
            }
            resting += seq.kernel().fingerprint().book.len();
        }
        assert!(fills > 50, "expected fills across seeds, got {fills}");
        assert!(resting > 0, "expected orders left resting, got {resting}");
    }
}
