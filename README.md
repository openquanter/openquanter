# OpenQuanter

**A deterministic, AI-native quantitative trading framework in Rust — from CTA to HFT.**

[English](README.md) · [中文](README.zh-CN.md)

> ⚠️ Early development. APIs are unstable before 1.0. Not financial advice; use at your own risk.

## What is OpenQuanter?

OpenQuanter is an open-source trading framework built around a deterministic
event core: the same engine runs backtesting and live trading, with only the
event producers swapped. It is designed for crypto perpetual markets first,
with a fidelity ladder that scales from fast tick-replay research to
orderbook-level simulation.

The 2.x line is a ground-up rewrite on a Rust core — a new architecture rather
than an incremental port.

### Design pillars

These describe the architecture the project is being built towards. What
exists today is in [Status](#status) below — read that first if you are
deciding whether to use this now.

- **Deterministic event core** — a pure state machine fed by a sequenced,
  journaled event stream (LMAX/Aeron lineage). Every run is replayable from
  `(journal, seed)`; crash recovery, audit trail, and simulation testing come
  from the same mechanism.
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
- **Margin** — tiered maintenance margin, liquidation pricing derived rather
  than copied, funding with spike injection, bitemporal rule schedules.
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
