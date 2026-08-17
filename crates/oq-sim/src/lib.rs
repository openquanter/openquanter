//! The failures a venue actually produces, on purpose and on demand.
//!
//! Every component in this workspace has a documented behaviour for
//! something going wrong — a fill delivered twice, a report arriving out
//! of order, a stream that stops without closing. Those behaviours are
//! tested one at a time, by a test that constructs the one case it is
//! about. What is not tested is the combination, or the twentieth
//! occurrence, or the case nobody thought to write down.
//!
//! # Reproducible or it is not a test
//!
//! A fault injector that cannot reproduce its own findings is a
//! generator of anecdotes. Everything here is driven by a seeded
//! generator with no access to the clock, the environment or the
//! address space, so `(seed, commit)` names a run exactly. A failure
//! found on someone else's machine is a failure you can have.
//!
//! That is also why the generator is written here rather than taken as a
//! dependency: a crate that changed its algorithm in a patch release
//! would silently renumber every scenario, and the seed in a bug report
//! would stop meaning what it meant.
//!
//! # What this does not do
//!
//! It does not know about venues, sockets or orders. It produces
//! *distortions* — reorder this, duplicate that, drop the third — and
//! the crate under test applies them to its own events. A simulator that
//! understood the domain would have to be kept in step with it, and the
//! failure would be a simulator that stopped covering the thing it was
//! written for.

#![forbid(unsafe_code)]

/// A seeded generator.
///
/// xorshift64\*: small, well-understood, and — the property that matters
/// here — fixed. Its output is part of what a seed means, so it is
/// written out rather than depended on.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// # Panics
    /// A seed of zero, which this generator cannot leave.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        assert!(seed != 0, "xorshift cannot escape a zero state");
        Self { state: seed }
    }

    /// The next value.
    pub const fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value below `bound`, or zero when `bound` is zero.
    ///
    /// Modulo, and the bias is stated rather than hidden: for the bounds
    /// this is used with — counts of events in a scenario — the bias is
    /// far below anything a test could notice, and rejection sampling
    /// would make the number of generator calls depend on the value,
    /// which is worse here because it makes a seed's meaning depend on
    /// the bound.
    pub const fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next_u64() % bound
    }

    /// True with probability `numerator / denominator`.
    pub const fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        denominator != 0 && self.below(denominator) < numerator
    }
}

/// One way a sequence of events can be wrong.
///
/// Named after what a venue does, not after the code that handles it.
/// The list is the operational scar tissue this project has written down:
/// each of these has happened, and each has a documented handling
/// somewhere in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The same event delivered twice. A reconnecting stream redelivers.
    Duplicate,
    /// Two events swapped. Two sockets, one account.
    Reorder,
    /// An event never arrives. A stream that dropped while it was quiet.
    Drop,
    /// A run of events never arrives, and nothing says so.
    Gap { length: usize },
    /// The connection ends without a closing handshake.
    Disconnect,
    /// The stream stays open and stops carrying anything.
    Zombie,
    /// A timestamp jumps backwards. Clocks are corrected in production.
    ClockJump { back_by_ms: i64 },
}

/// A named, reproducible sequence of faults.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: &'static str,
    /// What the scenario is trying to break, in a sentence a failing test
    /// can print.
    pub about: &'static str,
    pub seed: u64,
    pub faults: Vec<Fault>,
}

/// The seeded corpus.
///
/// Every entry corresponds to a failure this project has met and written
/// a handling for. The seeds are fixed so a name identifies a run.
#[must_use]
pub fn corpus() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "redelivery-storm",
            about: "a reconnecting stream repeats what it already said",
            seed: 0x5EED_0001,
            faults: vec![Fault::Duplicate, Fault::Duplicate, Fault::Duplicate],
        },
        Scenario {
            name: "two-sockets-one-account",
            about: "reports interleaved by arrival rather than by the venue",
            seed: 0x5EED_0002,
            faults: vec![Fault::Reorder, Fault::Reorder],
        },
        Scenario {
            name: "lost-cancel",
            about: "a withdrawal that never arrives, so the order still rests",
            seed: 0x5EED_0003,
            faults: vec![Fault::Drop],
        },
        Scenario {
            name: "quiet-then-gone",
            about: "the socket ends without a handshake during a quiet period",
            seed: 0x5EED_0004,
            faults: vec![Fault::Zombie, Fault::Disconnect],
        },
        Scenario {
            name: "feed-gap",
            about: "a run of events missing with nothing to mark it",
            seed: 0x5EED_0005,
            faults: vec![Fault::Gap { length: 5 }],
        },
        Scenario {
            name: "clock-corrected-mid-run",
            about: "a timestamp earlier than the one before it",
            seed: 0x5EED_0006,
            faults: vec![Fault::ClockJump { back_by_ms: 250 }],
        },
        Scenario {
            name: "everything-at-once",
            about: "the combination nobody writes a single test for",
            seed: 0x5EED_0007,
            faults: vec![
                Fault::Duplicate,
                Fault::Reorder,
                Fault::Drop,
                Fault::Gap { length: 2 },
                Fault::Disconnect,
                Fault::ClockJump { back_by_ms: 100 },
            ],
        },
    ]
}

/// Apply a scenario's faults to a sequence, returning what a consumer
/// would actually receive.
///
/// Generic over the event so this crate never learns what an order is.
/// The positions are chosen by the scenario's seed, so the same scenario
/// distorts the same places every time.
#[must_use]
pub fn distort<T: Clone>(scenario: &Scenario, events: &[T]) -> Vec<T> {
    let mut rng = Rng::new(scenario.seed);
    let mut out: Vec<T> = events.to_vec();

    for fault in &scenario.faults {
        if out.is_empty() {
            break;
        }
        match *fault {
            Fault::Duplicate => {
                let at = usize::try_from(rng.below(out.len() as u64)).unwrap_or(0);
                let copy = out[at].clone();
                out.insert(at + 1, copy);
            }
            Fault::Reorder => {
                if out.len() >= 2 {
                    let at = usize::try_from(rng.below((out.len() - 1) as u64)).unwrap_or(0);
                    out.swap(at, at + 1);
                }
            }
            Fault::Drop => {
                let at = usize::try_from(rng.below(out.len() as u64)).unwrap_or(0);
                out.remove(at);
            }
            Fault::Gap { length } => {
                let at = usize::try_from(rng.below(out.len() as u64)).unwrap_or(0);
                let end = (at + length).min(out.len());
                out.drain(at..end);
            }
            // These three are about the connection rather than the
            // sequence, so they truncate or leave it alone: a consumer
            // sees what arrived before the socket stopped, and the
            // scenario's point is what the consumer does next.
            Fault::Disconnect | Fault::Zombie => {
                let at = usize::try_from(rng.below(out.len() as u64)).unwrap_or(0);
                out.truncate(at);
            }
            Fault::ClockJump { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_reproduces_a_run_exactly() {
        // The property without which this is a generator of anecdotes.
        let events: Vec<u32> = (0..40).collect();
        for s in corpus() {
            let a = distort(&s, &events);
            let b = distort(&s, &events);
            assert_eq!(a, b, "{} must be reproducible", s.name);
        }
    }

    #[test]
    fn two_scenarios_do_not_produce_the_same_distortion() {
        // Different seeds have to mean different runs, or the corpus is
        // one scenario with seven names.
        let events: Vec<u32> = (0..40).collect();
        let outs: Vec<Vec<u32>> = corpus().iter().map(|s| distort(s, &events)).collect();
        for (i, a) in outs.iter().enumerate() {
            for (j, b) in outs.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "scenarios {i} and {j} distort identically");
            }
        }
    }

    #[test]
    fn the_generator_is_the_one_written_here() {
        // The output is part of what a seed means. If this changes, every
        // seed in every bug report changes meaning, so the first values
        // are pinned rather than left to a dependency.
        let mut rng = Rng::new(1);
        let got: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                5_180_492_295_206_395_165,
                12_380_297_144_915_551_517,
                13_389_498_078_930_870_103
            ],
            "the generator changed; every recorded seed now means something else"
        );
    }

    #[test]
    fn a_duplicate_lengthens_and_a_drop_shortens() {
        let events: Vec<u32> = (0..10).collect();
        let dup = Scenario {
            name: "d",
            about: "one duplicate",
            seed: 7,
            faults: vec![Fault::Duplicate],
        };
        let drop = Scenario {
            name: "x",
            about: "one drop",
            seed: 7,
            faults: vec![Fault::Drop],
        };
        assert_eq!(distort(&dup, &events).len(), 11);
        assert_eq!(distort(&drop, &events).len(), 9);
    }

    #[test]
    fn a_duplicate_is_adjacent_to_its_original() {
        // A redelivery arrives next to the thing it repeats. A duplicate
        // placed anywhere else would be a different fault wearing this
        // name, and a deduplicator that only checks its neighbour would
        // pass a test it should not.
        let events: Vec<u32> = (0..10).collect();
        let s = Scenario {
            name: "d",
            about: "one duplicate",
            seed: 99,
            faults: vec![Fault::Duplicate],
        };
        let out = distort(&s, &events);
        let adjacent = out.windows(2).any(|w| w[0] == w[1]);
        assert!(adjacent, "{out:?}");
    }

    #[test]
    fn a_reorder_keeps_every_event_and_only_moves_one() {
        let events: Vec<u32> = (0..10).collect();
        let s = Scenario {
            name: "r",
            about: "one swap",
            seed: 5,
            faults: vec![Fault::Reorder],
        };
        let out = distort(&s, &events);
        assert_eq!(out.len(), events.len(), "nothing lost");
        let mut sorted = out.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, events, "the same events, differently ordered");
        assert_ne!(out, events, "and actually reordered");
    }

    #[test]
    fn a_gap_removes_a_run_rather_than_scattered_events() {
        // A feed gap is contiguous. Scattered losses are a different
        // failure with a different handling, and conflating them would
        // let a gap detector pass on data that has no gap in it.
        let events: Vec<u32> = (0..20).collect();
        let s = Scenario {
            name: "g",
            about: "a run of five",
            seed: 11,
            faults: vec![Fault::Gap { length: 5 }],
        };
        let out = distort(&s, &events);
        assert_eq!(out.len(), 15);
        let missing: Vec<u32> = events
            .iter()
            .filter(|e| !out.contains(e))
            .copied()
            .collect();
        let contiguous = missing.windows(2).all(|w| w[1] == w[0] + 1);
        assert!(contiguous, "the gap must be a run: {missing:?}");
    }

    #[test]
    fn an_empty_sequence_survives_every_scenario() {
        // A harness that panics on nothing is a harness that cannot be
        // run at the start of a session, which is where faults are most
        // interesting.
        for s in corpus() {
            let out: Vec<u32> = distort(&s, &[]);
            assert!(out.is_empty(), "{}", s.name);
        }
    }

    #[test]
    fn the_corpus_covers_the_written_catalogue() {
        // D8 lists the seeded patterns. A corpus that drifted from it
        // would leave a documented failure with nothing exercising it.
        let all: Vec<Fault> = corpus().into_iter().flat_map(|s| s.faults).collect();
        for wanted in [
            Fault::Duplicate,
            Fault::Reorder,
            Fault::Drop,
            Fault::Disconnect,
            Fault::Zombie,
        ] {
            assert!(
                all.contains(&wanted),
                "{wanted:?} is in the catalogue and not the corpus"
            );
        }
        assert!(all.iter().any(|f| matches!(f, Fault::Gap { .. })));
        assert!(all.iter().any(|f| matches!(f, Fault::ClockJump { .. })));
    }

    #[test]
    fn every_scenario_says_what_it_is_trying_to_break() {
        for s in corpus() {
            assert!(s.about.len() > 20, "{}: {:?}", s.name, s.about);
            assert!(s.seed != 0, "{}", s.name);
        }
    }
}
