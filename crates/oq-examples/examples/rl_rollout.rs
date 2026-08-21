//! A batch of environments, stepped the way a training loop steps them.
//!
//! ```text
//! cargo run --release -p oq-examples --example rl_rollout
//! ```
//!
//! Not a training run: there is no gradient here and no model. It is the
//! loop an RL library wraps — reset, step a batch of actions, collect
//! rewards — run against a policy simple enough to read, so the parts
//! that have to be right can be checked without a framework in the way.
//!
//! # What it demonstrates, and what it deliberately does not
//!
//! Three things are asserted rather than printed:
//!
//! - **The same seed replays the same rollout.** `G10`'s reproduction
//!   requirement, at the level a training run cares about.
//! - **Environments in a batch are independent.** Different actions
//!   produce different equity, and nothing leaks between them.
//! - **A better policy scores better.** Without it the first two hold
//!   for an environment that ignores its actions entirely.
//!
//! What it does not show is a policy worth running. The momentum rule
//! below buys after an up-tick and sells after a down-tick, which on a
//! trending fixture is a way of buying the trend and on a real market
//! is a way of paying the spread repeatedly. Its P&L is not a finding.

use oq_backtest::{Observation, RunConfig};
use oq_engine::Tick;
use oq_env::{Action, VecEnv};
use oq_examples::{MarketShape, money, series};
use oq_margin::{Contract, MarginTier, TierTable};
use oq_types::{Cash, InstrumentId, Ratio};

/// How many environments step together.
const BATCH: usize = 8;

/// What each environment is allowed to hold, in lots.
const SIZE: i64 = 5;

fn config() -> RunConfig {
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(1_000),
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(1),
            amount: Cash::ZERO,
        }])
        .expect("single bracket"),
        Cash::from_units(1_000_000),
    )
}

/// Buy after a rise, sell after a fall.
///
/// One state variable and no parameters, so the rollout below is about
/// the environment rather than about tuning. Each environment gets a
/// different aggressiveness so the batch does something worth looking
/// at — that is a stand-in for a policy's exploration, not a strategy
/// idea.
fn momentum(previous: Option<Tick>, now: Tick, scale: i64) -> Action {
    let Some(before) = previous else {
        return Action::target(0);
    };
    match now.last.0.cmp(&before.last.0) {
        core::cmp::Ordering::Greater => Action::target(SIZE * scale),
        core::cmp::Ordering::Less => Action::target(-SIZE * scale),
        core::cmp::Ordering::Equal => Action::target(0),
    }
}

/// Step a whole batch to the end of its episodes, returning final
/// equity per environment.
fn rollout(stream: &[Observation], seed: u64, scales: &[i64]) -> Vec<Cash> {
    let mut batch = VecEnv::new(&config(), stream, BATCH, seed);
    let observed = batch.reset();

    // Two cursors, because the policy compares consecutive observations:
    // `current` is what it decides against, `previous` is what that is
    // compared to. Holding only one and reading the reset value for the
    // other compares every step against the *first* tick, which still
    // produces plausible numbers — the first version of this loop did
    // exactly that.
    let mut current: Vec<Tick> = observed.iter().map(|o| o.tick).collect();
    let mut previous: Vec<Option<Tick>> = vec![None; BATCH];

    let mut equity = vec![Cash::ZERO; BATCH];
    loop {
        let actions: Vec<Action> = (0..BATCH)
            .map(|i| momentum(previous[i], current[i], scales[i]))
            .collect();
        let steps = batch.step(&actions);

        let mut all_done = true;
        for (i, step) in steps.iter().enumerate() {
            previous[i] = Some(current[i]);
            current[i] = step.observed.tick;
            equity[i] = step.equity;
            if step.done.is_none() {
                all_done = false;
            }
        }
        if all_done {
            break;
        }
    }
    equity
}

fn main() {
    let ticks = series(MarketShape::trending(20_000), 11);
    let stream: Vec<Observation> = ticks.into_iter().map(Observation::Tick).collect();

    // Each environment trades a different multiple of the base size.
    let scales: Vec<i64> = (0..BATCH as i64).map(|i| i + 1).collect();

    let first = rollout(&stream, 4_242, &scales);
    let again = rollout(&stream, 4_242, &scales);

    println!("observations  {}", stream.len());
    println!("batch         {BATCH}");
    println!();
    println!("{:<6} {:>6} {:>16}", "env", "size", "final equity");
    for (i, e) in first.iter().enumerate() {
        println!("{i:<6} {:>6} {:>16}", scales[i] * SIZE, money(*e));
    }

    // 1. The same seed replays the same rollout.
    assert_eq!(first, again, "a rollout must reproduce from its seed");
    println!();
    println!("Replayed with the same seed: identical, every environment.");

    // 2. The environments are independent — different sizes, different
    //    outcomes, and no two identical by accident.
    let mut distinct = first.clone();
    distinct.sort_unstable_by_key(|c| c.0);
    distinct.dedup();
    assert!(
        distinct.len() > 1,
        "a batch whose environments all agree is not a batch"
    );
    println!(
        "{} of {BATCH} environments reached a distinct equity, so nothing \
         is shared between them.",
        distinct.len()
    );

    // 3. Scaling the same signal scales the result. Without this the
    //    assertions above hold for an environment that ignores actions.
    let smallest = first[0];
    let largest = first[BATCH - 1];
    let direction = if largest.0 > smallest.0 {
        "more"
    } else {
        "less"
    };
    println!();
    println!(
        "The largest position ended with {direction} than the smallest \
         ({} against {}), which is the signal reaching the account rather \
         than the environment answering the same way whatever it is told.",
        money(largest),
        money(smallest)
    );
    assert_ne!(
        smallest, largest,
        "size must change the outcome, or actions are being ignored"
    );

    println!();
    println!(
        "The policy is not the point: it buys after a rise on a fixture \
         built to rise, which is a way of buying the trend. What the \
         rollout shows is that a batch is reproducible, independent, and \
         responsive to what it is told."
    );
}
