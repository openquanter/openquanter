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

Pre-alpha. The workspace currently contains the initial crate skeleton.
Milestone progress is tracked in this repository's issues and milestones; see
the [Roadmap](docs/ROADMAP.md) for what each milestone unlocks.

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
