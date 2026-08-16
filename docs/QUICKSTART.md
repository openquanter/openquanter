# Quickstart

> [中文版](QUICKSTART.zh-CN.md) · Target: a running backtest in under 30 minutes from a clean machine.

## 1. Build

Rust 2024 edition, minimum version 1.85. No services to run and no data
to download — the examples generate their own market from a seed.

Cargo will fetch a dependency tree for two crates and no others:
`oq-l2feed` carries a WebSocket and TLS stack because it has to speak to
a venue, and `oq-examples` carries `criterion` as a dev-dependency for
its benchmarks. The engine itself — types, journal, core, matching,
margin, backtest, data, parity, statistics — is plain std Rust, which
`scripts/check-composability.sh` enforces in CI. If you only want the
engine, `cargo build -p oq-core` pulls nothing.

```bash
git clone https://github.com/openquanter/openquanter
cd openquanter
cargo test --workspace
```

If the tests pass, everything below will work.

## 2. Run the first example

```bash
cargo run --example hello
```

```text
strategy      buy-and-hold
observations  2000
fills         1
final equity      10861.94 USDT
lowest equity      9993.38 USDT
liquidations  0
```

Twenty lines of strategy: buy once, hold. It exists to show the loop —
a strategy receives observations, returns intents, and the host fills
them and keeps the books. Read
[`crates/oq-examples/examples/hello.rs`](../crates/oq-examples/examples/hello.rs)
before anything else; it is the whole API surface in one screen.

## 3. Run the example that explains the project

```bash
cargo run --example martingale_ladder
```

```text
                        enforced      margin-free
final equity             61.53     20908.11
lowest equity            61.53    -30302.14
fills                        4                6
liquidations                 1                0

martingale-ladder: LIQUIDATED 1x, margin-free equity 20908.11 vs real 61.53
(overstated by 20846.58); 2 fills in the margin-free run happened after the
account was already closed
```

The same strategy, the same market, run twice: once with liquidation
enforced and once without. A margin-free backtest — which is what most
open backtesters give you — reports **20 908 USDT** on an account that
in reality ended with **61.53**.

The tell is the lowest equity: **−30 302**. Equity below zero is not a
drawdown, it is an account that stopped existing. Every fill after that
point was placed by an account the venue had already closed, and the
report counts them.

This is what the margin overlay is for, and it is why the fidelity
ladder treats account realism as an axis of its own.

## 4. Run the API tour

```bash
cargo run --example ma_cross
```

A moving-average crossover: indicators, position flipping, `on_fill`.
Its parameters are round numbers chosen before the run and never
adjusted, because a tuned example is a lesson in overfitting dressed as
a tutorial. If you change the windows and the result improves, you have
just performed the search that `oq-stats` exists to penalise.

### One thing the numbers above leave out

**The examples charge no trading fees.** Every figure on this page is
gross of costs, because none of them sets a fee schedule. Fees are
modelled — maker and taker rates, and a maker rate may be negative
because rebates exist — but they default to zero and have to be asked
for:

```rust
use oq_backtest::Fees;
use oq_types::Ratio;

let config = config.with_fees(Fees::flat(Ratio::from_ppm(500))); // 0.05%
```

Zero is the default so that a run with no fee schedule is obviously a
run with no fee schedule, rather than one quietly using a plausible
number nobody chose. On a strategy that trades often the difference is
not decorative, and a page in this project should say so rather than
let you discover it.

## 5. Write your own

A strategy is one trait with one required method:

```rust
use oq_backtest::{Context, Intent, Strategy};
use oq_types::{OrderId, QtyLots, Side};

struct Mine;

impl Strategy for Mine {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if ctx.position.0 == 0 {
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(1),
            });
        }
    }

    fn name(&self) -> &str { "mine" }
}
```

The strategy has no clock, no I/O, and no handle to the engine. That is
deliberate: it cannot introduce non-determinism, and it cannot reach
around the risk layer, because it has nothing to reach with.

## 6. Where the data comes from

The examples run on a **generated** market, seeded so that every machine
produces the identical series — which is why the numbers above are
quotable and why golden tests can pin them.

For real data, `oq-l2feed` captures venue streams verbatim:

```bash
cargo run --bin oq-capture -- \
  --root ./archive --symbol BTCUSDT --stream depth --minutes 10 --floor-gb 10
```

Then check that what you captured is actually usable:

```bash
cargo run --bin oq-book-check -- --file ./archive/<venue>/BTCUSDT/depth/<date>.oqcap
```

It replays every depth update into a book and reports what it found —
updates applied, gap markers the capture declared, sequence breaks
nobody declared, and whether the book ever crossed — ending in a
verdict line, `RECONSTRUCTS CLEANLY` or the specific reason it does not.
Counts depend on your capture, so nothing is quoted here.

Run this on day one, not in six months. Files on disk prove the messages
arrived; only replaying them into an order book proves they can be
*used*. A capture defect — a mishandled reconnect, a misread sequence
field, a stream that turned out to be coalesced — looks perfectly
healthy on disk, and by the time it surfaces the window it corrupted
cannot be recaptured. The check exits non-zero when the archive is not
what it claims, so it belongs in whatever runs after a capture.

See [Capture Format](CAPTURE-FORMAT.md) for the archive layout, the
sealing and verification pipeline, and what the venue actually serves —
including the streams that accept a subscription and then send nothing.

## What to read next

| If you want to | Read |
|---|---|
| Know what exists and what does not | [README Status](../README.md#status) |
| Know what the framework promises | [Requirements](REQUIREMENTS.md) |
| Know what each milestone unlocks | [Roadmap](ROADMAP.md) |
| Understand the architecture | [Implementation Plan](IMPLEMENTATION.md) |
| Know what `2.0.0-alpha` promises | [Versioning](VERSIONING.md) |
| Read or write archive and tick files | [Capture Format](CAPTURE-FORMAT.md) · [Tick Format](TICK-FORMAT.md) |
| Contribute | [CONTRIBUTING.md](../CONTRIBUTING.md) |

## A word about the examples

Every example is expected to lose money, and none is a strategy to run.
They demonstrate properties of the framework. A project whose central
claim is that backtests flatter you would be a poor place to show off a
pretty equity curve.
