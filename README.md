# OpenQuanter

**Quantitative trading components in Rust. Take the whole engine, or one crate.**

[English](README.md) · [中文](README.zh-CN.md)

> ⚠️ Early development. APIs are unstable before 1.0. Not financial advice; use at your own risk.

## What is OpenQuanter?

Most trading frameworks are platforms: you write inside their world, on their
terms, with their dependency tree. OpenQuanter is a set of components you
assemble.

**Eleven of its twelve crates have no third-party dependencies at all.** The
whole engine — domain types, journal, event core, matching, margin, backtest
host, data plane, parity, statistics — is plain std Rust. The one crate with a
dependency tree is the one that has to speak to an exchange, and it is
isolated there on purpose. This is checked in CI, not asserted:
[`scripts/check-composability.sh`](scripts/check-composability.sh).

So you can use the margin model without the engine. The overfitting statistics
without the backtester. The capture toolkit with no engine at all. Or the whole
stack, where one core runs both backtesting and live trading with only the
event producers swapped.

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

## Documentation

| Document | Contents |
|---|---|
| [Requirements Specification](docs/REQUIREMENTS.md) | What the framework must do, and how acceptance is measured |
| [Roadmap](docs/ROADMAP.md) | Milestones, entry triggers, exit gates, path to 1.0 |
| [Implementation Plan](docs/IMPLEMENTATION.md) | Architecture, design decisions, crate map, task plan |

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
- **Capture** — verbatim venue records with local timestamps, UTC-day
  sealing, manifests with content hashes. Proven against a live venue.
- **Statistics** — deflated Sharpe ratio, probability of backtest
  overfitting, trial registry.
- **Parity** — trade-by-trade diffing with difference attribution, over
  baselines identified by code, data and configuration together.

**Designed but not built:** fidelity tiers L1 and L2 (only L0 exists), the
Python strategy tier, a sweep runner, live trading, and everything under
*AI-native* above. The pillars section describes where this is going; this
section describes where it is.

Start with the [Quickstart](docs/QUICKSTART.md) — three examples, no data to
download, a running backtest in a few minutes. See the
[Roadmap](docs/ROADMAP.md) for what each milestone unlocks and what triggers
it.

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
[CLA](CLA.md) for substantial contributions. Support is best-effort; there is no SLA before 1.0.
