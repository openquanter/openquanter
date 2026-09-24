//! Whether a strategy's decisions depend on data it could not yet have.
//!
//! A backtest that reads the future looks better than any live run can,
//! and nothing in its result says so. The usual way in is not a strategy
//! indexing past the current tick — the harness never hands it one — but
//! a strategy *built* with the whole window: indicators precomputed over
//! all of it, a normalisation fitted to its full range, a threshold
//! chosen from its distribution.
//!
//! The test is prefix invariance. Run once over the whole window and
//! record every intent and the tick it was sent on. Then, for a point
//! where the strategy acted, build a fresh strategy from the data **up to
//! that point only**, run it over the same prefix, and compare every
//! intent through that point. A strategy that uses only the past sends
//! the same orders on the same ticks either way; one that used the rest
//! of the window cannot, because the rest is not there. The engine is
//! deterministic, so the comparison is exact and any difference is the
//! strategy's.
//!
//! Each check reruns a prefix, so checking every signal is quadratic in
//! the window. Points are sampled evenly up to a bound and the report
//! says how many of how many were checked; a clean result on a sample is
//! a weaker statement than on all of them, and the report does not blur
//! the two.

use oq_engine::Tick;
use oq_strategy::{Context, Ending, Intent, Strategy};
use oq_types::{Fill, Nanos, OrderId};

use crate::run::{RunConfig, run};

/// The default bound on truncation points.
pub const DEFAULT_POINTS: usize = 200;

/// Intents in the order they were sent, each with the tick it belongs to.
///
/// A fill or an ending is delivered before the tick it arrived with, so
/// intents sent from those callbacks belong to that tick, not the one
/// before it.
struct Recorder<S> {
    inner: S,
    /// On-tick calls started so far.
    seen: usize,
    sent: Vec<(usize, Intent)>,
}

impl<S> Recorder<S> {
    const fn new(inner: S) -> Self {
        Self {
            inner,
            seen: 0,
            sent: Vec::new(),
        }
    }

    fn record(&mut self, tick: usize, out: &[Intent], from: usize) {
        self.sent
            .extend(out[from..].iter().map(|intent| (tick, *intent)));
    }
}

impl<S: Strategy> Strategy for Recorder<S> {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let tick = self.seen;
        self.seen += 1;
        let from = out.len();
        self.inner.on_tick(ctx, out);
        self.record(tick, out, from);
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &Context, out: &mut Vec<Intent>) {
        let from = out.len();
        self.inner.on_fill(fill, ctx, out);
        self.record(self.seen, out, from);
    }

    fn on_placed(&mut self, id: OrderId, accepted: bool) {
        self.inner.on_placed(id, accepted);
    }

    fn on_cancel_failed(&mut self, id: OrderId, why: &str) {
        self.inner.on_cancel_failed(id, why);
    }

    fn on_ended(&mut self, id: OrderId, ending: Ending, out: &mut Vec<Intent>) {
        let from = out.len();
        self.inner.on_ended(id, ending, out);
        self.record(self.seen, out, from);
    }

    fn on_history(&mut self, ctx: &Context) {
        self.inner.on_history(ctx);
    }

    fn on_history_fill(&mut self, fill: &Fill, ctx: &Context) {
        self.inner.on_history_fill(fill, ctx);
    }

    fn waiting_on(&self) -> Vec<(&'static str, i64)> {
        self.inner.waiting_on()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

/// The first tick at which the two runs sent different orders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The truncation point whose rerun disagreed.
    pub checked_at: usize,
    /// The tick where they first differ, at or before `checked_at`.
    pub tick: usize,
    /// That tick's exchange time.
    pub at: Nanos,
    /// What the run over the whole window sent on it.
    pub full: Vec<Intent>,
    /// What the run over the prefix sent on it.
    pub truncated: Vec<Intent>,
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookaheadReport {
    /// Ticks on which the full run sent anything: the candidate points.
    pub signals: usize,
    /// Of those, how many were rerun.
    pub checked: usize,
    /// Every checked point whose rerun disagreed.
    pub divergences: Vec<Divergence>,
}

impl LookaheadReport {
    /// No checked point disagreed.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.divergences.is_empty()
    }

    /// Fewer points were checked than there were signals.
    #[must_use]
    pub const fn sampled(&self) -> bool {
        self.checked < self.signals
    }
}

/// Up to `max` of `candidates`, evenly spread, the first and the last
/// always among them.
fn spread(candidates: &[usize], max: usize) -> Vec<usize> {
    if candidates.len() <= max {
        return candidates.to_vec();
    }
    if max == 0 {
        return Vec::new();
    }
    let n = candidates.len();
    let mut picked: Vec<usize> = (0..max)
        .map(|k| candidates[(k * (n - 1)) / (max - 1).max(1)])
        .collect();
    picked.dedup();
    picked
}

fn intents_on(sent: &[(usize, Intent)], tick: usize) -> Vec<Intent> {
    sent.iter()
        .filter(|(t, _)| *t == tick)
        .map(|(_, i)| *i)
        .collect()
}

/// Check that `build`'s strategy decides the same way on a prefix of
/// `ticks` as on all of them.
///
/// `build` is handed the data the strategy may know about — the whole
/// window for the reference run, the prefix for each rerun — and must
/// build the strategy from that and nothing else. A strategy that takes
/// no data ignores it.
///
/// At most `max_points` truncation points are rerun; see
/// [`DEFAULT_POINTS`].
pub fn lookahead<S, F>(
    config: &RunConfig,
    build: F,
    ticks: &[Tick],
    max_points: usize,
) -> LookaheadReport
where
    S: Strategy,
    F: Fn(&[Tick]) -> S,
{
    let mut reference = Recorder::new(build(ticks));
    run(config, &mut reference, ticks);
    let full = reference.sent;

    let mut candidates: Vec<usize> = full.iter().map(|(t, _)| *t).collect();
    candidates.dedup();
    let points = spread(&candidates, max_points);

    let mut divergences = Vec::new();
    for &point in &points {
        let prefix = &ticks[..=point];
        let mut rerun = Recorder::new(build(prefix));
        run(config, &mut rerun, prefix);
        let through = |sent: &[(usize, Intent)]| -> Vec<(usize, Intent)> {
            sent.iter().copied().filter(|(t, _)| *t <= point).collect()
        };
        let (a, b) = (through(&full), through(&rerun.sent));
        if a == b {
            continue;
        }
        let tick = a.iter().zip(&b).find(|(x, y)| x != y).map_or_else(
            || {
                a.get(b.len())
                    .or_else(|| b.get(a.len()))
                    .map_or(point, |(t, _)| *t)
            },
            |((ta, _), (tb, _))| (*ta).min(*tb),
        );
        divergences.push(Divergence {
            checked_at: point,
            tick,
            at: ticks[tick].stamp.exch,
            full: intents_on(&a, tick),
            truncated: intents_on(&b, tick),
        });
    }

    LookaheadReport {
        signals: candidates.len(),
        checked: points.len(),
        divergences,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tick_at;
    use oq_margin::{Contract, TierTable};
    use oq_types::{Cash, InstrumentId, PriceTicks, QtyLots, Side};

    fn config() -> RunConfig {
        RunConfig::new(
            InstrumentId::new(1),
            Contract::new(1_000),
            TierTable::example_btcusdt(),
            Cash::from_units(20_000),
        )
    }

    /// A zigzag with a drift, so there is something to buy and sell.
    fn ticks(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let wobble = [0, 300, -200, 500, -400, 100, -100][i % 7];
                let p = 6_000_000 + i as i64 * 50 + wobble;
                tick_at(i as i64 * 1_000_000_000, p, p, p)
            })
            .collect()
    }

    /// Buys after a fall, using only what it has seen.
    struct Honest {
        prev: Option<i64>,
        next: u64,
    }

    impl Strategy for Honest {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            let last = ctx.tick.last.0;
            if self.prev.is_some_and(|p| last < p) {
                self.next += 1;
                out.push(ctx.limit(
                    OrderId::new(self.next),
                    Side::Buy,
                    PriceTicks(last),
                    QtyLots(1),
                ));
            }
            self.prev = Some(last);
        }

        fn name(&self) -> &str {
            "honest"
        }
    }

    /// Buys before a rise, which it knows about because it was built with
    /// the whole window.
    struct Clairvoyant {
        future: Vec<i64>,
        seen: usize,
        next: u64,
    }

    impl Strategy for Clairvoyant {
        fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
            let i = self.seen;
            self.seen += 1;
            if self
                .future
                .get(i + 1)
                .is_some_and(|next| *next > ctx.tick.last.0)
            {
                self.next += 1;
                out.push(ctx.limit(
                    OrderId::new(self.next),
                    Side::Buy,
                    ctx.tick.last,
                    QtyLots(1),
                ));
            }
        }

        fn name(&self) -> &str {
            "clairvoyant"
        }
    }

    #[test]
    fn a_strategy_that_uses_only_the_past_is_clean() {
        let data = ticks(120);
        let report = lookahead(
            &config(),
            |_| Honest {
                prev: None,
                next: 0,
            },
            &data,
            DEFAULT_POINTS,
        );
        assert!(report.signals > 10, "{report:?}");
        assert_eq!(report.checked, report.signals);
        assert!(!report.sampled());
        assert!(report.clean(), "{report:?}");
    }

    #[test]
    fn a_strategy_built_from_the_whole_window_is_caught() {
        let data = ticks(120);
        let report = lookahead(
            &config(),
            |known: &[Tick]| Clairvoyant {
                future: known.iter().map(|t| t.last.0).collect(),
                seen: 0,
                next: 0,
            },
            &data,
            DEFAULT_POINTS,
        );
        assert!(!report.clean());
        let first = &report.divergences[0];
        // On the last tick of a prefix the rerun cannot see the next
        // price, so it does not buy where the full run did.
        assert_eq!(first.tick, first.checked_at);
        assert!(
            !first.full.is_empty() && first.truncated.is_empty(),
            "{first:?}"
        );
    }

    #[test]
    fn at_most_the_bound_is_checked_and_the_report_says_so() {
        let data = ticks(600);
        let report = lookahead(
            &config(),
            |_| Honest {
                prev: None,
                next: 0,
            },
            &data,
            20,
        );
        assert!(report.signals > 20);
        assert!(report.checked <= 20 && report.checked > 0);
        assert!(report.sampled());
        assert!(report.clean());
    }

    #[test]
    fn the_spread_is_even_and_keeps_both_ends() {
        let c: Vec<usize> = (0..100).collect();
        let s = spread(&c, 5);
        assert_eq!(s, vec![0, 24, 49, 74, 99]);
        assert_eq!(spread(&c[..3], 5), vec![0, 1, 2]);
        assert!(spread(&c, 0).is_empty());
    }
}
