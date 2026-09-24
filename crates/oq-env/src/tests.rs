//! What an environment has to guarantee before an agent trained in it
//! means anything.

use super::*;
use oq_margin::{Contract, MarginTier, TierTable};
use oq_types::{PriceTicks, Ratio, Stamp};

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

/// A rising market, so a long position earns and a short loses.
fn rising(n: i64) -> Vec<Observation> {
    (1..=n)
        .map(|i| {
            let price = 100_000 + i * 100;
            Observation::Tick(Tick {
                stamp: Stamp::new(i * 1_000_000, i * 1_000_000),
                last: PriceTicks(price),
                high: PriceTicks(price),
                low: PriceTicks(price),
                bid: PriceTicks(price),
                ask: PriceTicks(price),
                volume: QtyLots(1_000),
            })
        })
        .collect()
}

/// **G10's reproduction test.** The same seed and the same actions
/// produce the same episode, step for step.
#[test]
fn an_episode_reproduces_from_its_seed() {
    let run = |seed: u64| {
        let mut env = Env::new(config(), rising(50), seed);
        env.reset();
        let mut out = Vec::new();
        for i in 0..40 {
            out.push(env.step(Action::target(i % 5)));
        }
        out
    };

    assert_eq!(run(42), run(42), "same seed, same episode");
}

/// An action is a target, so the same action twice does nothing the
/// second time.
///
/// The property that makes actions comparable across states: an
/// incremental action would make the reachable position depend on every
/// action before it, and two agents that emitted the same one at the
/// same observation would be in different places.
#[test]
fn repeating_an_action_holds_rather_than_doubling() {
    let mut env = Env::new(config(), rising(20), 1);
    env.reset();

    let first = env.step(Action::target(5));
    assert_eq!(first.observed.position, QtyLots(5));

    let second = env.step(Action::target(5));
    assert_eq!(second.observed.position, QtyLots(5), "still five, not ten");
    assert_eq!(second.observed.working, 0, "and nothing was placed");
}

/// A long in a rising market is rewarded and a short is punished, which
/// is the least an environment has to get right.
#[test]
fn the_reward_follows_the_position_against_the_market() {
    let mut long = Env::new(config(), rising(30), 1);
    long.reset();
    long.step(Action::target(10));
    let long_total: i64 = (0..20)
        .map(|_| long.step(Action::target(10)).reward.0)
        .sum();

    let mut short = Env::new(config(), rising(30), 1);
    short.reset();
    short.step(Action::target(-10));
    let short_total: i64 = (0..20)
        .map(|_| short.step(Action::target(-10)).reward.0)
        .sum();

    assert!(long_total > 0, "a long earns in a rising market");
    assert!(short_total < 0, "a short does not");
}

/// Running out of observations ends the episode, and stepping past the
/// end is answered rather than panicking.
///
/// A batch steps every environment together, so the ones that finished
/// early have to return something.
#[test]
fn an_exhausted_episode_keeps_answering() {
    let mut env = Env::new(config(), rising(5), 1);
    env.reset();
    let mut last = None;
    for _ in 0..20 {
        last = Some(env.step(Action::target(1)));
    }
    let last = last.expect("stepped");
    assert_eq!(last.done, Some(Ending::Exhausted));
    assert_eq!(last.reward, Cash::ZERO, "and earns nothing after the end");
}

/// Liquidation is a different ending from running out.
///
/// An agent that reached it learned something an agent that survived
/// did not, and one `done` flag hides which.
#[test]
fn liquidation_is_distinguishable_from_exhaustion() {
    let thin = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(1_000),
        TierTable::new(vec![MarginTier {
            max_notional: Cash(i64::MAX),
            rate: Ratio::from_percent(50),
            amount: Cash::ZERO,
        }])
        .expect("single bracket"),
        Cash::from_units(10),
    );

    // A short in a rising market on a thin account.
    let mut env = Env::new(thin, rising(60), 1);
    env.reset();
    let mut ending = None;
    for _ in 0..50 {
        let step = env.step(Action::target(-500));
        if let Some(e) = step.done {
            ending = Some(e);
            break;
        }
    }
    assert_eq!(
        ending,
        Some(Ending::Liquidated),
        "the account should not survive this"
    );
}

/// Reset starts an episode, not a continuation.
///
/// A reset that reused the kernel would carry a position and resting
/// orders into an episode the agent believes is fresh — and the agent
/// would learn from a state it never chose.
#[test]
fn reset_clears_the_position_and_the_book() {
    let mut env = Env::new(config(), rising(30), 1);
    env.reset();
    env.step(Action::target(7));
    assert_eq!(env.kernel.summary().qty, QtyLots(7));

    let observed = env.reset();
    assert_eq!(observed.position, QtyLots::ZERO);
    assert_eq!(observed.working, 0);
    assert_eq!(env.remaining(), 29, "and the stream is back at the start");
}

// ---- Batches ----

/// Every environment in a batch runs the same episode when given the
/// same actions, because the stream is the same and nothing else varies
/// yet.
///
/// This is what makes a batch's spread meaningful later: any difference
/// between two environments has to come from their actions or their
/// seeds, and today there is no seeded source, so it must come from the
/// actions.
#[test]
fn a_batch_agrees_when_its_actions_agree() {
    let stream = rising(30);
    let mut batch = VecEnv::new(&config(), &stream, 8, 99);
    batch.reset();

    for _ in 0..20 {
        let steps = batch.step(&[Action::target(3); 8]);
        let first = steps[0];
        assert!(
            steps.iter().all(|s| *s == first),
            "identical actions, identical steps"
        );
    }
}

/// Different actions produce different outcomes in the same batch.
///
/// Without this the test above passes for a batch that ignores its
/// actions entirely.
#[test]
fn a_batch_separates_when_its_actions_do() {
    let stream = rising(30);
    let mut batch = VecEnv::new(&config(), &stream, 2, 99);
    batch.reset();

    let mut long_total = 0;
    let mut short_total = 0;
    for _ in 0..20 {
        let steps = batch.step(&[Action::target(10), Action::target(-10)]);
        long_total += steps[0].reward.0;
        short_total += steps[1].reward.0;
    }
    assert!(long_total > 0 && short_total < 0);
}

/// A batch reproduces from one number.
#[test]
fn a_batch_reproduces_from_its_seed() {
    let stream = rising(40);
    let run = || {
        let mut batch = VecEnv::new(&config(), &stream, 4, 7);
        batch.reset();
        (0..30)
            .map(|i| batch.step(&[Action::target(i % 3); 4]))
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

/// An action slice of the wrong length is refused rather than padded.
#[test]
#[should_panic(expected = "one action per environment")]
fn a_short_action_slice_is_refused() {
    let stream = rising(10);
    let mut batch = VecEnv::new(&config(), &stream, 4, 1);
    batch.step(&[Action::target(1)]);
}

/// A batch of nothing is refused: it steps nothing and returns an empty
/// result forever, which is a training loop that looks like it is
/// running.
#[test]
#[should_panic(expected = "at least one environment")]
fn an_empty_batch_is_refused() {
    let _ = VecEnv::new(&config(), &rising(10), 0, 1);
}

fn priced(prices: impl IntoIterator<Item = i64>) -> Vec<Observation> {
    prices
        .into_iter()
        .enumerate()
        .map(|(i, price)| {
            let at = (i as i64 + 1) * 1_000_000;
            Observation::Tick(Tick {
                stamp: Stamp::new(at, at),
                last: PriceTicks(price),
                high: PriceTicks(price),
                low: PriceTicks(price),
                bid: PriceTicks(price),
                ask: PriceTicks(price),
                volume: QtyLots(1_000),
            })
        })
        .collect()
}

fn final_equity(config: RunConfig, stream: Vec<Observation>) -> (Cash, Option<Ending>) {
    let mut env = Env::new(config, stream, 1);
    env.reset();
    let mut last = env.step(Action::target(10));
    while last.done.is_none() {
        last = env.step(Action::target(10));
    }
    (last.equity, last.done)
}

/// Funding reaches the reward. It was not settled at all, so a policy
/// that held through expensive funding learned it was free.
#[test]
fn a_long_held_through_positive_funding_pays_it() {
    let flat = priced([100_000; 20]);
    let funding = oq_margin::FundingSchedule::new(
        (5..15)
            .map(|i| oq_margin::FundingRate {
                at: oq_types::Nanos(i * 1_000_000 + 500_000),
                rate: Ratio::from_percent(1),
                mark: PriceTicks(100_000),
            })
            .collect(),
    );
    let (without, _) = final_equity(config(), flat.clone());
    let (with, _) = final_equity(config().with_funding(funding), flat);
    assert!(with < without, "funding paid: {with:?} vs {without:?}");
}

/// The margin mode the config names is the one the episode runs under,
/// as it is in the backtest.
#[test]
fn ignored_margin_does_not_end_an_episode_in_liquidation() {
    let thin = || {
        RunConfig::new(
            InstrumentId::new(1),
            Contract::new(1_000),
            TierTable::new(vec![MarginTier {
                max_notional: Cash(i64::MAX),
                rate: Ratio::from_percent(1),
                amount: Cash::ZERO,
            }])
            .expect("single bracket"),
            Cash::from_units(1),
        )
    };
    let falling = || priced((0..30).map(|i| 100_000 - i * 3_000));
    let (_, enforced) = final_equity(
        thin().with_margin(oq_backtest::MarginMode::Enforced),
        falling(),
    );
    assert_eq!(enforced, Some(Ending::Liquidated));
    let (_, ignored) = final_equity(
        thin().with_margin(oq_backtest::MarginMode::Ignored),
        falling(),
    );
    assert_eq!(ignored, Some(Ending::Exhausted));
}
