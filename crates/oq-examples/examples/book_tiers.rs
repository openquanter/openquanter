//! The same strategy and the same prices, matched with and without the
//! venue's book.
//!
//! ```text
//! cargo run --release -p oq-examples --example book_tiers
//! ```
//!
//! `tiers` compares L0 against L1, where the queue is a **policy** — a
//! number the caller supplies about their own market. This compares L0
//! against L2, where the queue is **read**: the size displayed at the
//! level an order joins is the queue ahead of it.
//!
//! The two runs below are identical in every respect except that one is
//! handed depth. Same strategy, same ticks, same seed, same policy. Any
//! difference in the fills is the book and nothing else.
//!
//! # What to take from it
//!
//! Not the P&L. The fill count. A maker strategy's backtest rests on
//! fills the queue would never have reached, and the number of those is
//! the size of the lie — not a percentage to shave off the return.
//!
//! # The depth here is generated, and that is a real limit
//!
//! The book is synthesised from each tick so the example runs with no
//! data to download. It is *consistent* with the prices, not measured
//! alongside them: a real book thickens and thins in ways a formula
//! does not reproduce, and the queue is the thing that matters most.
//! For the real thing, convert an archive and feed those updates —
//! `oq-book-check` proves an archive reconstructs, and this is the
//! shape a caller who has one uses.

use oq_backtest::{
    Context, Intent, Observation, RunConfig, RunResult, Strategy, Tier, run_observations,
};
use oq_engine::{Delay, Impact, Latency, Level, Policy, QueueAhead, Tick};
use oq_examples::{MarketShape, money, series};
use oq_margin::{Contract, MarginTier, TierTable};

use oq_types::{Cash, InstrumentId, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side};

/// Rests a buy one tick under the market, re-pricing as it moves.
///
/// A maker on purpose: a taker would show almost no queue effect and
/// the comparison would look like a rounding difference.
struct RestingBid {
    next_id: u64,
    working: Option<(OrderId, i64)>,
}

impl Strategy for RestingBid {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let want = ctx.tick.last.0 - 1;
        match self.working {
            Some((id, at)) if at != want => {
                out.push(Intent::Cancel(id));
                self.working = None;
            }
            Some(_) => {}
            None => {
                let id = OrderId(self.next_id);
                self.next_id += 1;
                out.push(Intent::Limit {
                    instrument: ctx.instrument,
                    id,
                    side: Side::Buy,
                    price: PriceTicks(want),
                    qty: QtyLots(1),
                    offset: Offset::Open,
                });
                self.working = Some((id, want));
            }
        }
    }

    fn on_fill(&mut self, _f: &oq_types::Fill, _c: &Context, _o: &mut Vec<Intent>) {
        self.working = None;
    }

    fn name(&self) -> &str {
        "resting-bid"
    }
}

/// A book around each observation, thick enough that the queue matters.
///
/// Every level carries `depth` lots, so an order joining the touch has
/// that much ahead of it and waits for the volume to arrive. The
/// sequence ids are consecutive because they have to be: the book
/// refuses an update it cannot place, which is what makes a
/// reconstruction trustworthy and what a generator has to respect.
fn book_for(ticks: &[Tick], depth: i64) -> Vec<Observation> {
    let mut out = Vec::with_capacity(ticks.len() * 2 + 1);
    out.push(Observation::Snapshot {
        update_id: 0,
        bids: Vec::new(),
        asks: Vec::new(),
    });
    for (i, t) in ticks.iter().enumerate() {
        let id = i as u64 + 1;
        out.push(Observation::Depth(Box::new(oq_engine::DepthUpdate {
            event_ms: t.stamp.exch.0 / 1_000_000,
            first_id: id,
            final_id: id,
            prev_final_id: if id > 1 { Some(id - 1) } else { None },
            // Two levels a side, the near one at the price this
            // strategy quotes into.
            bids: vec![
                Level {
                    price: t.last.0 - 1,
                    qty: depth,
                },
                Level {
                    price: t.last.0 - 2,
                    qty: depth,
                },
            ],
            asks: vec![
                Level {
                    price: t.last.0 + 1,
                    qty: depth,
                },
                Level {
                    price: t.last.0 + 2,
                    qty: depth,
                },
            ],
        })));
        out.push(Observation::Tick(*t));
    }
    out
}

fn strategy() -> RestingBid {
    RestingBid {
        next_id: 1,
        working: None,
    }
}

fn config(tier: Tier) -> RunConfig {
    let table = TierTable::new(vec![MarginTier {
        max_notional: Cash(i64::MAX),
        rate: Ratio::from_percent(1),
        amount: Cash::ZERO,
    }])
    .expect("single bracket");
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        table,
        Cash::from_units(1_000_000),
    )
    .at_tier(tier)
}

fn row(label: &str, r: &RunResult, baseline: usize) {
    let share = if baseline == 0 {
        0.0
    } else {
        100.0 * r.fills.len() as f64 / baseline as f64
    };
    println!(
        "{label:<20} {:>8} {:>7.1}% {:>14} {:>6}",
        r.fills.len(),
        share,
        money(r.final_equity),
        r.tier,
    );
}

fn main() {
    let ticks = series(MarketShape::calm(50_000), 7);
    // The one policy both runs share, so the tier is the only variable.
    // Latency and impact are off: they would move both runs the same way
    // and only obscure what the book did.
    let policy = Policy {
        queue: QueueAhead::None,
        latency: Latency {
            entry: Delay::Fixed(oq_types::Nanos(0)),
            response: Delay::Fixed(oq_types::Nanos(0)),
        },
        impact: Impact { coefficient: 0 },
    };

    let ticks_only: Vec<Observation> = ticks.iter().copied().map(Observation::Tick).collect();

    let l0 = run_observations(&config(Tier::L0), &mut strategy(), ticks_only.clone());
    let unfed = run_observations(&config(Tier::L2(policy)), &mut strategy(), ticks_only);
    let base = l0.fills.len();

    println!("ticks {}", ticks.len());
    println!();
    println!(
        "{:<20} {:>8} {:>8} {:>14} {:>6}",
        "", "fills", "of L0", "final equity", "tier"
    );
    row("L0", &l0, base);
    row("L2, no book", &unfed, base);

    assert_eq!(
        unfed.fills.len(),
        base,
        "an L2 given no depth must answer as L0"
    );

    // The same run at a range of book thicknesses. One number would
    // invite reading it as *the* effect; the range is the finding, and
    // it is steep.
    let mut counts = Vec::new();
    for depth in [1, 4, 16, 64, 256] {
        let fed = run_observations(
            &config(Tier::L2(policy)),
            &mut strategy(),
            book_for(&ticks, depth),
        );
        assert_eq!(fed.depth_refused, 0, "the generated book must sequence");
        assert_eq!(fed.depth_unused, 0, "and L2 must read it");
        row(&format!("L2, {depth} ahead"), &fed, base);
        counts.push((depth, fed.fills.len()));
    }

    // The same number, named instead of read. L1 is told to assume 64
    // lots ahead of every order; L2 above was shown a book displaying
    // 64. If the ladder is coherent these two agree -- and where they
    // do, the difference between the tiers is not the model, it is who
    // supplied the number and whether they could have known it.
    let named = run_observations(
        &config(Tier::L1(Policy {
            queue: QueueAhead::Fixed(QtyLots(64)),
            ..policy
        })),
        &mut strategy(),
        ticks
            .iter()
            .copied()
            .map(Observation::Tick)
            .collect::<Vec<_>>(),
    );
    row("L1, 64 assumed", &named, base);

    // More ahead of you cannot mean more fills. Asserted rather than
    // eyeballed: the queue only ever delays, so a rise here would be
    // the measurement disagreeing with what it claims to measure.
    for w in counts.windows(2) {
        assert!(
            w[1].1 <= w[0].1,
            "a deeper queue filled more: {} at {} vs {} at {}",
            w[1].1,
            w[1].0,
            w[0].1,
            w[0].0
        );
    }

    println!();
    println!(
        "An L2 that was never handed depth produced L0's {base} fills exactly. \
         Choosing a tier changes nothing on its own; being handed the data it \
         reads is what changes the answer."
    );
    println!();
    let (_, at_64) = counts[3];
    // Pinned, because it is the ladder's coherence and not a
    // coincidence: an assumed queue and a measured one of the same size
    // must produce the same fills, or the tiers are two models rather
    // than one model with two sources for its input.
    assert_eq!(
        named.fills.len(),
        at_64,
        "a named queue and a measured one of the same size must agree"
    );
    println!(
        "L1 told to assume 64 and L2 shown a book of 64 produced {} and {} \
         fills. They agree because the assumption happened to be right, which \
         is the only circumstance in which a policy is right -- and nothing in \
         an L1 run can tell you whether you are in it.",
        named.fills.len(),
        at_64
    );

    println!();
    let (_, at_16) = counts[2];
    println!(
        "A queue of 1 or 4 changes nothing: this market trades 10 to 100 lots \
         an observation, so a queue that small clears before the price moves. \
         The tier is only worth its name where the queue outlasts the \
         observation -- and between 16 ahead ({at_16} fills) and 64 ({at_64}) \
         most of the strategy stops existing."
    );
    println!();
    println!(
        "What disappears is not margin on each trade. It is the trades. A \
         backtest resting on fills the queue never reached is not a strategy \
         earning less than modelled; it is a different strategy, and the \
         equity beside it is about a market that was not there."
    );
    println!();
    println!(
        "That is the argument for measuring the queue rather than naming it. \
         A caller asked for a `QueueAhead` is choosing one of these rows, and \
         the run reports every one of them as fidelity."
    );
}
