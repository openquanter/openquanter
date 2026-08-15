# OpenQuanter — Implementation Plan

> Status: **Draft for review** · [中文版](IMPLEMENTATION.zh-CN.md)
> Companion documents: [Requirements](REQUIREMENTS.md) · [Roadmap](ROADMAP.md)

This document describes the architecture, the design decisions behind it, the
crate map, and the concrete engineering plan for delivering the requirements.
Where a decision has a well-known precedent — in open-source systems, in
exchange infrastructure, or in the literature — the precedent is named so that
future contributors can evaluate the reasoning rather than inherit it.

---

## 1. Architecture

```
             ┌── Input adapters ───────────────────────────────────────┐
             │  live: venue gateway │ backtest: data plane │ sim: gen  │
             └───────────────────────────┬─────────────────────────────┘
                                         ▼
┌── Sequencer + Journal (oq-journal) ─────────────────────────────────────────┐
│  Every event is numbered and appended to an mmap journal before it enters   │
│  the core. The journal is simultaneously: audit trail, replay source,       │
│  recovery mechanism, and IPC transport. Readers tolerate a torn tail.       │
└───────────────────────────┬─────────────────────────────────────────────────┘
                            ▼
┌── Deterministic core (oq-core) — sharded by instrument, single-threaded ────┐
│    fn apply(&mut State, Event) -> Outputs                                   │
│    Forbidden inside: clock reads, RNG, I/O, threads. Time is an event.      │
│  ┌─────────────┐ ┌───────────┐ ┌───────────┐ ┌────────────────┐             │
│  │ Matching    │ │ Margin    │ │ RiskGate  │ │ Ledger         │             │
│  │ oq-engine   │ │ oq-margin │ │ oq-risk   │ │ oq-types       │             │
│  │ L0…L3       │ │ liquidation│ │ hard caps │ │ typestate      │            │
│  └─────────────┘ └───────────┘ └───────────┘ └────────────────┘             │
└──────┬──────────────────────┬─────────────────────────┬─────────────────────┘
       ▼                      ▼                         ▼
┌─ Strategy layer ───┐ ┌─ AI layer ─────────────┐ ┌─ Output adapters ─────────┐
│ Tier A: Rust trait │ │ oq-infer  (production) │ │ live:     oq-gateway      │
│ Tier B: Python     │ │ oq-env    (RL, gated)  │ │ backtest: fill simulation │
│  compat/throughput │ │ oq-lab    (sandbox)    │ │ observers read the journal│
└────────────────────┘ └────────────────────────┘ └───────────────────────────┘
┌─ Data plane (oq-data) ──────────────────────────────────────────────────────┐
│ Arrow/Parquet, dual timestamps │ bitemporal reference data │ strict as-of    │
│ oq-l2feed capture toolkit      │ oq-features point-in-time feature layer     │
└─────────────────────────────────────────────────────────────────────────────┘
```

Three properties fall out of this shape and are worth stating explicitly:

- **Backtest and live differ only in the event producer.** There is no separate
  backtest engine to keep in sync with a live engine.
- **The journal is the integration point.** Monitoring, analytics, and
  cross-process aggregation all read the journal. Nothing writes back into the
  core except through the sequencer.
- **Determinism is not a testing convenience, it is the recovery mechanism.**
  The same property that lets a fuzzer reproduce a failure lets a crashed
  process rebuild its state.

---

## 2. Design decisions

### D1 — Pure deterministic state machine core

The core is `apply(State, Event) -> Outputs` with no ambient authority. Time
arrives as `TimeEvent`. This is the LMAX/Aeron lineage, and modern trading
frameworks have converged on it independently.

*Consequence:* every "just read the clock here" or "just log to disk here"
shortcut is a design violation, not a style preference. Lints and review
enforce it, and replay tests catch what they miss.

### D2 — Sequencer and journal ahead of the core

Events are numbered and durably appended before the core observes them. This
mirrors the design of tick-capture architectures used in exchange and market
data infrastructure, and of memory-mapped queue libraries in the same lineage.

*Consequence:* snapshots are triggered by in-stream events (so a snapshot is at
a well-defined sequence number), readers must tolerate a torn final record, and
"what actually happened" is never a question of log-level configuration.

### D3 — Sharded single-threaded execution with a ring buffer

Per-instrument shards, single-threaded within a shard, ring-buffer handoff,
preallocated buffers, minimal branching in the hot path. Async is confined to
the gateway edge. Work-stealing schedulers are excluded from the hot path
because their tail latency is unpredictable, which is exactly what a p99 target
cannot tolerate.

### D4 — Fidelity ladder plus orthogonal margin overlay

Execution realism (L0–L3) and account realism (margin overlay) are independent
axes. L0 semantics are frozen after release and act as the regression anchor.

`oq-margin` is a separate module, not a field on the account object: tiered
maintenance-margin tables versioned bitemporally (exchange rules change and old
backtests must use old rules), per-tick usage and liquidation price,
liquidation orders that go through the matching engine like any other order,
and injectable funding-spike scenarios.

Both matching and margin are pure functions covered by property tests:
quantity conservation, price-time priority, no crossed book, non-negative
margin usage, monotonic liquidation price. Formal-methods practice in exchange
matching engines is the precedent for treating these as machine-checked
invariants rather than review comments.

### D5 — Dual timestamps and fixed-layout events

`exch_ts` and `local_ts`, nanoseconds, on every market data record. Events use
`repr(C)` zero-copy layouts. **Fields are never repurposed** — a field that
changes meaning across versions is the failure mode behind some of the most
expensive incidents in electronic trading history.

### D6 — One core, three environments

Backtest, sandbox, and live differ by adapter and clock only. This is what
makes "the strategy I tested is the strategy I deployed" a structural property
rather than a discipline.

### D7 — Unbypassable risk gate; unknown state is fatal

A pipeline-level hard check between strategy output and any venue, plus a kill
switch and startup reconciliation. If reconciliation cannot establish ground
truth, the process refuses to trade. Regulatory pre-trade risk-control
requirements in equities markets encode the same principle, and the industry's
canonical catastrophic-loss incidents share the same root: continuing to act
while state was unknown.

### D8 — Determinism dividend: whole-system simulation

Because the core is deterministic, a randomized fault simulator can explore
scheduling, network, and venue misbehavior and reproduce any failure from
`(seed, commit)`. Fault-injection simulators in distributed databases are the
model here.

The scenario corpus is seeded with **generic** failure patterns: out-of-order
and duplicated execution reports, lost cancels, synthetic zero-price fills,
stale user streams, reconnect storms, clock jumps, feed gaps. Anyone operating
a trading system accumulates a list like this; encoding it as executable
scenarios is what turns operational scar tissue into a durable test asset.

### D9 — AI layered by evidence strength

- **Inference (mature).** Train in Python, export to ONNX or compile
  gradient-boosted trees, run in-process in Rust with single-threaded
  intra/inter-op, warm-up, and fixed shapes. A parity gate compares Python and
  Rust predictions, because float32 threshold handling in tree models drifts
  between implementations in ways that silently change trading decisions.
- **Training (gated).** Gym-style environments with inverted control and native
  vectorized batching. Gated on L1 fidelity: training in a low-fidelity
  environment produces policies that exploit the simulator.
- **Exploration (time-boxed).** LLM sandbox with a typed read-only tool API and
  audit logging. Evaluated only on post-training-cutoff data. Model output is a
  proposal; hard limits stay in the engine.

### D10 — Point-in-time correctness in the data layer

Bitemporal storage for anything that changes retroactively (funding schedules,
margin tiers, contract specs, listings). As-of joins default to **strict `<`**
because inclusive-boundary semantics in common dataframe libraries leak
same-timestamp information into features. One feature code path serves research
and live, with an online/offline consistency metric proving it.

### D11 — A codebase designed for coding agents

Per-crate `AGENTS.md` under 200 lines stating local commands and invariants.
Layered verification anchors — unit → property → golden → parity — so an agent
or a new contributor can tell whether a change is safe without holding the
whole system in their head. Golden baseline changes require explicit human
confirmation in the pull request.

This is also the bus-factor mitigation: projects in this space die when their
single maintainer leaves, and the defense is written-down, machine-checkable
behavior rather than documentation that describes intent.

### D12 — Runtime topology and security

- **One process per account.** A process owns one account, its strategies, and
  its journal. Fault domains do not overlap; a reconciliation failure in one
  account cannot corrupt another. Portfolio-level views are built by reading
  journals, not by sharing trading state.
- **Credential handling.** API keys never appear in committed configuration.
  They are injected from environment or OS keyring and exist only in gateway
  process memory; research and backtest processes have no access path.
- **Clock discipline.** Hosts run NTP/chrony. Capture processes record a
  clock-offset estimate at startup and archive it with the data — latency
  modeling built on an unverified local clock is built on sand.

### D13 — Baselines are identified by content, not by commit

A regression baseline pinned to a code commit alone expires silently. Input
data gets corrected, a configuration default moves, a dataset is re-exported —
the code is untouched, so the baseline still looks valid, and the next
comparison reports a mismatch that is not a regression at all.

This failure mode is worse than a missed regression: it produces a *confident*
wrong answer, and the person reading it has no way to tell "the engine
changed" from "the inputs changed". Time is then spent bisecting code that was
never at fault.

Therefore every parity and golden baseline in this project is identified by the
triple **(code commit, content hash of the input data, hash of the effective
configuration)**, and the comparison tool classifies its own output:

| Triple | Report |
|---|---|
| All three match | Differences are **behavioral** — a regression to investigate |
| Any element differs | **`baseline invalidated — rebase required`**, naming the element that changed |

Hashing the inputs costs microseconds and removes an entire class of
misdiagnosis. The rule extends to golden tests: a golden baseline whose sample
data changed is stale, not violated.

---

## 3. Technology choices

| Area | Choice | Rationale |
|---|---|---|
| Language | Rust stable; no async on the hot path | Predictable latency, no GC pauses, memory safety without a runtime |
| Event transport | Ring buffer (disruptor pattern) | Bounded, preallocated, lock-free handoff |
| Journal | Purpose-built mmap append log with zero-copy serialization | Existing queue libraries do not match the dual-timestamp, torn-tail, replay requirements exactly |
| Python bindings | PyO3 + maturin, abi3 wheels | One type system exposed to both tiers; wheels that work across CPython versions |
| Columnar data | `arrow-rs` + `parquet-rs` | Zero-copy interchange with the Python analysis ecosystem |
| Numerics | `i64` fixed point on the hot path; decimal at ledger boundaries | Exact money arithmetic without float error; float never touches money |
| Inference | ONNX runtime and compiled decision trees | Chosen per model class by measured latency, not by framework preference |
| Testing | `proptest`, `criterion`, purpose-built fault simulator | Invariants, benchmarks, and whole-system fuzzing respectively |
| Venue adapters | Written per venue against a thin contract | Universal abstraction layers hide exactly the venue-specific semantics that cause incidents |

---

## 4. Crate map

| Crate | Responsibility | Milestone |
|---|---|---|
| `oq-types` | Domain types, `i64` fixed point, typestate order/position machines | M1 |
| `oq-journal` | mmap journal, snapshots, replay, torn-tail tolerance | M1 |
| `oq-core` | Sequencer, deterministic kernel, injected clock, sharding | M1 |
| `oq-engine` | Matching: L0 (frozen anchor), L1, L2 | M1 / M4 |
| `oq-margin` | Tiered maintenance margin, liquidation paths, liquidation orders, funding spikes | M1–M2 ★ |
| `oq-backtest` | Run scheduling, funding, accounting, exports, participation rate, fidelity report | M1 |
| `oq-parity` | Trade-by-trade diff and difference attribution; baselines identified by the (commit, data hash, config hash) triple (D13) | M1 (built first) |
| `oq-data` | Dual-timestamp Arrow layer, bitemporal reference data, strict as-of joins | M1–M2 |
| `oq-l2feed` | Capture toolkit: incremental depth, BBO, trades, mark price, liquidations, rule tables | M0 |
| `oq-strategy` | Tier A traits, indicator components | M2 |
| `oq-py` | Tier B: compatibility mode and throughput mode | M2 |
| `oq-stats` | DSR, PBO/CSCV, trial registry | M0 |
| `oq-cli` | `backtest` / `sweep` / `live` / `replay` / `parity` / `data` | M2 |
| `oq-sim` | Randomized whole-system fault simulation and scenario corpus | M1 onward |
| `oq-risk` | RiskGate: limits, kill switch, reconciliation | M3 |
| `oq-gateway` | Venue adapters, conformance suite, reconciliation protocol, order-ID attribution | M3 ★ |
| `oq-live` | Process assembly, snapshot recovery, graceful restart | M3 |
| `oq-features` | Point-in-time feature layer, online/offline consistency metrics | M2 skeleton / M5 |
| `oq-infer` | ONNX and compiled-tree inference, prediction parity gate | M5 |
| `oq-env` | Gym-style vectorized environments | M5 |
| `oq-lab` | Experimental LLM sandbox with typed read-only tools | Time-boxed |

★ marks the two crates that carry disproportionate effort: `oq-margin` because
it is the differentiator, and `oq-gateway` because venue integration and
reconciliation are where trading systems actually break.

---

## 4.1 Repository layout

The architecture lives in the **crate split**, not in the directory tree.
Cargo forbids circular dependencies between crates, so the dependency
graph above is enforced by the compiler rather than by convention: the
deterministic core cannot reach a venue adapter because it does not
depend on the crate that contains one. That is a stronger boundary than
any directory arrangement provides, and it is why "no I/O in the core"
is an architectural property here instead of a rule people have to
remember.

Within a crate, modules are flat files until a module genuinely needs
submodules. Nesting a 150-line module inside its own directory adds a
path segment and no information.

```
openquanter/
  Cargo.toml            workspace manifest, shared lints and metadata
  crates/
    oq-stats/
      AGENTS.md         local commands and invariants (nearest file wins)
      Cargo.toml
      src/*.rs          flat while modules stay small
    oq-engine/
      src/
        lib.rs
        matching/       l0.rs, l1.rs, l2.rs — the fidelity ladder
        book/           order book reconstruction
    oq-gateway/
      src/
        contract.rs     the connector contract every venue implements
        binance/        market_data.rs, orders.rs, user_stream.rs, reconcile.rs
    ...
  docs/                 requirements, roadmap, implementation plan
  scripts/              repository tooling (DCO check, name reservation)
  examples/             example strategies
  data/                 sample datasets and golden baselines
  crates/*/tests/       integration tests, separate from in-module unit tests
  crates/*/benches/     criterion benchmarks, tracked in CI
```

Unit tests live beside the code they test, in a `#[cfg(test)]` module in
the same file — the Rust convention, and it keeps an invariant next to
the function that has to satisfy it. Integration tests, which exercise a
crate through its public API only, live in `tests/`.

## 5. Execution plan

Task IDs are stable references for issues and pull requests.

### Phase 0 — Foundations (M0)

| ID | Task | Done when |
|---|---|---|
| P0.1 | Repository hygiene: DCO check action, CLA text, crates.io name reservations, Rust edition upgrade | CI enforces sign-off; `oq-*` names reserved |
| P0.2 | `oq-stats`: DSR, PBO/CSCV, trial counting | Statistics computable on real sweep output, unit-tested against published examples |
| P0.3 | `oq-l2feed` design and trial capture | 7-day trial: measured volume, gap rate, storage cost, clock-offset archival verified |
| P0.4 | `oq-l2feed` continuous operation | Capture running as a service with monitoring; daily margin-tier table snapshots accumulating |
| P0.5 | Scenario catalogue | ≥ 6 generic failure scenarios written as reproducible descriptions |
| P0.6 | Latency and fill-outcome sampling methodology | Documented offline procedure for extracting latency and fill samples from operational logs |

**Capture specification** (the part of M0 that cannot be deferred, because
uncaptured days are permanently lost):

| Priority | Stream | Purpose |
|---|---|---|
| Required | Incremental depth updates + REST snapshot on connect + end-of-day snapshot | L2 book reconstruction; queue model input |
| Required | Best bid/offer stream | True tick-level BBO — depth streams are coalesced and downsampled |
| Required | Aggregated trades | Volume consuming the queue ahead of you: the other half of the queue model |
| Required | Mark price / funding rate / index price | Margin engine input; liquidation uses mark price |
| Required | Forced liquidation stream | Scarce tail-behavior data, valuable even when rate-limited |
| Recommended | Leverage and maintenance-margin tier tables (daily snapshot, bitemporal) | Margin rule source; rules change silently; storage cost is negligible |
| Recommended | Open interest, long/short ratios (periodic REST) | Cheap research inputs |

Format rules: dual timestamps on every message; raw messages archived verbatim
with aggregation left to consumers; a gap marker and fresh snapshot on every
reconnect; clock-offset estimate archived alongside. Capture runs on a
dedicated host with local storage and compressed batch archival — this workload
does not belong on a shared or bandwidth-constrained link.

### Phase 1 — Deterministic core (M1)

| ID | Task | Done when |
|---|---|---|
| P1.1 | `oq-parity` harness | Trade-by-trade diff with attribution, runnable against two arbitrary run outputs; baselines carry the (commit, data hash, config hash) triple and a stale baseline reports `baseline invalidated` rather than a mismatch |
| P1.2 | `oq-types` | Fixed-point arithmetic property-tested; illegal order state transitions unrepresentable |
| P1.3 | `oq-journal` | Append, snapshot, replay, torn-tail recovery all tested including crash injection |
| P1.4 | `oq-core` | Sequencer plus kernel; determinism test replays a journal to identical output |
| P1.5 | `oq-engine` L0 | Semantics specified in prose and tests; property invariants green |
| P1.6 | `oq-margin` skeleton | Tier tables bitemporal; per-tick usage and liquidation price; liquidation order path |
| P1.7 | `oq-backtest` | End-to-end run on sample data with fidelity report |
| P1.8 | `oq-sim` prototype | First three catalogue scenarios reproduce from seed |
| P1.9 | Benchmarks in CI | `criterion` baselines tracked; regressions fail the build |

**Gate:** G1 and G2 met; first public preview tag.

### Phase 2 — Python tier and research workflow (M2)

| ID | Task | Done when |
|---|---|---|
| P2.1 | `oq-py` compatibility mode | An example strategy runs unmodified end-to-end |
| P2.2 | `oq-py` throughput mode | Subscription granularity, batched callbacks, mirrored state; meets G3 and re-passes parity |
| P2.3 | Margin fidelity and tail-divergence reporting | Historical stress windows recomputed with and without margin; methodology documented (G5) |
| P2.4 | `oq-data` | Dual-timestamp layer, bitemporal store, strict as-of joins, leakage tests |
| P2.5 | `oq-features` skeleton | One feature definition, two execution paths, consistency metric emitted |
| P2.6 | `oq-cli` + `oq-stats` integration | Sweeps emit DSR/PBO by default; strict mode refuses over-threshold results |
| P2.7 | Adoption readiness | Quickstart, two example strategies, sample dataset with goldens, external cold start ≤ 30 min (G11) |

**Gate:** G3, G4, G5, G7, G11 initial; beta release.

### Phase 3 — Live trading (M3, trigger-gated)

| ID | Task | Done when |
|---|---|---|
| P3.1 | `oq-gateway` reference adapter | Market data, order entry, user stream; reconciliation protocol handles lost/duplicate/out-of-order reports |
| P3.2 | Connector conformance suite | Replay and synthetic misbehavior tests any adapter must pass |
| P3.3 | Additional venue adapters | Second and third venues pass the conformance suite |
| P3.4 | `oq-risk` RiskGate | Pre-trade checks, kill switch, startup reconciliation with fatal-on-unknown |
| P3.5 | `oq-live` | Assembly, snapshot recovery, graceful restart, one process per account |
| P3.6 | Observability | Latency histograms at the defined boundary; parsing and signing budgeted separately (G6) |
| P3.7 | `oq-sim` full | Whole catalogue plus gateway fuzzing green |
| P3.8 | Cutover playbook | Position-carrying cutover rehearsed on testnet ≥ 2 times |
| P3.9 | Staged rollout | Shadow run, then small-size live, then staged expansion; each step separately authorized |

**Gate:** G6; full scenario suite green; rollout stable ≥ 2 weeks.

### Phase 4 — HFT fidelity (M4, trigger-gated)

| ID | Task | Done when |
|---|---|---|
| P4.1 | L1 queue model | Conservative model first; probabilistic model enabled only after calibration against recorded fills |
| P4.2 | L1 latency and impact | Three-segment latency distributions replayed from recordings; square-root impact penalty; participation-rate alerts |
| P4.3 | L2 book reconstruction | Book rebuilt from incremental depth with snapshot reconciliation and gap handling |
| P4.4 | Stylized-facts test set | Simulator reproduces fat tails, volatility clustering, order flow autocorrelation |
| P4.5 | Cross-tier comparison | Same strategy reported across L0 / L0+margin / L1 |

**Gate:** G8 plus a calibration report on fill-probability accuracy.

### Phase 5 — AI extensions (M5, independently triggered)

| ID | Task | Done when |
|---|---|---|
| P5.1 | `oq-infer` | In-process inference meeting the latency budget; Python/Rust parity gate green (G9) |
| P5.2 | `oq-features` complete | Production point-in-time layer with drift monitoring |
| P5.3 | `oq-env` | Vectorized environments; identical seeds reproduce identical training runs (G10) |
| P5.4 | `oq-lab` | Sandboxed LLM tooling with audit log; explicitly experimental |

---

## 6. Testing strategy

Four layers, each answering a different question.

| Layer | Question | Mechanism |
|---|---|---|
| **Unit** | Does this function do what it says? | Standard `cargo test`, fast, per-crate |
| **Property** | Are the invariants preserved under arbitrary inputs? | `proptest`: quantity conservation, price-time priority, no crossed book, non-negative margin, monotonic liquidation price |
| **Golden** | Did observable behavior change? | Sample data replayed, full output compared; baselines carry the identity triple (D13) and regenerate only with recorded human confirmation |
| **Parity** | Do two implementations or two modes agree? | `oq-parity` trade-by-trade diff with attribution; used for ports, refactors, throughput mode, and Python/Rust inference. A stale baseline is reported as stale, never as a difference |
| **Simulation** | Does the whole system survive hostile conditions? | `oq-sim` randomized fault injection, every failure reproducible from `(seed, commit)` |

Rules that are not negotiable:

- An invariant is never weakened to make a test pass. If the invariant is
  wrong, that is a design change with its own review.
- Golden baseline updates require an explicit human confirmation recorded in
  the pull request. An agent or a script may propose one; it may not approve it.
- A comparison never reports a difference it cannot attribute. If the inputs
  or configuration moved, the output says so (D13) instead of blaming the code.
- Public CI runs entirely on sample data. No quality gate may depend on a
  private dataset.
- Benchmarks run in CI with tracked baselines; a performance regression is a
  build failure, not a note.

---

## 7. Performance budgets

| Path | Budget | Measurement boundary |
|---|---|---|
| Live order emission | p99 ≤ 100 µs | Journal write → order bytes leaving the gateway socket |
| Market data parsing | Separately budgeted and reported | Socket read → normalized event |
| Request signing | Separately budgeted and reported | Signing entry → exit |
| Core `apply()` | Allocation-free after warm-up | Per-event, per-shard |
| GBDT inference | ≤ 10 µs single row | Feature vector in → prediction out |
| Backtest throughput | ≥ 8× the interpreted/Cython baseline | Wall clock, same machine, same configuration |
| Sweep | 100 configurations ≤ 30 minutes | Wall clock on a reference machine |

Latency is always reported as a histogram. Averages hide exactly the tail that
matters, and a mean latency figure for a trading path is close to meaningless.

---

## 8. Development workflow

- **Branching:** short-lived branches into `main`; `main` is always green.
- **Commits:** DCO sign-off required (`git commit -s`); English commit messages
  in `type: short description` form.
- **Review:** every change states which verification layer covers it. Changes
  to the core additionally state how determinism is preserved.
- **CI:** build, test, `fmt --check`, `clippy -D warnings`, property tests,
  goldens, benchmarks, and secret scanning.
- **Code comments:** English throughout. Documentation is authored in English
  and mirrored in Chinese.
- **Per-crate `AGENTS.md`:** local commands and invariants, kept under 200
  lines, updated in the same pull request that changes the invariant.

---

## 9. Open questions

These are recorded rather than resolved. Contributions to the discussion are
welcome as issues.

1. **Cross-margin and portfolio-margin scope.** Isolated and cross margin are
   in scope for M1–M2. Portfolio margin varies enough between venues that
   modeling it generically may not be worthwhile; a venue-specific
   implementation behind a trait may be the better answer.
2. **Reference-data distribution.** Margin tier tables and contract specs are
   small, change silently, and everyone needs them. Whether to distribute a
   community-maintained bitemporal dataset, and under what terms, is an open
   question.
3. **Second and third venue priority.** Driven by user demand; the connector
   contract should be validated by at least two structurally different venues
   before 1.0.
4. **Queue model selection policy.** How the framework should choose between
   conservative and probabilistic queue models when calibration data is thin —
   currently a manual setting, arguably should be automatic with a warning.
5. **Python packaging surface.** How much of the Rust API to expose in the
   Python bindings. Exposing everything invites coupling to internals; exposing
   too little forces users into Rust prematurely.
