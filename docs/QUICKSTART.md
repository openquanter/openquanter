# Quickstart

> [中文版](QUICKSTART.zh-CN.md) · Target: a running backtest in under 30 minutes from a clean machine.

## 1. Build

Rust 2024 edition, minimum version 1.85. No services to run and no data
to download — the examples generate their own market from a seed.

Cargo fetches a dependency tree only for the crates that talk to a
venue — `oq-l2feed` and `oq-ingest` for capture, `oq-gateway` for reading
an account — plus `criterion` as a dev-dependency of `oq-examples` for
its benchmarks. The engine itself — types, journal, core, matching,
margin, backtest, data, parity, statistics — is plain std Rust, which
`scripts/check-composability.sh` enforces in CI. If you only want the
engine, `cargo build -p oq-core` pulls nothing.

**`cargo install` does not work yet, and this is the only place that
says so.** The names on crates.io are `0.0.1` placeholders reserving
them — `oq-cli` is 1306 bytes against 16K of source — and their own
published descriptions say the implementation lives in this repository.
Installing one gets an empty crate and no error, which is worse than a
missing package.

So build from the checkout. Everything below assumes it:

```bash
git clone https://github.com/openquanter/openquanter
cd openquanter
cargo test
```

If the tests pass, everything below will work. Measured on a clean
clone with an empty target directory: 3 s to clone, 34 s for the tests,
1 s for the first example — 38 s to a backtest, assuming Rust is
already installed.

`cargo test` and not `cargo test --workspace`. The workspace variant
also builds `oq-py`, whose tests link against a CPython shared library
and fail on a machine whose Python does not match — which says nothing
about this repository and everything about that machine. The Python
bindings have their own CI job with a pinned interpreter. This page
said `--workspace` until somebody ran it on a clean clone and got exit
101 one line above the sentence promising everything below would work.

The command-line tools ship inside the library crates rather than as
separate packages, so there is nothing named `oq-capture` to look for
either. Run them with `cargo run`:

```bash
cargo run --bin oq          # one name that finds the rest
cargo run --bin oq-capture  # also oq-book-check, oq-trade-check, oq-merge, oq-resequence
cargo run --bin oq-ingest
cargo run --bin oq-recon    # also oq-order-check
cargo run --bin oq-trade    # also oq-belief, oq-replay
```

`oq` on its own lists every tool with what it is for, and `oq <tool>`
runs it with the arguments passed through unchanged. It is worth
running first: it is the only one that tells you the others exist.

When the crates are published for real, `cargo install oq-cli` becomes
the shorter path and this paragraph goes away.


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

martingale-ladder: LIQUIDATED 1x, first at t=114750000000; margin-free equity
20908.11 vs real 61.53
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

## 7. Backtest on what you captured

An archive is not yet something the engine reads. `oq-ingest` folds
captured depth and trades into the tick format a backtest replays:

```bash
cargo run --bin oq-ingest -- \
  --archive ./archive/binance-perp/BTCUSDT --day 2026-08-16 --out btc.ticks
```

It reports what it built — windows emitted, how many carried a trade,
gap markers seen, payloads it could not read — so a thin conversion is
visible rather than quietly small. `--window-ms` sets the window; one
second is the default.

The conversion is lossy on purpose. A window of L2 depth becomes a best
bid and a best ask, and the book behind them is dropped. That is only an
acceptable trade because the archive is kept: the capture is the record,
and this is a projection of it for the strategies a projection can
carry. Strategies that need the book itself need the L2 fidelity tier,
which does not exist yet — a richer tick would not substitute for it.

Two conventions matter if you read the output directly. Extremes belong
to their own window: `high` and `low` are the highest and lowest trades
*inside* a window, never a running maximum carried forward. Volume
accumulates, so per-window volume is the difference between consecutive
ticks; a difference that comes out negative means the venue reset its
counter rather than that trades were undone.

Quoting precision comes from the venue's instrument table rather than a
default, because a wrong scale does not fail — it silently rescales
every price. If the instrument is unknown, the tool stops instead of
guessing.

## 8. Run one against a venue

Everything above runs on recorded data. This is the same loop with the
venue supplying the events instead of a file — the one step that cannot
be checked by reading, because the difference between a backtest and
live trading is entirely in what happens to an order after it leaves.

You need testnet credentials from the venue. **Testnet.** Nothing here
asks you to risk anything, and the example refuses `--live` outright.

```bash
export OQ_VENUE_KEY=…
export OQ_VENUE_SECRET=…

cargo run --release -p oq-live --example grid_live -- \
  --symbol BTCUSDT --minutes 30
```

That is a real strategy — the grid from §"Strategies you already know"
— against a real order book, with the risk gate in front of it, the
kernel keeping the account, the journal recording it, and the shadow
backtest running alongside so the gap between them is measured while it
happens rather than argued about afterwards.

It will refuse to start beside a position it was not told about. That
is not caution for its own sake: a process that adopts an unexplained
position has no way to tell a leftover from someone else's, and the
first thing it would do is manage it. `--adopt-existing` says you
checked.

### Why the grid

It is short volatility with no stop, which is the failure shape a
margin model exists to make visible: every rung is profitable until the
range breaks, and then the position is on the wrong side of a trend
with more size than any single decision ever approved. A long run of it
exercises the part of this framework that is hardest to exercise any
other way.

**It is not a recommendation.** It is the strategy most worth watching
fail.

### What to watch

The run prints a banner with the limits it will enforce and their
version, then a line per order. Three things are worth reading rather
than skimming:

- **`placed` and `refused` are different numbers.** The summary line
  reports both, and their ratio is the thing a backtest cannot show
  you: a simulated matcher answers every submission, so *asked* and
  *accepted* coincide there and only there.
- **An `UNRESOLVED` line means the account may not be where the rest of
  the summary says it is.** It is printed separately, and only when it
  happened, because it is the one outcome that is neither a yes nor a
  no. Nothing replaces such an order — resending is the single move
  that turns *maybe one order* into *certainly two*.
- **Refusals are normal and are supposed to be loud.** A risk limit
  firing is the gate working. The grid's own cap and the limits it runs
  under are deliberately small so this happens inside half an hour
  rather than never.
- **The end-of-run metrics and alerts.** Alerts here are *judgements*,
  not notifications — nothing sends anything. Wiring them to something
  that does is deployment, and deliberately not this framework's job.

### If you want it to send orders through `oq-trade` instead

`oq-trade` has `observe` (sends nothing) and `probe` (a connectivity
diagnostic, not a strategy). Neither is a strategy runner, on purpose:
the strategies in this repository live in `oq-examples`, which is not
published, and a published `oq-live` cannot depend on an unpublished
crate. Writing your own is §5, and `grid_live.rs` is about thirty lines
you can copy.

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

### And a word about the markets they run on

`series` and `crash_series` are **fixtures, not simulations**, and
`oq_stats::StylizedFacts` measures the difference rather than leaving it
to be assumed. Against the properties that hold in essentially every
liquid market:

| | calm | trending | crash |
|---|---|---|---|
| uncorrelated returns | holds | holds | **absent, ρ(1) = 0.54** |
| heavy tails | absent | absent | absent |
| volatility clustering | absent | absent | holds |
| aggregational gaussianity | holds | absent | holds |

Two things follow, and both change how the numbers above should be read.

**None of them has heavy tails.** Excess kurtosis of 0.03, 0.07 and
−0.05, against a real perpetual's tens. Liquidation is a tail event, so
a fixture without a tail produces one only where it was told to — and
the margin-free-versus-enforced gap these examples report is therefore
a **floor** on the real one rather than an estimate of it.

**The crash fixture's returns are strongly autocorrelated.** A sustained
monotone move means consecutive returns share a sign for hundreds of
observations, which a one-lag rule would predict and no real market
offers. That predictability is exactly what makes it a usable fixture —
the liquidation happens where it was put — and exactly why a strategy
result from it is a statement about this series and not about trading.

The measurements are pinned in `crates/oq-examples/tests/stylized.rs`,
so changing a generator is noticed by whoever changes it.

## Strategies you already know

```bash
cargo run --release -p oq-examples --example classics
```

Six classics — RSI reversion, MACD, Bollinger bands, Donchian breakout,
grid, Dual Thrust — with their published parameters, untuned.

**None of them is a recommendation.** Every one is decades old and traded
by enough people that whatever edge it had is not waiting in a public
repository. They are here so the framework can be learned by recognising
something, rather than by learning two things at once.

The example does not print an equity curve and stop. It runs each one
with liquidation modelled and without, and prints both — because a curve
that never gets liquidated is a curve about an account no venue offers.
Unlevered the two columns agree for all six, which is itself the finding:
**a margin model is invisible until leverage is real.** Levered, the grid
ends at 4.46 with the venue having closed the account twice, and the
margin-free arm reports −508.12 for a position it kept holding.
