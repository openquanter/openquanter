//! Reinforcement-learning environments over the deterministic core.
//!
//! `FR-*`/**G10**: vectorized batch environments, seeds threaded through
//! every random source, and training runs that reproduce.
//!
//! # The inversion
//!
//! A backtest pushes: the host owns the loop, calls a strategy per
//! observation, and the strategy returns intents. An environment pulls:
//! the agent owns the loop, calls `step`, and gets an observation back.
//!
//! Bridging those usually means a thread and a channel, and that buys a
//! scheduler's non-determinism to solve a control-flow problem. Instead
//! the loop is taken apart: [`Env`] holds the kernel and a cursor, and
//! `step` advances exactly one observation. Nothing is concurrent,
//! nothing is buffered, and the same seed produces the same episode on
//! any machine — which is what G10's reproduction test is for.
//!
//! # What an action means here
//!
//! [`Action`] is a target position, not an order. An agent that emits
//! orders has to learn order management — ids, partial fills,
//! cancellation — before it can learn anything about a market, and
//! those are the parts this framework already does. So the environment
//! takes the position the agent wants and places what closes the gap.
//!
//! The cost is stated rather than hidden: an agent trained this way
//! cannot learn to work an order, because it never sees one.
//!
//! # What the reward is, and why that is a choice
//!
//! [`Reward`] is the change in equity over the step, in cash units. It
//! is the one definition that needs no parameter and no scaling
//! decision, which is why it is the default and not a recommendation.
//!
//! It is also known to be a poor training signal on its own: it is
//! dominated by market direction, so an agent can score well by holding
//! through a rise and learn nothing about execution. A risk-adjusted or
//! drawdown-penalised reward is the usual answer, and both introduce a
//! parameter that changes what is learned. Shaping one is the caller's
//! decision, and [`Step::equity`] is there so it can be made outside.

#![forbid(unsafe_code)]

pub mod vec;

use oq_backtest::{Observation, RunConfig, Tier};
use oq_core::matcher::Matcher;
use oq_core::{Event, Kernel, Output, State};
use oq_engine::Tick;
use oq_types::{Cash, InstrumentId, Offset, OrderId, QtyLots, Side};

pub use vec::VecEnv;

/// What the agent asks for: a position, in lots.
///
/// Signed, and absolute rather than incremental. An incremental action
/// makes the reachable position depend on every action before it, so
/// two agents that emitted the same action at the same observation are
/// in different states — which is a training signal about history
/// rather than about the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Action {
    /// The position the agent wants to hold after this step.
    pub target: QtyLots,
}

impl Action {
    /// Hold whatever is held.
    #[must_use]
    pub const fn hold(current: QtyLots) -> Self {
        Self { target: current }
    }

    /// A target position in lots.
    #[must_use]
    pub const fn target(lots: i64) -> Self {
        Self {
            target: QtyLots(lots),
        }
    }
}

/// What the agent sees.
///
/// Deliberately small. Every field is something the venue published or
/// the account holds — no indicators, no normalisation, no window. A
/// feature layer is `oq-features`' job, and one baked in here would be
/// one every agent inherits whether it wanted it or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observed {
    /// The observation that ended this step.
    pub tick: Tick,
    /// Signed position after the step.
    pub position: QtyLots,
    /// Average entry, zero when flat.
    pub entry: oq_types::PriceTicks,
    /// Account equity in its settlement currency.
    pub equity: Cash,
    /// Orders still resting.
    pub working: usize,
}

/// The result of one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub observed: Observed,
    /// Change in equity over this step, in cash units.
    pub reward: Cash,
    /// Equity after the step, so a caller shaping its own reward does
    /// not have to accumulate one.
    pub equity: Cash,
    /// Whether the episode ended, and why.
    pub done: Option<Ending>,
}

/// Why an episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The observations ran out.
    Exhausted,
    /// The account was liquidated.
    ///
    /// Distinct from running out, because an agent that reaches it has
    /// learned something different from one that survived to the end,
    /// and a single `done` flag hides which.
    Liquidated,
}

/// One episode over one instrument.
///
/// Not `Clone`: two environments sharing a cursor would step each
/// other's episodes, and the copy would look independent.
#[derive(Debug)]
pub struct Env {
    kernel: Kernel,
    stream: Vec<Observation>,
    cursor: usize,
    instrument: InstrumentId,
    next_id: u64,
    last_equity: Cash,
    ended: Option<Ending>,
    config: RunConfig,
    seed: u64,
}

impl Env {
    /// Build an environment over a stream of observations.
    ///
    /// The stream is owned rather than borrowed because an episode
    /// replays it from the start on every `reset`, and a borrowed one
    /// would tie every environment in a batch to one lifetime for no
    /// benefit — they share the data by cloning a `Vec` of `Copy` ticks
    /// once, not per step.
    #[must_use]
    pub fn new(config: RunConfig, stream: Vec<Observation>, seed: u64) -> Self {
        let instrument = config.instrument;
        let mut env = Self {
            kernel: Kernel::new(Self::state(&config)),
            stream,
            cursor: 0,
            instrument,
            next_id: 1,
            last_equity: config.starting_balance,
            ended: None,
            config,
            seed,
        };
        env.reset();
        env
    }

    fn state(config: &RunConfig) -> State {
        let matcher = match &config.tier {
            Tier::L0 => Matcher::l0(config.instrument),
            Tier::L1(p) => Matcher::L1(Box::new(oq_engine::L1Engine::new(config.instrument, *p))),
            Tier::L2(p) => Matcher::L2(Box::new(oq_engine::L2Engine::new(
                oq_engine::L1Engine::new(config.instrument, *p),
            ))),
        };
        State::new(
            config.instrument,
            config.contract,
            config.table.clone(),
            config.starting_balance,
        )
        .with_fees(config.fees)
        .with_mode(config.position_mode)
        .matching_with(matcher)
    }

    /// Start the episode over.
    ///
    /// Returns the first observation. The kernel is rebuilt rather than
    /// rewound: a reset that reused it would carry resting orders and a
    /// position into an episode the agent believes is fresh.
    pub fn reset(&mut self) -> Observed {
        self.kernel = Kernel::new(Self::state(&self.config));
        self.cursor = 0;
        self.next_id = 1;
        self.last_equity = self.config.starting_balance;
        self.ended = None;
        // Advance to the first tick so the agent's first action is taken
        // against a market it has seen.
        let observed = self.advance_to_tick();
        self.last_equity = observed.equity;
        observed
    }

    /// The seed this environment was built with.
    ///
    /// Every random source in an episode derives from it. There is none
    /// today — the matcher's latency draws are keyed on order ids, and
    /// the stream is fixed — and it is threaded through anyway, because
    /// a seed added once a source exists is a seed nobody's saved runs
    /// were produced with.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// How many observations remain.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.stream.len().saturating_sub(self.cursor)
    }

    /// Take one step.
    ///
    /// Places whatever closes the gap between the current position and
    /// the action's target, then advances to the next observation.
    /// Returns the same step repeatedly once the episode has ended,
    /// rather than panicking: a vectorized batch steps every
    /// environment together, and the ones that finished early have to
    /// answer something.
    pub fn step(&mut self, action: Action) -> Step {
        if let Some(ending) = self.ended {
            let observed = self.observe();
            return Step {
                observed,
                reward: Cash::ZERO,
                equity: observed.equity,
                done: Some(ending),
            };
        }

        let held = self.kernel.summary().qty;
        let delta = action.target.0 - held.0;
        if delta != 0 {
            let id = OrderId(self.next_id);
            self.next_id += 1;
            let side = if delta > 0 { Side::Buy } else { Side::Sell };
            // Reducing when the trade shrinks exposure rather than when
            // it reverses: a flip is a close and an open, and the kernel
            // splits it. Naming it `Open` throughout would post margin
            // for a leg being closed under hedge accounting.
            let offset = if held.0 != 0 && (held.0 > 0) != (delta > 0) {
                Offset::Close
            } else {
                Offset::Open
            };
            let stamp = self.kernel.summary().now;
            let outputs = self.kernel.apply(&Event::Submit {
                instrument: Some(self.instrument),
                id,
                side,
                price: None,
                qty: QtyLots(delta.abs()),
                offset,
                stamp: oq_types::Stamp::new(stamp.0, stamp.0),
            });
            if outputs
                .iter()
                .any(|o| matches!(o, Output::Liquidated { .. }))
            {
                self.ended = Some(Ending::Liquidated);
            }
        }

        let observed = self.advance_to_tick();
        let reward = observed.equity.sub(self.last_equity);
        self.last_equity = observed.equity;
        Step {
            observed,
            reward,
            equity: observed.equity,
            done: self.ended,
        }
    }

    /// Feed observations until one is a tick, and report the state after
    /// it.
    fn advance_to_tick(&mut self) -> Observed {
        while self.cursor < self.stream.len() {
            let observation = self.stream[self.cursor].clone();
            self.cursor += 1;
            match observation {
                Observation::Depth(u) => {
                    let _ = self.kernel.apply_depth(&u);
                }
                Observation::Snapshot {
                    update_id,
                    bids,
                    asks,
                } => {
                    self.kernel.install_snapshot(update_id, &bids, &asks);
                }
                Observation::Tick(tick) => {
                    let outputs = self.kernel.apply(&Event::Tick {
                        instrument: Some(self.instrument),
                        tick,
                    });
                    if outputs
                        .iter()
                        .any(|o| matches!(o, Output::Liquidated { .. }))
                    {
                        self.ended = Some(Ending::Liquidated);
                    }
                    return self.observe();
                }
            }
        }
        self.ended.get_or_insert(Ending::Exhausted);
        self.observe()
    }

    fn observe(&self) -> Observed {
        let s = self.kernel.summary();
        Observed {
            tick: Tick {
                stamp: oq_types::Stamp::new(s.now.0, s.now.0),
                last: s.mark,
                high: s.mark,
                low: s.mark,
                bid: s.mark,
                ask: s.mark,
                volume: QtyLots::ZERO,
            },
            position: s.qty,
            entry: s.entry,
            equity: s.equity,
            working: self.kernel.working().len(),
        }
    }
}

#[cfg(test)]
mod tests;
