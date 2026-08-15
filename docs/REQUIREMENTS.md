# OpenQuanter — Requirements Specification

> Status: **Draft for review** · [中文版](REQUIREMENTS.zh-CN.md)
> Applies to: OpenQuanter 2.x (the Rust-core rewrite)
> Companion documents: [Roadmap](ROADMAP.md) · [Implementation Plan](IMPLEMENTATION.md)

OpenQuanter 2.x is a ground-up rewrite of OpenQuanter on a Rust deterministic
event core. It is a new architecture, not an incremental port: the earlier
Python/Cython generation informs the semantics we must reproduce, but none of
its structure is carried over. This document specifies **what the framework must
do** and **how we will know it does it**. The sequencing lives in
[ROADMAP.md](ROADMAP.md); the *how* lives in
[IMPLEMENTATION.md](IMPLEMENTATION.md).

---

## 1. Positioning

**One sentence.** OpenQuanter is a deterministic, AI-native quantitative
trading framework: a single Rust event core drives both backtesting and live
trading, execution and margin fidelity are independently selectable, and
machine learning is a first-class citizen rather than a bolt-on.

**Where it sits.** The framework space already has strong entries — a
multi-asset Rust/Python engine with a mature message-bus architecture, a
high-fidelity crypto HFT backtester with queue-position modeling, and a large
Python ecosystem for Chinese futures markets. OpenQuanter targets the
intersection none of them currently occupy:

| Differentiator | Why it matters |
|---|---|
| **Margin and liquidation as an orthogonal fidelity layer** | Backtests that can never be liquidated systematically overstate leveraged strategies in the tail. Any fidelity tier can be run with `+margin` enabled. |
| **Crypto perpetuals first** | Funding, mark price, tiered maintenance margin, and forced-liquidation streams are modeled natively, not approximated by an equities account model. |
| **AI layered by evidence strength** | In-process inference is production-grade; RL environments arrive only once simulation fidelity justifies them; LLM-driven research is explicitly sandboxed and experimental. |
| **Determinism as a product feature** | Replay, audit, crash recovery, and randomized simulation testing are all the same mechanism, not four subsystems. |
| **Bilingual project** | Documentation and community support in both English and Chinese. |

**Explicitly not a claim.** OpenQuanter does not ship alpha. It ships the
machinery that makes alpha research honest: fidelity reporting, overfitting
statistics, deterministic reproduction, and a margin model that can actually
blow up your simulated account.

---

## 2. Users and use cases

| User | Primary use case | Capabilities relied on |
|---|---|---|
| Strategy researcher | Parameter sweeps, hypothesis testing, tail-risk assessment | Python tier, L0/L0+margin backtests, DSR/PBO statistics, analysis exports |
| Systematic (CTA-style) trader | Running medium-frequency strategies live | Python tier, Rust gateway, risk gate, journal audit trail |
| Market maker / HFT researcher | Queue-aware simulation, orderbook signals | Rust tier, L1/L2 fidelity, latency models |
| ML practitioner | Deploying trained models, RL experiments | ONNX/compiled-tree inference, vectorized environments, point-in-time features |
| Operator | Deployment, monitoring, incident forensics | CLI, journal audit stream, reconciliation, structured metrics |
| Framework integrator | Adding venues, embedding the core | Stable crate boundaries, connector contract, conformance test suite |

A **private overlay** deployment pattern is a supported first-class use case:
proprietary strategies, parameters, and captured data live in a private
repository that depends on the public crates one-way. The public repository
never contains proprietary content, and nothing in the public design assumes
access to it.

---

## 3. Functional requirements

Requirements are grouped by domain. Each carries a stability expectation:
**Core** (must exist for 1.0), **Extended** (planned, gated on a milestone
trigger), or **Experimental** (time-boxed, no compatibility promise).

### 3.1 Event core — `FR-CORE`

| ID | Requirement | Tier |
|---|---|---|
| FR-CORE-1 | The core is a pure state machine of the shape `apply(&mut State, Event) -> Outputs`. It must not read a wall clock, use RNG, perform I/O, or spawn threads. | Core |
| FR-CORE-2 | Time enters the core exclusively as injected `TimeEvent`s. Backtest, sandbox, and live differ only in the event producer and clock source. | Core |
| FR-CORE-3 | Every event is sequenced and appended to a memory-mapped journal **before** it reaches the core. | Core |
| FR-CORE-4 | Any run is bit-exactly reproducible from `(journal, seed, commit)`. Replaying a journal must produce identical outputs. | Core |
| FR-CORE-5 | State snapshots are triggered by in-stream events; recovery is snapshot + journal tail replay, tolerating a torn final record. | Core |
| FR-CORE-6 | The core is sharded by instrument; each shard is single-threaded. Cross-shard interaction happens through the sequenced stream, never shared mutable state. | Core |
| FR-CORE-7 | Journal readers (monitoring, analytics, IPC consumers) are observers. An observer must never be able to influence core state. | Core |

### 3.2 Matching and fidelity — `FR-MATCH`

| ID | Requirement | Tier |
|---|---|---|
| FR-MATCH-1 | Matching is organized as a **fidelity ladder** (§6). The selected tier is recorded in every run's output. | Core |
| FR-MATCH-2 | L0 semantics (tick replay with configurable crossing rules) are frozen once released; they are the migration and regression anchor. | Core |
| FR-MATCH-3 | Every backtest emits a **fidelity report**: participation rate, maker/taker split, latency assumptions, impact deductions, and — when margin is enabled — peak margin usage and closest approach to liquidation. | Core |
| FR-MATCH-4 | When measured participation rate exceeds a configurable threshold, the run is flagged: replay-based backtests lose validity once the simulated strategy would have moved the market. | Core |
| FR-MATCH-5 | L1 adds queue-position modeling, three-segment latency (feed / entry / response), and a square-root impact penalty for taker flow. | Extended |
| FR-MATCH-6 | L2 reconstructs the order book from incremental depth updates and matches against it, with periodic snapshot reconciliation and gap detection. | Extended |
| FR-MATCH-7 | Matching logic is expressed as pure functions covered by property tests: quantity conservation, price-time priority, no crossed book, no negative fills. | Core |

### 3.3 Margin, funding, and liquidation — `FR-MARGIN`

This is the differentiating subsystem and it is **orthogonal** to the fidelity
ladder: any tier may be run with margin enabled.

| ID | Requirement | Tier |
|---|---|---|
| FR-MARGIN-1 | Tiered maintenance-margin schedules are modeled per venue and per instrument, stored **bitemporally** — exchange rules change silently and historical backtests must use the rules in force at the simulated time. | Core |
| FR-MARGIN-2 | Margin usage and liquidation price are recomputed on every relevant tick, driven by mark price rather than last trade price where the venue does so. | Core |
| FR-MARGIN-3 | When a liquidation triggers, a liquidation order enters the matching engine like any other order and is subject to the same fidelity tier. | Core |
| FR-MARGIN-4 | Funding payments are applied on venue schedule, and funding-spike scenarios can be injected for stress testing. | Core |
| FR-MARGIN-5 | The framework can produce a **tail-divergence report**: the same strategy and data run with and without margin modeling, quantifying the optimism of margin-free simulation. | Core |
| FR-MARGIN-6 | Cross-margin, isolated-margin, and (where supported) portfolio-margin account modes are distinguishable. | Extended |
| FR-MARGIN-7 | Property tests enforce: margin usage is never negative, liquidation price moves monotonically with position size in the expected direction, and equity is conserved across fee/funding application. | Core |

### 3.4 Strategy layer — `FR-STRAT`

| ID | Requirement | Tier |
|---|---|---|
| FR-STRAT-1 | **Tier A**: strategies implemented as Rust traits, running in-process on the hot path. | Core |
| FR-STRAT-2 | **Tier B**: strategies implemented in Python via PyO3, sharing one type system with Tier A — no parallel object model, no v1/v2 split. | Core |
| FR-STRAT-3 | Tier B offers two modes: **compatibility mode** (naive per-event callbacks, prioritizes ease of porting, makes no throughput promise) and **throughput mode** (subscription granularity control, batched callbacks, mirrored state). | Core |
| FR-STRAT-4 | The same strategy source runs unmodified in backtest, sandbox, and live. Environment differences are configuration, not code. | Core |
| FR-STRAT-5 | A strategy can express orders, cancels, and amendments; the framework guarantees each is journaled before transmission. | Core |
| FR-STRAT-6 | Common indicator and position-sizing components ship as reusable building blocks, not as an inheritance hierarchy strategies must join. | Core |

### 3.5 Risk control — `FR-RISK`

| ID | Requirement | Tier |
|---|---|---|
| FR-RISK-1 | A **RiskGate** sits between strategy output and any venue, and cannot be bypassed by strategy code — including by Tier A strategies compiled into the same binary. | Core |
| FR-RISK-2 | Pre-trade checks cover: notional and position limits, order rate, price sanity bands, self-trade prevention, and instrument allow-lists. | Core |
| FR-RISK-3 | A kill switch halts all order emission and optionally flattens, reachable from CLI and from an operator signal. | Core |
| FR-RISK-4 | On startup and periodically thereafter, the framework reconciles positions, orders, and balances against the venue. **Unknown state is fatal**: an irreconcilable difference stops trading rather than guessing. | Core |
| FR-RISK-5 | Risk limits are configuration, versioned and journaled — a limit change is an auditable event. | Core |

### 3.6 Venue connectivity — `FR-VENUE`

| ID | Requirement | Tier |
|---|---|---|
| FR-VENUE-1 | Venue adapters implement a documented connector contract: market data, order entry, user stream, and reconciliation are separable concerns. | Core |
| FR-VENUE-2 | A **conformance test suite** exercises every adapter against recorded and synthetic venue behavior, including lost, duplicated, and out-of-order reports. | Core |
| FR-VENUE-3 | Reconciliation is a first-class design object in every adapter, not an afterthought — the venue is the source of truth at startup. | Core |
| FR-VENUE-4 | Order identifiers support a broker/referral prefix scheme from day one, so integrators can attribute flow without patching the adapter. | Core |
| FR-VENUE-5 | Credentials are never read from committed configuration. Adapters accept them from environment or OS keyring, and only the gateway process holds them in memory. | Core |
| FR-VENUE-6 | Reference adapters ship for at least one major perpetuals venue at 1.0, with additional venues following the same contract. | Core |

### 3.7 Data plane — `FR-DATA`

| ID | Requirement | Tier |
|---|---|---|
| FR-DATA-1 | Every market data record carries **dual timestamps**: exchange time and local receive time, both nanosecond precision. | Core |
| FR-DATA-2 | Storage is columnar (Arrow/Parquet) with a documented schema; time-series database integration is optional, not assumed. | Core |
| FR-DATA-3 | Reference-data that changes over time (funding schedules, margin tiers, contract specifications, listings) is stored bitemporally. | Core |
| FR-DATA-4 | As-of joins use **strict `<`** semantics by default; the framework must not silently allow a same-timestamp record to leak into a feature. | Core |
| FR-DATA-5 | A capture toolkit records raw venue streams verbatim — no merging, no downsampling — with gap markers on reconnect and a recorded clock-offset estimate. | Core |
| FR-DATA-6 | A point-in-time feature layer computes features from one code path for both research and live, and reports online/offline consistency drift. | Extended |

### 3.8 Research workflow — `FR-RESEARCH`

| ID | Requirement | Tier |
|---|---|---|
| FR-RESEARCH-1 | Parameter sweeps are a first-class command with parallel execution and structured result output. | Core |
| FR-RESEARCH-2 | Sweeps automatically report **Deflated Sharpe Ratio** and **Probability of Backtest Overfitting**, with the number of trials tracked across the sweep. | Core |
| FR-RESEARCH-3 | Results exceeding an overfitting threshold are marked and, in strict mode, refused for deployment packaging. | Core |
| FR-RESEARCH-4 | Backtest output is analysis-friendly: trade-level records, equity curve, and fidelity report in open formats. | Core |
| FR-RESEARCH-5 | A **parity harness** performs trade-by-trade diffing between two runs with difference attribution — used for validating ports, refactors, and fidelity-tier changes. | Core |

### 3.9 AI and ML — `FR-AI`

Layered by evidence strength. We ship what is demonstrably reliable and label
the rest honestly.

| ID | Requirement | Tier |
|---|---|---|
| FR-AI-1 | In-process inference: ONNX runtime and compiled gradient-boosted trees callable from the hot path with bounded latency and no allocation per call. | Extended |
| FR-AI-2 | A **prediction parity gate** verifies that the Rust inference path and the Python training path agree within tolerance, catching float32 threshold drift in tree models. | Extended |
| FR-AI-3 | Gym-style environments with inverted control and native vectorized batching, with the seed threaded through every random source. | Extended |
| FR-AI-4 | RL environments are gated on L1 fidelity: training against a low-fidelity simulator teaches models to exploit simulator artifacts. | Extended |
| FR-AI-5 | LLM-driven research runs in a sandbox with a typed read-only tool API and a full audit log. LLM output is a *proposal*; hard limits always live in the engine. | Experimental |

### 3.10 Operations and tooling — `FR-OPS`

| ID | Requirement | Tier |
|---|---|---|
| FR-OPS-1 | A single CLI covers `backtest`, `sweep`, `live`, `replay`, `parity`, and `data` subcommands. | Core |
| FR-OPS-2 | The recommended live topology is **one process per account**: a process owns one account, its strategies, and its journal, so a fault in one account cannot corrupt another. Cross-process aggregation happens by reading journals. | Core |
| FR-OPS-3 | Graceful restart preserves open positions and working orders through snapshot, reconnect, and reconciliation. | Core |
| FR-OPS-4 | A documented **position-carrying cutover procedure** exists and is exercised in testnet before any production use. | Core |
| FR-OPS-5 | Metrics are emitted as histograms (not averages) for latency-sensitive paths, with clearly defined measurement boundaries. | Core |
| FR-OPS-6 | Clock discipline is documented and verified: hosts run NTP/chrony, and capture processes archive their clock-offset estimate alongside the data. | Core |

---

## 4. Non-functional requirements

| ID | Requirement |
|---|---|
| NFR-1 **Determinism** | No wall-clock reads, RNG, I/O, or threading inside the core. Enforced by review, by lint where possible, and by replay tests in CI. |
| NFR-2 **Latency** | Live hot path, measured from journal write to order bytes leaving the gateway socket: p99 ≤ 100 µs. JSON parsing and request signing are budgeted and reported separately. |
| NFR-3 **Throughput** | Single-strategy tick-replay backtest of a multi-year dataset completes at least 8× faster than an equivalent interpreted/Cython implementation, in throughput mode. |
| NFR-4 **Memory** | Hot path is allocation-free after warm-up; buffers are preallocated per shard. |
| NFR-5 **Portability** | Linux x86-64 is the primary target; macOS (Intel and Apple Silicon) is supported for development. Python bindings target abi3 across supported CPython versions. |
| NFR-6 **Safety** | `unsafe` is warned on by default and requires justification in review. Zero-copy layouts use audited crates rather than hand-rolled transmutes. |
| NFR-7 **Fixed-point discipline** | The hot path uses `i64` fixed-point arithmetic; decimal types are used only at ledger and reporting boundaries. Float is never used for money. |
| NFR-8 **Schema evolution** | Serialized event fields are **never repurposed**. New semantics get new fields; old fields are deprecated, not reused. |
| NFR-9 **Onboarding** | A new user reaches a running example backtest in ≤ 30 minutes from a clean machine, using shipped sample data. |
| NFR-10 **Agent-friendly codebase** | Each crate carries a short `AGENTS.md` (<200 lines) stating local commands and invariants. Verification is layered: unit → property → golden → parity. Golden baselines change only with explicit human confirmation. |
| NFR-11 **Bus-factor resistance** | Design rationale lives in the repository, not in anyone's head. Behavioral knowledge is encoded in deterministic tests, not tribal memory. |
| NFR-12 **Versioning** | Pre-1.0: APIs may break at any minor version, with changes listed in the changelog. Post-1.0: semantic versioning is enforced for public crate APIs and the Python binding surface. |

---

## 5. Measurable acceptance goals

Each goal has a defined verification method. A goal is not "done" until its
verification runs in CI or as a documented, reproducible command.

| # | Goal | Target | Verification |
|---|---|---|---|
| **G1** | Core determinism | Replaying any journal reproduces outputs exactly; `(seed, commit)` reproduces any simulated scenario | Replay test suite in CI |
| **G2** | Semantic parity harness | Trade-by-trade equality (time, price, quantity, side) with relative P&L error ≤ 1e-6 against a reference run | `oq-parity` diff + attribution report |
| **G3** | Backtest throughput | ≥ 8× over the interpreted/Cython baseline for a multi-year single-strategy run in throughput mode; compatibility mode need only not regress | Same-machine, same-config benchmark |
| **G4** | Sweep throughput + statistics | 100 configurations in ≤ 30 minutes on a reference machine, with DSR and PBO emitted automatically | Sweep benchmark + statistics report |
| **G5** | Margin fidelity | Tiered maintenance margin, mark-price liquidation paths, and funding-spike scenarios reproduce venue behavior; tail-divergence report produced | Recomputation of historical stress windows + spot-check against venue calculators |
| **G6** | Live hot-path latency | p99 ≤ 100 µs from journal write to socket write | HDR histogram instrumentation |
| **G7** | Strategy portability | A strategy runs unchanged in compatibility mode; the same strategy in throughput mode meets G3 and re-passes G2 | Both modes benchmarked and parity-checked |
| **G8** | HFT fidelity | L1 queue + latency + impact; L2 book reconstruction; stylized-facts test set passes | Fidelity test suite + calibration report |
| **G9** | Inference | Single-row GBDT inference ≤ 10 µs in-process; Python/Rust prediction parity gate passes | Inference benchmark + parity test |
| **G10** | RL environments | Vectorized batch environments with full seed propagation and reproducible training runs | Training throughput benchmark + reproduction test |
| **G11** | Adoption readiness | Cold start to first backtest ≤ 30 minutes; public CI green; semantic versioning after 1.0 | External-user cold-start trial |

---

## 6. The fidelity ladder

Fidelity is two independent axes: **execution realism** (the ladder) and
**account realism** (the margin overlay). Runs declare both.

| Tier | Matching semantics | Data required | Typical use |
|---|---|---|---|
| **L0** | Tick replay with configurable crossing rules; frozen as the regression anchor | Trade/tick series | Fast sweeps, CTA research, migration validation |
| **L0 + margin** | L0 plus the full margin overlay | L0 data + mark price + margin rule tables | **Honest tail risk for leveraged strategies** |
| **L1** | Queue-position model, three-segment latency, square-root impact penalty | Ticks + best bid/offer + calibration samples | Pre-deployment validation, maker strategy screening |
| **L2** | Order book reconstructed from incremental depth; matching against the book | Full incremental depth + periodic snapshots | Market making, high-frequency research |
| **L3 / interactive** | Order-by-order FIFO, or reactive simulation | MBO data / calibrated reactive model | Long-term research direction |

**Rules of the ladder.**

1. Higher tiers are not automatically "better" — they are more expensive and
   require data you may not have. The framework reports what a run actually
   assumed rather than implying realism it cannot support.
2. The margin overlay composes with every tier.
3. Generative order book simulators are a research tool, never the arbiter of
   a strategy's P&L.
4. Above the participation-rate threshold, replay-based results are flagged as
   out-of-domain regardless of tier.

---

## 7. Non-goals

- **Not doing:** kernel bypass, FPGA, or custom NIC drivers. The latency
  target is achievable in user space and the complexity is not worth it here.
- **Not doing:** an external message bus (Redis or similar) on the hot path.
  The journal is the bus.
- **Not doing:** a universal exchange abstraction layer generated from a
  common schema. Venue adapters are written per venue against a thin contract,
  because venue semantics genuinely differ.
- **Not doing:** async runtimes with work stealing inside the core. Async
  belongs at the gateway edge only.
- **Not doing:** maintaining two object models for Rust and Python.
- **Not doing:** generative simulators as a P&L judgment source.
- **Not doing:** hosted execution of user strategies with custody of user API
  keys. Custodial key handling is a liability class this project will not take on.
- **Not promising:** API stability before 1.0, or a support SLA. Community
  support is best-effort.
- **Not shipping:** trading strategies with production parameters, proprietary
  datasets, or anything resembling investment advice.

---

## 8. Repository boundary policy

The public repository contains the framework, teaching examples, sample data,
and public CI. It must never contain production strategies, live parameters,
credentials, captured proprietary market data, deployment topology details, or
incident records. Users who run the framework in production are expected to
keep such material in a private overlay that depends on the public crates in
one direction only.

This is a hard rule with three consequences that shape the public design:

1. Public quality gates run on **sample data goldens plus property tests**;
   they never require access to private datasets.
2. Simulation scenarios contributed publicly describe **generic failure
   patterns** (out-of-order reports, partial-fill races, feed gaps), not
   operational specifics of any deployment.
3. Every public interface must be usable and testable without any private
   component present.

---

## 9. Glossary

| Term | Meaning |
|---|---|
| **Journal** | Append-only memory-mapped event log; simultaneously the audit trail, replay source, recovery mechanism, and inter-process transport. |
| **Sequencer** | The component that assigns a total order to events and writes them to the journal before the core sees them. |
| **Fidelity tier** | Selected level of execution realism (L0–L3). |
| **Margin overlay** | Orthogonal account-realism layer adding maintenance margin, liquidation, and funding. |
| **Parity** | Trade-by-trade semantic equality between two runs, with attribution of any difference. |
| **Compatibility mode** | Python tier mode optimized for straightforward porting; no throughput promise. |
| **Throughput mode** | Python tier mode with batched callbacks and subscription control; meets the throughput goal. |
| **DSR / PBO** | Deflated Sharpe Ratio / Probability of Backtest Overfitting — sweep-level overfitting statistics. |
| **Participation rate** | Fraction of market volume the simulated strategy would have consumed; the validity boundary of replay backtesting. |
| **VOPR-style simulation** | Randomized, seed-reproducible whole-system fault simulation. |
