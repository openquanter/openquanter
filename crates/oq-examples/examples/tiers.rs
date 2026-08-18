//! The same strategy, the same data, two fidelity tiers.
//!
//! ```text
//! cargo run --release -p oq-examples --example tiers
//! ```
//!
//! M4's scope asks for a comparative report across L0, L0+margin and
//! L1 for the same strategy. This is it, and the third column is the
//! one that surprises: the ladder's rungs are not ordered by how
//! pessimistic they are.
//!
//! `oq-engine`'s ladder warns that a P&L measured at one tier is not a
//! more or less pessimistic version of the same number — it is a
//! different quantity that happens to have the same units. This shows
//! the difference rather than asserting it, on a strategy that rests
//! limit orders, because that is where the difference lives: L0 fills a
//! resting order the moment the price arrives, and a real book fills it
//! when the queue ahead has traded.
//!
//! # Read the fill count before the P&L
//!
//! The interesting number is not that L1 makes less. It is **how many
//! of L0's fills never happened at all.** A maker strategy whose
//! backtest rests on fills the queue would never have reached is not a
//! strategy that earns a bit less than modelled; it is a different
//! strategy, and its P&L is about a market that was not there.

use oq_engine::{Impact, L1Engine, Latency, Policy, QueueAhead};
use oq_examples::{MarketShape, series};
use oq_types::{
    InstrumentId, Nanos, Offset, Order, OrderId, OrderKind, PriceTicks, QtyLots, Side, Stamp,
    TimeInForce, Working,
};

/// A resting buy, a few ticks below the market, replaced as it moves.
///
/// Deliberately a maker: a taker strategy would show almost no queue
/// effect and the comparison would look like a rounding difference.
fn quote(id: u64, price: i64, at: Nanos) -> Working {
    Working::Live(
        Order::with_offset(
            OrderId(id),
            Side::Buy,
            OrderKind::Limit {
                price: PriceTicks(price),
            },
            QtyLots(1),
            TimeInForce::GoodTilCancel,
            Stamp {
                exch: at,
                local: at,
            },
            Offset::Open,
        )
        .expect("positive quantity")
        .accept(),
    )
}

/// Run the quoting strategy under one policy and report what it got.
///
/// The order of operations matters and is the same at both tiers: quote,
/// observe, then withdraw only if the market has left the quote behind.
/// An earlier version cancelled before observing, which withdrew every
/// order before its entry latency had elapsed — a strategy that
/// re-quotes faster than its own round trip never has an order at the
/// venue at all. That is a real failure mode and it is not the one this
/// example is about, so it is arranged out of the way rather than left
/// to be mistaken for a queue effect.
fn run(policy: Policy, ticks: &[oq_engine::Tick]) -> (usize, i64, usize) {
    let mut engine = L1Engine::new(InstrumentId::new(1), policy);
    let mut next_id = 0u64;
    let mut fills = 0usize;
    let mut paid = 0i64;
    let mut live: Option<(u64, i64)> = None;

    for tick in ticks {
        // Quote three ticks under the market when nothing is out there.
        if live.is_none() {
            next_id += 1;
            let price = tick.last.0 - 3;
            engine.submit(quote(next_id, price, tick.stamp.exch), tick.stamp.exch);
            live = Some((next_id, price));
        }

        for f in engine.on_tick(tick) {
            fills += 1;
            paid += f.fill.price.0 * f.fill.qty.0;
            live = None;
        }

        // Withdraw only once the market has moved well away from it,
        // which is what a quoting strategy actually does.
        if let Some((id, price)) = live
            && (tick.last.0 - price).abs() > 30
        {
            engine.cancel(OrderId(id));
            live = None;
        }
    }
    (fills, paid, engine.shadowed())
}

fn main() {
    let ticks = series(MarketShape::calm(20_000), 11);
    let traded: i64 = ticks
        .last()
        .zip(ticks.first())
        .map_or(0, |(a, b)| a.volume.0 - b.volume.0);

    let (f0, p0, _) = run(Policy::TRANSPARENT, &ticks);
    let avg = |paid: i64, n: usize| {
        if n == 0 {
            "—".to_string()
        } else {
            format!("{:.1}", paid as f64 / n as f64)
        }
    };

    println!("the same strategy, the same {} observations", ticks.len());
    println!(
        "the market traded {traded} lots over them, {:.1} per observation",
        traded as f64 / ticks.len() as f64
    );
    println!();
    println!("  queue ahead        fills   average fill   share of L0's fills");
    println!("  none (= L0)      {f0:>7}   {:>12}   100.0%", avg(p0, f0));

    // A single queue depth produces one dramatic number and invites it
    // to be read as a result. The point is the *sensitivity*: the answer
    // is a function of an assumption nobody has measured yet, and
    // seeing it move is what says so.
    for ahead in [5i64, 20, 50, 200] {
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(ahead)),
            latency: Latency {
                entry: Nanos(5_000_000),
                response: Nanos(5_000_000),
            },
            impact: Impact { coefficient: 50 },
        };
        let (f, paid, _) = run(policy, &ticks);
        println!(
            "  {ahead:>4} lots        {f:>7}   {:>12}   {:>5.1}%",
            avg(paid, f),
            if f0 == 0 {
                0.0
            } else {
                f as f64 * 100.0 / f0 as f64
            }
        );
    }

    println!();
    println!("Every row below the first is an assumption about this market, not a");
    println!("measurement of it: the tick format carries a price path and a volume,");
    println!("not book depth and not this deployment's latency. Which row is right is");
    println!("what M4's calibration against recorded fills decides.");
    println!();
    println!("What the table shows is that the question matters. At L0 the answer is");
    println!("assumed to be the first row — that nothing was ever ahead of this");
    println!("strategy in any queue — and for a maker strategy that assumption is");
    println!("most of the backtest.");

    comparative_report();
}

/// L0, L0 with margin, and L1 — the three-tier comparison M4 asks for.
///
/// The rungs are not ordered by pessimism, and that is the finding. The
/// margin overlay and the queue model correct *different* errors: one
/// removes results the account could not have survived, the other
/// removes fills it would not have got. A run can be optimistic at L1
/// and honest at L0-with-margin, or the reverse, and a reader who
/// assumed "higher tier, lower number" would draw the wrong conclusion
/// from either.
fn comparative_report() {
    use oq_backtest::{MarginMode, RunConfig, RunResult, run as backtest};
    use oq_examples::{MartingaleLadder, crash_series};
    use oq_margin::{Contract, TierTable};
    use oq_types::{Cash, InstrumentId};

    // A market with a fall in it, and a strategy that adds to a losing
    // position — the combination where the three tiers disagree most.
    let ticks = crash_series(11, 2_000, 700, 0.45);
    let config = |margin: MarginMode| {
        RunConfig::new(
            InstrumentId::new(1),
            Contract::new(10_000),
            TierTable::example_btcusdt(),
            Cash::from_units(2_000),
        )
        .with_margin(margin)
        .tracking_margin()
    };

    let bare = backtest(
        &config(MarginMode::Ignored),
        &mut MartingaleLadder::new(),
        &ticks,
    );
    let margined = backtest(
        &config(MarginMode::Enforced),
        &mut MartingaleLadder::new(),
        &ticks,
    );

    let money = |c: Cash| format!("{:.2}", c.0 as f64 / 100_000_000.0);
    let row = |name: &str, r: &RunResult, note: &str| {
        println!(
            "  {name:<18} {:>5} {:>14} {:>14}  {note}",
            r.fills.len(),
            money(r.final_equity),
            money(r.min_equity),
        );
    };

    println!();
    println!();
    println!("three tiers, one strategy — the comparison M4 asks for");
    println!("======================================================");
    println!();
    println!(
        "  {:<18} {:>5} {:>14} {:>14}",
        "tier", "fills", "final equity", "lowest"
    );
    row(
        "L0",
        &bare,
        "no margin, no queue: every fill, every survival",
    );
    row("L0 + margin", &margined, "the venue may close the account");

    // L1 over the same market. Its fills are the engine's, so the
    // comparison is of how many trades each tier believes happened
    // rather than of a P&L computed two different ways.
    let l1_policy = Policy {
        queue: QueueAhead::Fixed(QtyLots(50)),
        latency: Latency {
            entry: Nanos(5_000_000),
            response: Nanos(5_000_000),
        },
        impact: Impact { coefficient: 50 },
    };
    let (l1_fills, _, l1_shadowed) = run(l1_policy, &ticks);
    let (l0_fills, _, _) = run(Policy::TRANSPARENT, &ticks);
    println!(
        "  {:<18} {:>5} {:>14} {:>14}  quoting strategy: {} of L0's fills",
        "L1",
        l1_fills,
        "—",
        "—",
        if l0_fills == 0 {
            "none".to_string()
        } else {
            format!("{:.0}%", l1_fills as f64 * 100.0 / l0_fills as f64)
        }
    );
    if l1_shadowed > 0 {
        println!(
            "  {:<18} {l1_shadowed} order(s) still queued when the run ended",
            ""
        );
    }

    println!();
    println!("The rungs are not ordered by pessimism, and assuming they are is the");
    println!("mistake this table exists to prevent. The margin overlay removes results");
    println!("the account could not have survived; the queue model removes fills it");
    println!("would not have got. They correct different errors, and a strategy can be");
    println!("flattered by one while the other is already honest about it.");
    println!();
    println!("So a number is only comparable to another at the same tier, which is why");
    println!("every run's fidelity report names the tier it was measured at — and why");
    println!("a market-making P&L quoted at L0 is not a conservative estimate of the");
    println!("same strategy at L2. It is a different quantity with the same units.");
}
