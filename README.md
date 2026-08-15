# OpenQuanter

**A deterministic, AI-native quantitative trading framework in Rust — from CTA to HFT.**

> ⚠️ Early development. APIs are unstable before 1.0. Not financial advice; use at your own risk.

## What is OpenQuanter?

OpenQuanter is an open-source trading framework built around a deterministic
event core: the same engine runs backtesting and live trading, with only the
event producers swapped. It is designed for crypto perpetual markets first,
with a fidelity ladder that scales from fast tick-replay research to
orderbook-level simulation.

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

## Status

Pre-alpha. The workspace currently contains the initial crate skeleton.
See the roadmap in the issues/milestones of this repository.

## 中文简介

OpenQuanter 是一个以 Rust 确定性事件核为中心的开源量化交易框架：回测与实盘
共用同一引擎，保真度按需分级（tick 回放 → 排队/延迟建模 → L2 订单簿），
并将保证金/强平建模与 AI 能力（进程内推理、RL 训练环境、LLM 研究沙盒）作为
一等公民。面向加密永续市场优先，欢迎中文社区参与共建。

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). By contributing you agree to the
project's contribution terms (DCO sign-off required; CLA for substantial
contributions).
