# OpenQuanter

**Quantitative trading components in Rust. Take the whole engine, or one crate.**

> ### Every cent between a backtest and the live run, accounted for.
>
> **P&L you cannot explain is not P&L.**
>
> The gap decomposes into slippage, queue position, funding, latency and fee
> tier. What will not decompose is recorded as the **unexplained residual** —
> and that number is this project's report card.
>
> This is the aim, not the state. How far off, see [Status](#status).

**Who it is for.** Levered traders with money at stake and someone to answer
to; teams rewriting or migrating an engine who must prove the behaviour did
not change; anyone who has to show results to a third party. **Not for** those
wanting a large indicator library, dozens of broker integrations, or hosting —
each of those conflicts directly with auditability. The reasoning and the
trade-offs are in [Why OpenQuanter exists](docs/WHY.md).

[English](README.md) · [中文](README.zh-CN.md)

> ⚠️ Early development. APIs are unstable before 2.0. Not financial advice; use at your own risk.

## What is OpenQuanter?

Most trading frameworks are platforms: you write inside their world, on their
terms, with their dependency tree. OpenQuanter is a set of components you
assemble.

**The whole engine has no third-party dependencies at all** — domain types,
journal, event core, matching, margin, backtest host, data plane, parity and
statistics are plain std Rust. Every crate that does carry a dependency tree
is one that has to speak to a venue: capture, the archive-to-tick bridge, and
the account reader. The boundary is the point, and it is checked in CI rather
than asserted:
[`scripts/check-composability.sh`](scripts/check-composability.sh) fails the
build if an engine crate acquires a dependency, or if a venue crate exceeds
its declared budget. (The examples crate additionally pulls `criterion` to run
its benchmarks; that is a dev-dependency of documentation, and nothing you
would depend on.)

So you can use the margin model without the engine. The overfitting statistics
without the backtester. The capture toolkit with no engine at all. Or the whole
stack, where one core runs both backtesting and live trading with only the
event producers swapped.

**Which markets.** The engine is not tied to an asset class: it consumes
timestamped market events and orders, which equities, futures, foreign
exchange and crypto all produce. What differs between them lives behind
adapters — how a venue is subscribed to, how its payloads are shaped, what a
trading day is, and how an account is margined.

Crypto perpetuals are implemented first because their venues are the easiest to
integrate against: public market data with no entitlement, and account access
that takes an API key rather than a contract. That is a reason about
onboarding, not about scope. Anything below that says *venue* means whichever
market you point it at; anything that says *funding* or *mark price* is
perpetuals-specific and named as such.

What the components are built to get right, in order:

1. **A backtest that does not flatter you.** Tiered margin and liquidation are
   modelled, not skipped — see the [worked example](docs/QUICKSTART.md#3-run-the-example-that-explains-the-project)
   where a margin-free run reports 20 908 USDT on an account that really ended
   with 61.53.
2. **Speed that does not cost fidelity.** Integer fixed point, no async on the
   hot path, allocation-free after warm-up. Most fast backtesters are fast
   because they simplify; the point here is not having to choose — and the
   price of not simplifying is a number rather than a claim:

   ```
   cargo run --release -p oq-examples --example throughput

   throughput   35.38 M ticks/s      # matching + margin + accounting + strategy
   ```

   With liquidation modelling switched off the same loop runs at roughly
   72 M ticks/s, so **margin fidelity costs about half the throughput**. That
   is the trade this project argues is worth making; the figure is here so you
   can judge it rather than take it on faith. Measured on an M4 Mac over a
   seeded market, in memory — reading and parsing tick files is not included.
   Reproduce it with the command above; `cargo bench -p oq-examples` gives the
   full breakdown.
3. **Runs you can reproduce and audit.** A journaled event stream means crash
   recovery, forensics, and reproducible research are the same mechanism.

### Where this comes from

OpenQuanter 1.x is a closed-source trading platform that has run live for
several years. It stays closed: it carries strategies, parameters and
operational history that are not ours to publish.

This project exists because 1.x hit a ceiling, and the binding constraint was
backtest throughput over large datasets. That sounds like an efficiency
complaint and is not one. When a parameter search takes days instead of an
afternoon, the cost is not the waiting — it is the studies you quietly stop
attempting, and the hypotheses that never get tested because testing them is
not worth the calendar. Slow research does not just take longer; it becomes
different, smaller research.

By the time the profiling was done and the obvious optimisations were in, what
remained was structural. No further tuning of that codebase was going to move
it, because the limit was the architecture rather than the code. So 2.x is a
ground-up rewrite on a Rust core rather than an incremental port.

Two things follow that are worth stating plainly. Everything here was shaped by
running a real system and hitting real walls — the margin work exists because a
backtest that could not liquidate had been quietly flattering results for
years, not because it seemed like a good feature. And the first requirement was
one specific need of one specific user, which is why this README leads with
what the components are rather than with the migration that started them.

The six concrete shapes that wall took — silent failure, an unattributable
gap, two implementations diverging unnoticed, slowness that narrows research,
conclusions that expire without saying so, and overfitting with no price tag —
are in [Why OpenQuanter exists](docs/WHY.md).

### Design pillars

These describe the architecture the project is being built towards. What
exists today is in [Status](#status) below — read that first if you are
deciding whether to use this now.

- **Composable, not monolithic** — every crate builds and is usable on its
  own, and the engine carries no third-party dependencies. Embedding one piece
  does not mean adopting a platform, and it does not widen your supply-chain
  surface.
- **Deterministic event core** — a pure state machine fed by a sequenced,
  journaled event stream (LMAX/Aeron lineage). This is the *mechanism*, not
  the point: it is what makes crash recovery, audit, reproducible research and
  fuzz testing the same piece of machinery rather than four subsystems.
- **Margin-aware backtesting** — tiered maintenance margin, liquidation-price
  paths, and funding-spike scenarios are first-class. Most open backtesters
  never liquidate you; real exchanges do.
- **Fidelity ladder** — L0 tick replay for fast parameter sweeps → queue-position
  and latency models → L2 orderbook reconstruction. Every backtest emits a
  fidelity report (participation rate, latency assumptions, margin peaks).
- **Two strategy tiers** — Rust traits for latency-sensitive strategies;
  Python (PyO3) for research velocity. One type system, no dual runtimes.
- **AI-native** — ONNX/compiled-tree inference in-process, vectorized
  gym-style environments for RL, and sandboxed hooks for LLM-driven research.
- **Overfitting statistics built in** — parameter sweeps report Deflated
  Sharpe Ratio and Probability of Backtest Overfitting by default.
- **Gap attribution** — decompose the live/backtest difference into slippage,
  queue position, funding, latency and fee tier, and report what will not
  decompose as an **unexplained residual**. This is the aim at the top of the
  page made concrete, and it is the **furthest away** of these pillars: it
  requires the live process to journal its decisions, which it does not yet.

## Documentation

| Document | Contents |
|---|---|
| [**Why OpenQuanter exists**](docs/WHY.md) | **What it is for, who for, and the wall a predecessor hit after years of live trading** |
| [Requirements Specification](docs/REQUIREMENTS.md) | What the framework must do, and how acceptance is measured |
| [Roadmap](docs/ROADMAP.md) | Milestones, entry triggers, exit gates, path to 2.0 |
| [Implementation Plan](docs/IMPLEMENTATION.md) | Architecture, design decisions, crate map, task plan |
| [Changelog](CHANGELOG.md) | What changed, and every note a semantics change is required to carry |

Full index: [docs/](docs/README.md).

## Status

Pre-alpha, and specific about it. **Built and tested today:**

- **Deterministic core** — sequenced journal, replay that reproduces outputs
  and state exactly (asserted by test, including a liquidation path),
  torn-tail recovery, journal-before-apply enforced by fault injection.
- **L0 matching** — tick replay with gap fill, price improvement and
  price-time priority. Frozen as the regression anchor.
- **Margin and costs** — tiered maintenance margin, liquidation pricing derived
  rather than copied, funding with spike injection, bitemporal rule schedules,
  and maker/taker fees (a maker rate may be negative, because rebates exist).
- **Backtest host** — including the margin deviation report, which runs a
  strategy twice and quantifies what a margin-free run overstates.
- **Data plane** — dual timestamps, leakage-free as-of joins, bitemporal
  reference data.
- **Capture** — verbatim venue records with local timestamps, UTC-day or
  hourly sealing, manifests with content hashes. Proven against a live
  venue. A venue is an adapter: which streams to subscribe, how the
  subscription is confirmed, where the exchange timestamp sits, and the
  quoting precision of each instrument, and what a trading day is —
  because a session that opens the evening before is one day and two
  UTC days. Two venues are implemented, chosen to differ: one puts the
  subscription in the URL and can only be confirmed by its first
  message, the other sends a JSON frame and acknowledges explicitly.
- **Proving a capture is usable** — bytes on disk show the messages
  arrived; only replay shows they can be used. `oq-book-check` rebuilds
  the order book and reports breaks the capture did not declare.
  `oq-trade-check` follows the venue's own trade ids, which is the one
  thing a hash cannot tell you: an unbroken run means nothing was
  missed, and a capture that silently dropped a third of the trades
  would pass every integrity check in the pipeline. `oq-merge` and
  `oq-resequence` reconcile archives that two writers or two runs
  produced.
- **Capture to backtest** — `oq-ingest` folds captured depth and trades
  into the tick format the engine replays. Conversion is deliberately
  lossy: a window of L2 becomes a best bid and a best ask, and the raw
  archive stays the record. Verified on a real captured day — 978,118
  depth updates and 127,276 trades became 26,237 one-second ticks with
  no unreadable payload.
- **Statistics** — deflated Sharpe ratio, probability of backtest
  overfitting, trial registry.
- **Parity** — trade-by-trade diffing with difference attribution, over
  baselines identified by code, data and configuration together.

- **The order path** — a venue-independent execution contract with one
  venue behind it. Placement returns three outcomes, not two: accepted,
  rejected, and *unknown*, because a timeout does not mean the order
  failed, it means nobody knows, and folding that into an error is what
  produces duplicate positions. Every order carries an id the caller
  chose before sending, which is the only handle that survives a request
  whose answer never came back. Fills arrive on a separate socket, a
  disconnect is reported as a gap rather than papered over, and an open
  socket that has stopped delivering is caught by disagreeing with the
  venue's own view of the positions three times running.
- **A pre-trade gate** — order size, resulting position, notional, a
  price band that catches the missing digit a notional cap waves
  through, a resting-order cap, a rate limit, and a kill switch that
  stays down until a person clears it. A passing check returns a permit
  carrying the order it approved, so a check cannot validate one order
  while a different one is sent. Limits nobody set refuse everything.

**Designed but not built:** fidelity tiers L1 and L2 (only L0 exists), the
Python strategy tier, a sweep runner, and everything under *AI-native*
above. The pillars section describes where this is going; this section
describes where it is.

**On live trading specifically,** because the pieces above make it easy to
overstate. The whole loop has run against a real venue's testnet.
`oq-trade` reads the contract's precision, grid and order floor from that
deployment, refuses to start beside a position it was not told about,
connects market data and the account stream, folds ticks, hands them to a
strategy, sends what the gate approved, watches the same order arrive on
the account stream, cancels it, and sees the cancellation confirmed there
too: 523 ticks, one order placed and withdrawn, no redelivered fills.

Those runs are the evidence for everything claimed above, and each of them
found something no unit test could reach — a price with the right number
of decimal places that was not a multiple of the tick size; a risk gate
handed a hardcoded position of zero, so its position cap could never fire;
a contract lookup that returned whichever symbol the venue listed first,
correct for one and silently wrong for six hundred; a read timeout
reported as a lost connection, costing fourteen reconnections in two
minutes on a market doing nothing; and an order below the venue's minimum,
because knowing the precision and the grid is not knowing the floor.

What has not happened is a strategy with an edge running unattended with
money behind it. The two strategies that ship are deliberately not
strategies: one never trades and exists to prove the loop, the other rests
one order far from the market and withdraws it.

**The assembly now exists**: `oq-live` composes market data, the strategy,
the risk gate and the order path into one process.

**The first half of the attribution chain is connected.** The live
process now depends on `oq-journal` and writes through `record.rs`, so
there is a record to replay.

**The second half is not.** `oq-live` still depends on neither `oq-core`
nor `oq-margin`: what it shares with the backtest is the strategy and the
matching types (`oq-strategy` → `oq-engine`), **not the kernel, the
ledger or the margin model**. So there is now something to replay and
nothing to replay it into, and a live run still cannot be diffed fill by
fill against what the kernel would have done.

So "every cent accounted for" still accounts for **none of them** — but
the gap is one link rather than two. `oq-core` has `sequencer::replay()`
and it is tested. **What is missing is the wire between it and the live
process.**

One clarification, because the names collide: the order book reconstruction
in `oq-l2feed` is a tool for **verifying an archive**, not the L2 fidelity
tier. It rebuilds a book to prove captured data is usable; matching against
a reconstructed book is M4 work and does not exist.

Start with the [Quickstart](docs/QUICKSTART.md) — three examples, no data to
download, a running backtest in a few minutes. See the
[Roadmap](docs/ROADMAP.md) for what each milestone unlocks and what triggers
it, and [Versioning](docs/VERSIONING.md) for what `2.0.0-alpha` promises.

## Building

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). By contributing you agree to the
project's contribution terms: a DCO sign-off on every commit, and the
[CLA](CLA.md) for substantial contributions. Support is best-effort; there is no SLA before 2.0.
