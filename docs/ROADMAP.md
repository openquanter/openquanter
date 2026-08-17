# OpenQuanter — Roadmap

> Status: **Draft for review** · [中文版](ROADMAP.zh-CN.md)
> Companion documents: [Why this exists](WHY.md) · [Requirements](REQUIREMENTS.md) · [Implementation Plan](IMPLEMENTATION.md)

**What every milestone is ultimately for:** every cent between a backtest and
the live run, accounted for. [Why that is the goal](WHY.md), and what the
predecessor hit that made it the goal, is a separate document; this one is
about the order of the work.

This roadmap describes the path from where the project is today to a 2.0
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
| **M3** | Live trading: gateways, risk gate, reconciliation | 39–62 pw | **Half built, entry triggers unmet** |
| **M4** | HFT fidelity: L1 queue/latency, L2 book reconstruction | 54–83 pw | Triggered |
| **M5** | AI extensions: inference, RL environments, feature layer | 62–97 pw | Triggered |
| **2.0** | API stabilization and semantic versioning | — | After M3 + external adoption |

"Largely landed" on M0 and M1 means most of the scope is built and tested,
not that the exit gate has passed. As of the latest revision: the
deterministic core, L0 matching, the margin overlay, the backtest host, the
parity harness, the capture toolkit and the overfitting statistics are in
and green, and `criterion` benchmarks now exist with a throughput floor
enforced in CI. Still open on M1's gate is trade-by-trade parity against a
reference implementation run — close, but not passed, and a gate that is
nearly met is not met. Capture has since run continuously for over a
day across 23 streams on two venues with zero archive failures, which is
past a trial and short of the exit gate: that asks for a *measured* gap
rate and documented storage growth, and what exists is an uptime figure. See the README for the built/not-built split.

One M1 item was deliberately built smaller than planned. The benchmark job
asserts a **floor**, not a tracked baseline: shared CI runners vary by
several times from hour to hour, so a tight comparison would fail on noise
and be switched off within a week. The floor catches the engine becoming an
order of magnitude slower, which is the regression that actually happens.
Comparing two versions precisely is a local `cargo bench` job on one
machine. This is a change to the plan, recorded rather than quietly
dropped.

**M3 was built to roughly half its scope with none of its four entry
triggers met, and that is a departure from this document's own discipline
rather than a revision of it.** The gateway speaks order entry and the user
stream, the risk gate exists with a kill switch and a fatal startup check,
and `oq-live` assembles them into a process that has placed and cancelled a
real order on a testnet and seen both confirmed on the account stream. None
of M3's four conditions — six months of public release, an attributed
shadow run, two rehearsed cutovers, one outside user — has happened.

Recorded because principle 4 at the top of this page says trigger, don't
schedule, the expensive parts, and this is what it looks like when that is
not followed. The work is not wasted and none of it is being reverted; the
cost is that the ordering was the thing meant to ensure the expensive half
got built against evidence, and it was built against enthusiasm instead.

Still unbuilt in M3, so that "half" is a number rather than a feeling:
snapshot recovery and graceful restart in `oq-live` (present in its
comments, absent from its functions), the connector conformance suite, a
second execution venue, observability of any kind, the live/backtest
attribution, `oq-sim`, and the cutover playbook.

**G6 is not merely unmet — by its own definition it is unmeasurable.** It
asks for p99 latency from journal write to socket write, and the live path
does not journal: `oq-live` depends on neither `oq-core` nor `oq-journal`,
so there is no first timestamp to measure from. This is the same missing
wire as the attribution item in M3's scope, which is why that item calls
itself the first task of the milestone rather than the last.

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
- **First public preview release** — a `2.0.0-alpha.N` tag, with sample data and one example
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
2. A strategy has been run in shadow against a live venue for long enough to
   compare, and every divergence between the shadow run and the venue is
   attributed rather than tolerated.
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
- **Attribution of the live/backtest gap.** Both the entry trigger and the exit
  gate above require every divergence to be *attributed rather than tolerated*,
  and nothing currently produces that attribution — the requirement names a
  standard without naming an instrument. The instrument decomposes the gap into
  slippage, queue position, funding against model, latency, and fee tier, and
  reports what will not decompose as an **unexplained residual**. Its
  prerequisite is that the live process journals its decisions, which today it
  does not: `oq-live` depends on neither `oq-core` nor `oq-journal`, so there is
  no record to replay and therefore nothing to attribute against. Wiring that is
  the first task of this milestone, not the last.
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

## Road to 2.0

2.0 is an **API stability commitment**, not a feature count. It requires:

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
| Preview `2.0.0-alpha.N` / `-beta.N` | Per milestone | Tagged at each milestone exit gate |
| Patch | As needed | Correctness fixes; never silently changes engine semantics |

Any change to L0 matching semantics, margin computation, or the event schema
requires an explicit note in the [changelog](../CHANGELOG.md) and a parity report showing the
behavioral delta. Golden baselines are regenerated only with human
confirmation recorded in the pull request.

---

## Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| L0 semantics drift during implementation | High | Parity harness built first; every difference attributed, none waived |
| Margin rules reproduced incorrectly (tier tables, cross-margin details) | High | Bitemporal rule storage; spot-checks against venue liquidation calculators; property tests on monotonicity |
| Throughput-mode conversion changes strategy behavior | High | Throughput mode independently re-passes the parity gate |
| PyO3 callback overhead dominating the Python tier | Medium | Downgraded, and the reason is external: this was written for a world with a global interpreter lock. PyO3 v0.28 supports free-threaded Python 3.14 and the GIL-release API, and a module holding no shared Python state can declare `gil_used = false` and run without re-enabling the lock. The two-mode design still isolates the risk and compatibility mode still promises no speed, but the question is now one to measure rather than one to design around. See D16 |
| Capture infrastructure undersized (volume, gaps, cost) | Medium-high | Dedicated capture host with local storage and batch archival; a 7-day trial run measures volume, gap rate, and cost before committing |
| Insufficient calibration data for L1 | Medium | Capture starts at M0; until enough data exists, only the conservative queue model is enabled |
| Type system splitting into Rust and Python dialects | Medium | Single type system rule; bindings expose the same types, never a parallel model. The sharper form of this risk is that whatever the bindings expose first becomes the API and freezes hardest, which argues for a small first surface rather than none — see D16 |
| The Rust core becomes an implementation detail of a Python library | Medium | The engine must stay buildable and testable with no interpreter present. Enforced rather than intended: per-crate dependency budgets keep twelve crates at zero, and the composability check builds each crate on its own |
| Scope inflation | High | Committed scope ends at M2; everything later is trigger-gated |
| Proprietary content leaking into the public repository | High | Fresh history; secret and pattern scanning in CI; proprietary material only in private overlays; pre-release manual audit |
| Bus factor of one | Medium-high | Agent-friendly codebase (per-crate `AGENTS.md`, layered verification anchors); behavior encoded in deterministic tests; all design rationale written down rather than remembered |
| Building the framework becomes the goal instead of using it | High | Every milestone states the capability it unlocks; the question "what got measurably better because of this?" is asked at each gate |
| The execution seam has one implementation and no second | Medium | Was: no seam at all. `Execution` now exists and `oq-gateway` implements it for one venue, so the layers above no longer name a venue — but the claim that a second one is an implementation rather than a rewrite is exactly as unproven as it was for market data before OKX landed. The way to close this is a second venue, not more design |
| The instrument model is split in two, and neither half is in the core | High | `oq-margin::Contract` holds the economics — `tick_cash` is the contract multiplier, so a 300x index future is already expressible — and `oq-l2feed::Instrument` holds the quoting precision. They never meet, and `oq-types` names an instrument only as `InstrumentId(u32)`, so nothing below the two hosts knows what a contract is. Harder than either half: `Cash` carries no currency dimension, so a book settling in more than one currency cannot be expressed at all, which is what equities and FX require. Unify identity, precision and economics in the core and settle the currency question before the Python surface freezes |
| `Strategy` is defined in the backtest host | Medium-high | Live execution would either depend on `oq-backtest` or need a second strategy trait, and G7 — the same strategy unchanged in both modes — cannot hold either way. Move the trait below both hosts before external code implements it |
| L0 is the frozen regression anchor and has no matching seam | Medium | `oq-engine` has one matching path and no trait, so L1 and L2 require refactoring the anchor they are measured against. Introduce the seam with L0 as its first and only implementation, so the anchor's behaviour is unchanged by construction |


The last four entries are not hypotheses. They are present-tense structural
gaps found by reading the code against this plan, and they share a shape: each
is cheap to fix now and grows more expensive with every crate, binding or
external implementation that depends on the current arrangement. They are
recorded here rather than in an issue because a risk register is where the
project already asks "what will this cost us later".

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
SLA before 2.0.
