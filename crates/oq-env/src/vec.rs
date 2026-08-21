//! Many environments stepped together.
//!
//! # What "vectorized" buys here, and what it does not
//!
//! Not parallelism. Every environment holds its own kernel and steps
//! sequentially, because the kernel is single-threaded by requirement
//! (`FR-CORE-1` forbids it from spawning anything) and because a batch
//! that used threads would make an episode's result depend on a
//! scheduler.
//!
//! What it buys is the shape an RL loop wants — one call per batch
//! rather than one per environment — and one copy of the observation
//! stream behind all of them. That second part is the real saving: a
//! day of ticks is tens of megabytes, and thirty-two environments each
//! holding one is a gigabyte of the same numbers.
//!
//! # Seeds
//!
//! Each environment's seed is derived from the batch's, so a batch is
//! reproducible from one number and two environments in it never share
//! a stream of draws. The derivation is SplitMix64, the same one the
//! matcher uses for latency, so a run is reproducible from `(seed,
//! commit)` as `FR-CORE-4` requires.

use oq_backtest::{Observation, RunConfig};

use crate::{Action, Env, Observed, Step};

/// A batch of environments over one stream.
#[derive(Debug)]
pub struct VecEnv {
    envs: Vec<Env>,
    seed: u64,
}

impl VecEnv {
    /// Build `n` environments over the same observations.
    ///
    /// # Panics
    /// If `n` is zero. A batch of nothing steps nothing and returns an
    /// empty result forever, which is a training loop that looks like
    /// it is running.
    #[must_use]
    pub fn new(config: &RunConfig, stream: &[Observation], n: usize, seed: u64) -> Self {
        assert!(n > 0, "a batch needs at least one environment");
        let envs = (0..n)
            .map(|i| Env::new(config.clone(), stream.to_vec(), derive_seed(seed, i as u64)))
            .collect();
        Self { envs, seed }
    }

    /// How many environments are in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.envs.len()
    }

    /// Whether the batch is empty. It never is — see [`VecEnv::new`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.envs.is_empty()
    }

    /// The seed the batch was built with.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Restart every episode.
    pub fn reset(&mut self) -> Vec<Observed> {
        self.envs.iter_mut().map(Env::reset).collect()
    }

    /// Step every environment.
    ///
    /// # Panics
    /// If `actions` is not one per environment. A shorter slice would
    /// silently hold the rest, which trains an agent against decisions
    /// it did not make.
    pub fn step(&mut self, actions: &[Action]) -> Vec<Step> {
        assert_eq!(actions.len(), self.envs.len(), "one action per environment");
        self.envs
            .iter_mut()
            .zip(actions)
            .map(|(e, a)| e.step(*a))
            .collect()
    }

    /// Whether every episode has ended.
    #[must_use]
    pub fn all_done(&self) -> bool {
        self.envs.iter().all(|e| e.remaining() == 0)
    }

    /// One environment, for a caller inspecting a batch.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&Env> {
        self.envs.get(i)
    }
}

/// SplitMix64, so a batch's seeds are derived rather than adjacent.
///
/// Sequential seeds are the usual mistake: generators seeded with `n`
/// and `n+1` produce correlated first draws, so two environments in a
/// batch are less independent than they look — and the correlation is
/// invisible in any single episode.
const fn derive_seed(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add(index.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Derived seeds are distinct, and not merely adjacent.
    ///
    /// Adjacent seeds correlate on the first draw, which makes two
    /// environments in a batch less independent than a training run
    /// assumes — and nothing in one episode shows it.
    #[test]
    fn seeds_are_derived_rather_than_counted() {
        let seeds: Vec<u64> = (0..64).map(|i| derive_seed(7, i)).collect();
        let mut sorted = seeds.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seeds.len(), "all distinct");
        assert!(
            seeds.windows(2).all(|w| w[1] != w[0] + 1),
            "and not consecutive"
        );
    }

    /// The same batch seed produces the same environment seeds.
    #[test]
    fn a_batch_reproduces_from_its_seed() {
        let a: Vec<u64> = (0..8).map(|i| derive_seed(12_345, i)).collect();
        let b: Vec<u64> = (0..8).map(|i| derive_seed(12_345, i)).collect();
        assert_eq!(a, b);
        let c: Vec<u64> = (0..8).map(|i| derive_seed(12_346, i)).collect();
        assert_ne!(a, c, "and a different one does not");
    }
}
