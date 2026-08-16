# OpenQuanter — Roadmap

> Status: **Draft for review** · [中文版](ROADMAP.zh-CN.md)
> Companion documents: [Requirements](REQUIREMENTS.md) · [Implementation Plan](IMPLEMENTATION.md)

This roadmap describes the path from the current pre-alpha skeleton to a 1.0
release, and the research directions beyond it. It is organized by
**milestone**, not by date. Milestones have **entry triggers** and **exit
gates**; work does not start because a calendar says so, and does not finish
because someone says it feels done.

Effort figures are indicative planning ranges in person-weeks for a small team,
and already include a contingency multiplier. They are estimates, not
commitments.

---

## Guiding principles

1. **Anchor first.** Determinism and the parity harness come before features.
   Without them, every later change is a guess.
2. **Fidelity before AI.** Reinforcement learning on a low-fidelity simulator
   teaches models to exploit the simulator. The AI milestones are gated on the
   fidelity milestones.
3. **Risk value counts as much as performance value.** Margin modeling is
   prioritized above speed work because a backtest that cannot be liquidated
   is wrong in the one place that matters most.
4. **Trigger, don't schedule, the expensive parts.** Live trading, HFT
   fidelity, and AI extensions each carry an explicit entry condition. If the
   condition is not met, the milestone does not start.
5. **Data is time-sensitive; code is not.** A day of market data not captured
   is permanently lost. Capture infrastructure therefore starts at M0, before
   the code that will consume it exists.

---

## Milestone overview

| Milestone | Theme | Cumulative effort | Status |
|---|---|---|---|
| **M0** | Foundations: repository, capture, statistics | 3–6 pw | Mostly landed |
| **M1** | Deterministic core, L0 engine, margin skeleton → first preview release | 15–27 pw | Largely landed |
| **M2** | Python tier, margin fidelity reporting, research workflow → beta | 24–41 pw | Committed |
| **M3** | Live trading: gateways, risk gate, reconciliation | 39–62 pw | Triggered |
| **M4** | HFT fidelity: L1 queue/latency, L2 book reconstruction | 54–83 pw | Triggered |
| **M5** | AI extensions: inference, RL environments, feature layer | 62–97 pw | Triggered |
| **1.0** | API stabilization and semantic versioning | — | After M3 + external adoption |

"Largely landed" on M0 and M1 means most of the scope is built and tested,
not that the exit gate has passed. As of the latest revision: the
deterministic core, L0 matching, the margin overlay, the backtest host, the
parity harness, the capture toolkit and the overfitting statistics are in
and green. Still open on M1's gate are `criterion` benchmarks in CI, and
trade-by-trade parity against a reference run — close, but not passed, and
a gate that is nearly met is not met. Continuous capture is still a trial
rather than a service. See the README for the built/not-built split.

"Committed" means it is the current default plan. "Triggered" means the
milestone has an entry condition stated below and will not be started before
that condition is met.

---

## M0 — Foundations

**Goal.** Put in place the things whose value decays if delayed, and the
minimum project scaffolding for external contribution.

**Scope.**

- Public repository scaffold: workspace layout, Apache-2.0, DCO enforcement,
  CI (build, test, fmt, clippy with warnings denied), `AGENTS.md` conventions.
- Crate name reservations on crates.io (`oq-*` placeholders) to avoid
  namespace squatting.
- **Market data capture toolkit and running capture.** Full incremental depth,
  best bid/offer, aggregated trades, mark price and funding rate, forced
  liquidation stream, plus a daily snapshot of leverage/maintenance-margin
  tier tables. Dual timestamps on every record; raw messages archived verbatim;
  gap markers on reconnect; clock-offset estimate archived with the data.
- **Overfitting statistics** (DSR, PBO/CSCV) implemented and usable
  standalone, so research quality improves before the new engine exists.
- **Simulation scenario catalogue**: a written catalogue of generic failure
  modes the system must survive — out-of-order or duplicated execution reports,
  lost cancels, synthetic zero-price fills, stale user streams, reconnect
  storms, feed gaps. These become the seed corpus for `oq-sim`.

**Exit gate.**

- Capture running continuously with a measured gap rate and documented
  storage growth.
- DSR/PBO computable on real sweep output.
- Scenario catalogue ≥ 6 entries with reproduction descriptions.
- Public CI green on the workspace skeleton.

---

## M1 — Deterministic core, L0 engine, margin skeleton

**Entry.** M0 exit gate met. This is the anchor milestone: everything later
depends on the invariants established here.

**Scope.**

- `oq-parity` **first** — trade-by-trade diff and difference attribution,
  with baselines identified by the (commit, data hash, config hash) triple so
  a stale baseline reports itself instead of masquerading as a regression.
  Building the measuring instrument before the thing being measured is
  deliberate.
- `oq-types`: domain types, `i64` fixed-point arithmetic, typestate order and
  position state machines.
- `oq-journal`: memory-mapped append log, snapshots, replay, torn-tail
  tolerance.
- `oq-core`: sequencer, deterministic kernel, injected clock, instrument
  sharding.
- `oq-engine` L0: tick-replay matching, specified precisely enough that its
  semantics can be frozen as the regression anchor.
- `oq-margin` skeleton: tiered maintenance margin tables (stored bitemporally),
  per-tick margin usage, liquidation price computation.
- `oq-backtest`: run scheduling, funding application, accounting, result
  export, participation-rate measurement, fidelity report.
- Property tests for engine and margin invariants; `oq-sim` prototype running
  the first scenarios from the M0 catalogue; `criterion` benchmarks in CI.

**Exit gate.**

- **G1** — journal replay reproduces outputs exactly.
- **G2** — parity harness demonstrates trade-by-trade equality with relative
  P&L error ≤ 1e-6 against a reference implementation run.
- Property test suite green, including margin invariants.
- **First public preview release (0.x tag)** with sample data and one example
  strategy.

---

## M2 — Python tier, margin fidelity, research workflow

**Entry.** M1 exit gate met.

**Scope.**

- `oq-py`: compatibility mode formalized; throughput mode designed and
  implemented (subscription granularity, batched callbacks, mirrored state).
- **Margin fidelity reporting (G5).** Recompute historical stress windows with
  and without the margin overlay and publish the methodology behind the
  **tail-divergence report**. For leveraged strategies this is the single most
  valuable deliverable of the entire plan: it quantifies how optimistic
  margin-free backtesting actually is.
- `oq-data`: dual-timestamp Arrow/Parquet layer, bitemporal reference data,
  strict as-of join utilities; `oq-features` skeleton.
- `oq-cli`: `backtest`, `sweep`, `replay`, `parity`, `data` subcommands;
  `oq-stats` integrated so sweeps emit DSR/PBO by default.
- **Adoption readiness (G11):** quickstart documentation, at least two example
  strategies, sample dataset with golden tests, and a verified cold-start run
  by someone outside the core team.

**Exit gate.**

- **G3** throughput and **G4** sweep targets met.
- **G5** margin fidelity verified; tail-divergence methodology published.
- **G7** — a strategy runs unchanged in compatibility mode and, after
  throughput-mode conversion, re-passes parity.
- **G11** initial verification: external cold start ≤ 30 minutes.
- **Beta release** with documented, if still unstable, APIs.

---

## M3 — Live trading

**Entry trigger.** All of:

1. The public core has been released for ≥ 6 months with no open P0/P1 defects.
2. A parity gate plus shadow-run comparison passes for the operator's own
   strategies.
3. The position-carrying cutover procedure has been rehearsed successfully on
   testnet at least twice.
4. At least one real third-party user is running the framework — external
   usage is the only honest quality signal a small team can get.

**Scope.** Effort is deliberately weighted toward connectivity and
reconciliation rather than the core: in systems of this shape, incidents
overwhelmingly originate at the venue boundary and in state reconciliation,
not in the matching kernel.

- `oq-gateway`: reference perpetuals adapter — market data, order entry, user
  stream — with **reconciliation as a first-class design object**: full
  handling paths for lost, duplicated, and out-of-order reports. Plus the
  connector conformance test suite.
- Additional venue adapters against the same contract.
- `oq-risk`: RiskGate with pre-trade checks, kill switch, and startup
  reconciliation with fatal-on-unknown-state semantics.
- `oq-live`: process assembly, snapshot recovery, graceful restart, one
  process per account.
- Observability: latency histograms at the defined measurement boundary,
  structured metrics, alert integration hooks.
- `oq-sim` at full strength: the entire scenario catalogue plus gateway fuzzing
  (disconnects, reordering, duplication, partial fills).
- **Position-carrying cutover playbook**, rehearsed end-to-end.

**Exit gate.**

- **G6** latency target met at the defined boundary.
- Shadow-run agreement rate at target, with every divergence attributed.
- Full scenario catalogue passing under fuzzing.
- Staged rollout stable for ≥ 2 weeks.

---

## M4 — HFT fidelity

**Entry trigger.** Both of:

1. A concrete maker or high-frequency strategy candidate exists whose edge is
   limited by simulation fidelity — not a hypothetical one.
2. At least 6 months of incremental depth data has been captured.

**Scope.**

- **L1**: queue-position modeling (conservative model first; probabilistic
  model enabled only after calibration against recorded fills), three-segment
  latency with distributions rather than constants, square-root impact penalty,
  participation-rate alerting.
- **L2**: order book reconstruction from incremental depth, matching against
  the reconstructed book, snapshot reconciliation and gap handling.
- Stylized-facts test set in CI (fat tails, volatility clustering, order flow
  autocorrelation) validating that the simulator behaves like a market.
- Comparative report across L0 / L0+margin / L1 for the same strategy.

**Exit gate.** **G8** met, plus a calibration report comparing modeled fill
probability against recorded fills.

---

## M5 — AI extensions

Each component triggers independently.

| Component | Entry trigger | Delivers |
|---|---|---|
| `oq-infer` | A trained model is ready to run in the loop | ONNX and compiled-tree inference in-process, warm-up and fixed shapes, Python/Rust prediction parity gate (**G9**) |
| `oq-env` | L1 fidelity achieved (M4) | Gym-style environments with inverted control and native vectorized batching, seeds threaded through every random source (**G10**) |
| `oq-features` | Alongside `oq-infer` | Point-in-time feature layer, one code path for research and live, online/offline consistency metrics |
| `oq-lab` | Time-boxed exploration only | LLM sandbox with typed read-only tool API and audit log. No acceptance target, no compatibility promise |

**On `oq-lab` specifically.** Published evidence for LLM-driven strategy
discovery is weak: reproducibility audits of the literature find close to
nothing that replicates, and model training data has typically already seen the
backtest windows being evaluated. We keep an experimental sandbox because the
tooling is cheap and the option value is real, and we label it experimental
because pretending otherwise would be dishonest. Evaluation uses only data
after the model's training cutoff.

---

## Road to 1.0

1.0 is an **API stability commitment**, not a feature count. It requires:

- M3 complete, with the framework running live somewhere other than the
  maintainers' machines.
- Public crate APIs and the Python binding surface reviewed for stability;
  known-wrong shapes fixed *before* they are frozen.
- Event schema versioned with a documented evolution policy (fields are added,
  never repurposed).
- Connector contract documented well enough for a third party to write an
  adapter without reading adapter internals.
- Documentation covering quickstart, architecture, fidelity semantics, margin
  semantics, and operations.
- Semantic versioning enforced from that point forward.

---

## Release cadence

| Track | Cadence | Contents |
|---|---|---|
| `main` | Continuous | Always green; property tests and goldens gate every merge |
| Preview `0.x` | Per milestone | Tagged at each milestone exit gate |
| Patch | As needed | Correctness fixes; never silently changes engine semantics |

Any change to L0 matching semantics, margin computation, or the event schema
requires an explicit note in the changelog and a parity report showing the
behavioral delta. Golden baselines are regenerated only with human
confirmation recorded in the pull request.

---

## Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| L0 semantics drift during implementation | High | Parity harness built first; every difference attributed, none waived |
| Margin rules reproduced incorrectly (tier tables, cross-margin details) | High | Bitemporal rule storage; spot-checks against venue liquidation calculators; property tests on monotonicity |
| Throughput-mode conversion changes strategy behavior | High | Throughput mode independently re-passes the parity gate |
| PyO3 callback overhead dominating the Python tier | Medium-high | Two-mode design isolates the risk; compatibility mode makes no speed promise |
| Capture infrastructure undersized (volume, gaps, cost) | Medium-high | Dedicated capture host with local storage and batch archival; a 7-day trial run measures volume, gap rate, and cost before committing |
| Insufficient calibration data for L1 | Medium | Capture starts at M0; until enough data exists, only the conservative queue model is enabled |
| Type system splitting into Rust and Python dialects | Medium | Single type system rule; bindings expose the same types, never a parallel model |
| Scope inflation | High | Committed scope ends at M2; everything later is trigger-gated |
| Proprietary content leaking into the public repository | High | Fresh history; secret and pattern scanning in CI; proprietary material only in private overlays; pre-release manual audit |
| Bus factor of one | Medium-high | Agent-friendly codebase (per-crate `AGENTS.md`, layered verification anchors); behavior encoded in deterministic tests; all design rationale written down rather than remembered |
| Building the framework becomes the goal instead of using it | High | Every milestone states the capability it unlocks; the question "what got measurably better because of this?" is asked at each gate |

---

## How to follow along and contribute

Milestone progress is tracked in GitHub issues and milestones. The highest-value
contributions right now are, in order:

1. Precise bug reports with reproduction seeds — determinism makes these
   unusually actionable.
2. Venue adapters written against the connector contract.
3. Scenario contributions for `oq-sim` describing generic failure modes.
4. Documentation and translation.

See [CONTRIBUTING.md](../CONTRIBUTING.md). Support is best-effort; there is no
SLA before 1.0.
