//! One layer at a time: the journal, each matching tier, the kernel.
//!
//! ```text
//! cargo bench -p oq-examples --bench engine
//! ```
//!
//! The end-to-end benchmark beside this one says how fast a backtest is;
//! it cannot say which layer moved when that number does. These measure
//! the hot paths one by one, on the same seeded market, so a change to
//! one layer can be judged against its own baseline (`scripts/bench-compare.sh`)
//! rather than against a total that also contains everything else.
//!
//! "Deep" means a hundred resting orders that the market never reaches:
//! the book the engine walks on every tick without filling anything,
//! which is the cost a quoting strategy pays continuously.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use oq_core::{Event, Kernel, State};
use oq_engine::{Delay, Impact, L0Engine, L1Engine, Latency, Policy, QueueAhead, Tick};
use oq_examples::{MarketShape, series};
use oq_margin::{Contract, TierTable};
use oq_types::{
    Cash, InstrumentId, Nanos, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side, Stamp,
};

const TICKS: usize = 20_000;
const DEPTH: u64 = 100;

fn ticks() -> Vec<Tick> {
    series(MarketShape::calm(TICKS), 7)
}

/// Bids far enough under the market that none of them fills.
fn deep_bids(ticks: &[Tick]) -> impl Iterator<Item = (OrderId, PriceTicks)> {
    let low = ticks.iter().map(|t| t.last.0).min().unwrap_or(0);
    (1..=DEPTH).map(move |i| (OrderId(i), PriceTicks(low / 2 - i as i64)))
}

fn l1_policy() -> Policy {
    // Non-zero impact, as `tiers` uses: with a zero coefficient the
    // impact path is skipped and its cost would not be measured.
    Policy {
        queue: QueueAhead::Fixed(QtyLots(50)),
        latency: Latency {
            entry: Delay::Fixed(Nanos(5_000_000)),
            response: Delay::Fixed(Nanos(5_000_000)),
        },
        impact: Impact { coefficient: 50 },
    }
}

fn l0(c: &mut Criterion) {
    let ticks = ticks();
    let mut group = c.benchmark_group("l0_on_tick");
    group.throughput(Throughput::Elements(TICKS as u64));
    for (name, deep) in [("empty", false), ("deep", true)] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut e = L0Engine::new(InstrumentId::new(1));
                    if deep {
                        for (id, price) in deep_bids(&ticks) {
                            e.submit_limit(id, Side::Buy, price, QtyLots(1), Stamp::new(0, 0));
                        }
                    }
                    e
                },
                |mut e| {
                    for t in &ticks {
                        black_box(e.on_tick(t));
                    }
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn l1(c: &mut Criterion) {
    let ticks = ticks();
    let mut group = c.benchmark_group("l1_on_tick");
    group.throughput(Throughput::Elements(TICKS as u64));
    for (name, deep) in [("empty", false), ("deep", true)] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut e = L1Engine::new(InstrumentId::new(1), l1_policy());
                    if deep {
                        for (id, price) in deep_bids(&ticks) {
                            e.submit(
                                oq_engine::limit_order(
                                    id,
                                    Side::Buy,
                                    price,
                                    QtyLots(1),
                                    Stamp::new(0, 0),
                                    Offset::Open,
                                ),
                                Nanos(0),
                            );
                        }
                    }
                    e
                },
                |mut e| {
                    for t in &ticks {
                        black_box(e.on_tick(t));
                    }
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// The kernel holding a position, so every tick marks it and checks
/// margin — the path a leveraged backtest runs on every observation.
fn kernel(c: &mut Criterion) {
    let ticks = ticks();
    let mut group = c.benchmark_group("kernel_apply_tick");
    group.throughput(Throughput::Elements(TICKS as u64));
    group.bench_function("margin_holding", |b| {
        b.iter_batched(
            || {
                let state = State::new(
                    InstrumentId::new(1),
                    Contract::new(10_000),
                    TierTable::example_btcusdt(),
                    Cash::from_units(1_000_000),
                );
                let mut k = Kernel::new(state);
                k.apply(&Event::Submit {
                    instrument: None,
                    id: OrderId(1),
                    side: Side::Buy,
                    price: None,
                    qty: QtyLots(10),
                    offset: Offset::Open,
                    stamp: Stamp::new(0, 0),
                });
                k.apply(&Event::Tick {
                    instrument: None,
                    tick: ticks[0],
                });
                // Measuring a flat account would skip the margin path
                // this is meant to price.
                assert_ne!(k.summary().qty, QtyLots(0), "the position opened");
                k
            },
            |mut k| {
                for t in &ticks[1..] {
                    black_box(k.apply(&Event::Tick {
                        instrument: None,
                        tick: *t,
                    }));
                }
            },
            criterion::BatchSize::LargeInput,
        );
    });
    group.finish();
}

/// Encoding an event and appending it, without the fsync: what the
/// journal adds per event before the device is involved.
fn journal(c: &mut Criterion) {
    let ticks = ticks();
    let dir = std::env::temp_dir().join(format!("oq-bench-journal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut group = c.benchmark_group("journal_append");
    group.throughput(Throughput::Elements(TICKS as u64));
    let mut run = 0u64;
    group.bench_function("tick_event_no_sync", |b| {
        b.iter_batched(
            || {
                run += 1;
                oq_journal::Writer::open(
                    dir.join(format!("{run}.oqj")),
                    oq_journal::SyncPolicy::Never,
                )
                .expect("writer")
            },
            // Returned, so closing the writer falls outside the timing;
            // the files go with the directory at the end.
            |mut w| {
                for t in &ticks {
                    let event = Event::Tick {
                        instrument: None,
                        tick: *t,
                    };
                    black_box(w.append(event.kind(), &event.encode()).expect("append"));
                }
                w
            },
            criterion::BatchSize::PerIteration,
        );
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fixed-point arithmetic the margin path leans on.
fn arithmetic(c: &mut Criterion) {
    let ticks = ticks();
    let contract = Contract::new(10_000);
    let mut group = c.benchmark_group("fixed_point");
    group.throughput(Throughput::Elements(TICKS as u64));
    group.bench_function("notional_and_scaled", |b| {
        b.iter(|| {
            let mut sum = 0i64;
            for t in &ticks {
                let n = contract.notional(black_box(t.last), QtyLots(37));
                sum = sum.wrapping_add(n.scaled(Ratio::from_percent(3)).0);
            }
            black_box(sum)
        });
    });
    group.finish();
}

criterion_group!(all, l0, l1, kernel, journal, arithmetic);
criterion_main!(all);
