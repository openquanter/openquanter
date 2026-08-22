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
  gradient-boosted trees, run in Rust with single-threaded intra/inter-op,
  warm-up, and fixed shapes. A parity gate compares Python and Rust
  predictions, because float32 threshold handling in tree models drifts
  between implementations in ways that silently change trading decisions.

  **Inference runs outside the kernel, and a prediction enters as a
  journaled event.** See D14 — the reason is not primarily determinism.
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

### D14 — Model predictions are events, not calls

Inference runs **outside** the kernel. A prediction is produced, submitted to
the sequencer, journaled, and only then delivered — like any other input.
Nothing in the kernel ever calls a model.

The determinism argument for this is real but secondary. The primary reason is
that a synchronous in-kernel call **would flatter every backtest**.

In live trading a prediction always lags the market data that produced it:
features have to be computed, the model has to run, the result has to travel.
A backtest that calls the model inline hands the strategy a prediction with
zero latency, from data the market has only just produced. That is the same
class of lie as an account that never gets liquidated — a simulation quietly
granting something the world does not offer. Making the prediction an event
means it arrives after the data it was derived from, in backtest exactly as in
production, and a strategy cannot accidentally be built on a timing advantage
that will not exist when it trades.

What follows from the decision:

- **Replay never re-runs a model.** It replays the recorded prediction, so a
  past run reproduces exactly even though the model that produced it may be
  non-deterministic across machines, may have been retrained, or — in the case
  of a remote language model — may be irreproducible in principle. The core
  keeps its guarantee by not depending on the world keeping its.
- **The kernel stays integer.** Floating-point results differ across CPUs in
  the last bits; keeping inference outside is what lets the kernel remain
  `i64` fixed point and therefore bit-identical across machines.
- **The journal records what the model actually said.** "What did the model
  predict at 14:32:07, and on what inputs" becomes answerable after the fact,
  which is exactly the question an incident review asks first.
- **A prediction event carries the model's identity and a hash of its input
  features**, following D13. That distinguishes "the model changed" from "the
  features changed" — without it, a differing prediction is unattributable.
- **The Python/Rust parity gate (G9) falls out for free.** Replay a journal
  through a second inference implementation and diff the prediction events
  with `oq-parity`; no separate harness is needed.
- **One code path.** In a backtest the inference component runs and emits
  prediction events into the journal; in production it does the same. The
  difference is the event producer, as everywhere else in this design.

The cost is one event round-trip on the hot path and a larger journal. Against
a 100 µs p99 budget the round-trip is affordable, and the journal growth is
capacity planning rather than a design problem. Paying it buys a backtest that
does not lie about when the strategy knew things.

---

### D15 — A venue is an adapter, and one of the things it decides is what a day is

The capture path stores payloads verbatim and parses only enough to place a
record in a window. Everything a venue does differently lives behind one trait:
which streams to subscribe, how a subscription is confirmed, how its payloads
are shaped, the quoting precision of each instrument, and which archive window
a record belongs to.

That last one is the reason this is a design decision rather than a refactor.
The archive's central invariant is that a file holds one whole period — the
merge tool, the order-book check and the tick converter all lean on it — and
dividing time by the UTC clock satisfies that invariant only for a market that
never closes. A US equities session is six and a half hours inside a UTC day,
mostly empty. A futures session opens the evening before and runs past
midnight, so the clock rule turns one trading day into two files and every
tool downstream inherits the split. The framework is not tied to an asset
class, so the assumption had to come out of the archive core and into the
venue, where a session actually belongs.

No exchange calendar ships with it. Writing one without a venue to check it
against would be guessing, and a wrong calendar is worse than an absent one
because it looks authoritative. What exists is the seam and a default that
reproduces the present behaviour exactly.

The same reasoning made subscription and payload parsing part of the contract,
each after a failure that argued for it. A venue that accepts any stream name
without validating it will confirm a retired stream and then say nothing,
which is indistinguishable from a quiet market — so the contract carries how a
subscription is acknowledged, and silence past a deadline is an error. A
payload reader written for one venue finds nothing in another and returns an
empty result rather than failing — so parsing belongs to the venue too, or a
perfectly readable archive converts to nothing and reports itself as empty.

### D16 — Python is a binding, not a compromise

The case against a Python tier was that it dilutes the zero-dependency
claim. Checked against what the repository actually enforces, it does not,
and the objection rests on a claim this project does not make.

`scripts/check-composability.sh` sets a dependency budget **per crate**:
twelve crates at zero, four at sixty. The README's claim is correspondingly
specific — the *engine* has no third-party dependencies, and every crate
that carries a tree is one that has to talk to something outside the
process. `oq-l2feed` carries a TLS stack because it speaks to a venue.
A binding crate would carry PyO3 because it speaks to Python. Neither
changes the twelve zeros, and CI proves it on every pull request rather
than asking anyone to believe it.

So the reason to be careful about Python is not dependency hygiene. It is
three other things, and they are worth naming because they are the ones
that actually cost:

**The Python surface becomes the API.** Whatever is exposed first is what
users write against, and it freezes hardest. This is an argument for
exposing a small surface early rather than a large one, not for exposing
none.

**Distribution stops being `cargo build`.** A binding ships as wheels, per
platform, per interpreter. As of 2026 that means an `abi3` wheel for the
minimum supported version, a version-specific `cp314t` wheel for
free-threaded 3.14, and an `abi3t` wheel once 3.15 lands — PEP 803 was
approved in 2026 and defines a stable ABI for free-threaded builds. This
is real work and it is build infrastructure, not architecture.

**A Rust-only path has to stay usable.** The engine must remain buildable
and testable without a Python interpreter anywhere near it, or the Rust
core quietly becomes an implementation detail of a Python library. The
per-crate budgets already enforce the dependency half of this; the
composability check's standalone-build pass enforces the rest.

The most comparable project resolves it the same way. NautilusTrader runs
a Rust core with PyO3 bindings and Python as the control plane, and keeps
a pure-Rust path that runs without Python at all.

**One risk this supersedes.** The register's entry about PyO3 callback
overhead was written for a world with a global interpreter lock. PyO3
v0.28 supports free-threaded Python 3.14 and the GIL-release API, and a
module whose pipeline holds no shared Python state can declare
`gil_used = false` and run on a free-threaded interpreter without
re-enabling the lock. The overhead question is now measurable rather than
structural, which is a different kind of risk and is recorded as one.

**Sequencing, and the reason for it.** The narrow proposal — expose the
statistics and the margin-deviation report, so a Python user can *evaluate
their existing backtest* rather than migrate to a new one — is kept, not
as an alternative to full bindings but as the first stage of them. Its
merit is not that it is safer; it is that it is the shortest path to an
outside user, and an outside user is one of M3's four entry conditions and
the only one that cannot be bought with engineering time. Full bindings
follow. Nothing about the small surface forecloses the large one.

### D17 — No MainEngine: assembly lives in types, not in a registry

Anyone arriving from the 1.x lineage looks first for a `MainEngine` — a central
object holding gateways, apps, the event engine, the database and the OMS, with
the parts reaching each other at runtime through `get_gateway(name)` and
`add_app(obj)`. There is none here, and the absence is a decision rather than an
omission.

**Assembly lives in `oq-live`**, whose job is stated differently: the gate
decides whether an order may be sent, the gateway knows how to send it, the
stream says what happened, and **none of them know about each other**; this
crate is where they meet, and the meeting is the point. `Session` is **the only
path that can send an order** — correct ordering is not something the caller
remembers, it is the absence of another route.

Four differences in shape. The first three are ordinary Rust practice:

| | MainEngine | Here |
|---|---|---|
| Composition | runtime registration, `add_gateway(obj)` | compile-time generics, `Session<E: Execution>` |
| Ownership | one object holds everything, `&mut` from all sides | components own their parts; messages move ownership |
| Communication | a shared event-bus object | channels; nobody owns "the engine" |
| The wiring diagram | implicit in registration order | it is the code in `main`, and every connection is readable |

The first two are not stylistic in Rust: a hub that every subsystem needs `&mut`
on pushes you into `Rc<RefCell<_>>` or `Arc<Mutex<_>>`, **trading compile-time
errors for runtime panics and deadlocks**.

**The fourth is the one that matters here.** A hub anything can reach into and
mutate is exactly where "who changed this state" stops having an answer — and
the kernel's entire premise is that state moves only by event
([D1](#d1--pure-deterministic-state-machine-core)). The moment a component
can call the hub and change state directly, that change is not journalled, the
replay disagrees, and the
[attribution](REQUIREMENTS.md#310-live-gap-attribution--fr-attrib) chain is
broken. **A MainEngine and "every cent accounted for" are structurally in
conflict.** That is not a preference.

**Dynamic dispatch is still used where it belongs**: which venue to connect to
is a configuration question answered at runtime, and `Box<dyn Account>` is the
right tool. **Which one is a runtime question; what the graph looks like is a
compile-time one** — the problem with a MainEngine was never that it used
dynamic dispatch, but that it made the whole graph mutable at runtime.

The trait is `Account` rather than `Execution` for a reason worth recording,
because the first attempt got it wrong. `Execution` says how to send an order,
and that is not enough to run: before the first order there is a clock to agree
on, an instrument whose precision and grid come from the deployment, a position
mode to discover, a balance and a book of resting orders to read, and a stream
to hear the answers on. Those were reached for on a concrete `Binance`, in nine
places inside the runner, so an adapter could implement `Execution` in full and
still be unusable — which is what happened: an OKX client existed, could not be
run, and **nothing said what it still owed**, because the missing part had no
name.

One of those nine is worth naming on its own. The runner built client order ids
to `IdRules::BINANCE`, a compiled-in constant — 36 characters with punctuation.
OKX allows 32, alphanumeric. An OKX adapter would therefore have been rejected
at submission, *after* the strategy had already decided to trade, by a rule the
runner had no business holding an opinion about.

`Account` is that list, and the list is now checkable: `oq-gateway` carries a
test venue implementing the trait against nothing but the trait. If a method
arrives whose only sensible implementation is Binance's, it stops compiling
there rather than six months later in somebody's half-finished adapter.

Market data is deliberately *not* folded in — it has its own seam,
`oq_l2feed::Venue`, and merging them would couple a capture tool to an account
credential. The two are tied by identity instead: the runner opens its feed with
`venue.id()`, so the account side and the market side cannot disagree about
which venue is being traded.

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
| Testing | `criterion`, `proptest`, and a purpose-built fault simulator — all adopted | Benchmarks; generated-input invariants over the matching and margin properties `FR-MATCH-7` and `FR-MARGIN-7` name, which found a wrapping conversion that gave a large enough position no maintenance requirement at all; and `oq-sim`'s corpus driving the live books, which found a redelivered fill doubling a position. `proptest` is a dev-dependency in both crates, so the per-crate budgets are unchanged |
| Venue adapters | Written per venue against a thin contract | Universal abstraction layers hide exactly the venue-specific semantics that cause incidents |

---

## 4. Crate map

| Crate | Responsibility | Milestone |
|---|---|---|
| `oq-types` | Domain types, `i64` fixed point, typestate order/position machines | M1 |
| `oq-hash` | SHA-256 and CRC-32, shared by the journal, capture and parity | M1 |
| `oq-examples` | Teaching examples and the seeded synthetic market they run on | M2 |
| `oq-journal` | mmap journal, snapshots, replay, torn-tail tolerance | M1 |
| `oq-core` | Sequencer, deterministic kernel, injected clock, sharding | M1 |
| `oq-engine` | Matching: L0 (frozen anchor), L1, L2 | M1 / M4 |
| `oq-margin` | Tiered maintenance margin, liquidation paths, liquidation orders, funding spikes | M1–M2 ★ |
| `oq-backtest` | Run scheduling, funding, accounting, exports, participation rate, fidelity report | M1 |
| `oq-parity` | Trade-by-trade diff and difference attribution; baselines identified by the (commit, data hash, config hash) triple (D13) | M1 (built first) |
| `oq-data` | Dual-timestamp Arrow layer, bitemporal reference data, strict as-of joins | M1–M2 |
| `oq-l2feed` | Capture toolkit: incremental depth, BBO, trades, mark price, liquidations, rule tables | M0 |
| `oq-book` | Order book rebuilt from incremental depth, shared by capture and matching | M0 / M4 |
| `oq-ingest` | Captured archives folded into the tick format the engine replays | M0 |
| `oq-strategy` | Tier A traits, indicator components | M2 |
| `oq-py` | Tier B: compatibility mode and throughput mode | M2 |
| `oq-stats` | DSR, PBO/CSCV, trial registry | M0 |
| `oq-cli` | `backtest` / `sweep` / `live` / `replay` / `parity` / `data` | M2 |
| `oq-sim` | Randomized whole-system fault simulation and scenario corpus | M1 onward |
| `oq-risk` | RiskGate: limits, kill switch, reconciliation | M3 |
| `oq-gateway` | Venue adapters, **execution conformance suite** (`conformance::check` drives an adapter through the placement contract using payloads it supplies), reconciliation protocol, order-ID attribution | M3 ★ |
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
    oq-journal/
      src/*.rs          framing, writer, reader, snapshot store
    oq-engine/
      src/
        lib.rs
        book.rs         price-time priority order book
        l0.rs           tick-replay matching
        l1.rs, l2.rs    the rest of the ladder, each wrapping the one below
    oq-book/
      src/              depth application and the book both halves share
    oq-l2feed/
      src/
        *.rs            framing, sealing, depth parsing, reconstruction
        bin/            oq-capture, oq-book-check, oq-merge, oq-resequence
    oq-ingest/
      src/
        agg.rs          windowed aggregation from archive to tick
        bin/            oq-ingest, oq-tiers
    oq-examples/
      examples/         runnable teaching strategies
      benches/          criterion benchmarks
      tests/golden.rs   pins every number the documentation quotes
    oq-gateway/
      src/
        exec.rs         the connector contract every venue implements
        binance.rs, okx.rs   one adapter each
        conformance.rs  the suite both adapters answer to
    ...
  docs/                 requirements, roadmap, formats, quickstart
  scripts/              repository tooling: DCO check, secret scan,
                        composability check, crate-name reservation
  .github/workflows/    CI: test, hygiene, composability, throughput floor
```

Two things are deliberately *not* in this tree. There is no top-level
`examples/` or `data/`: runnable examples are a crate, so they compile
under `cargo test` and cannot rot, and the sample market is
**generated from a seed** rather than stored, which is why golden tests
can pin exact numbers without shipping a dataset or inheriting anyone's
redistribution terms.

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
| Required | Raw trades | What consumed the queue ahead of you: the other half of the queue model. Raw rather than aggregated — individual fills, not fills pre-grouped by price and time |
| Required | Mark price / funding rate / index price | Margin engine input; liquidation uses mark price. Captured by **polling** where no stream carries it — see [Capture Format](CAPTURE-FORMAT.md) §10 |
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
| P1.9 | Benchmarks in CI | **Landed.** `criterion` benches in `crates/oq-examples/benches/`, plus a CI job asserting a throughput floor. A floor rather than a tracked baseline, deliberately — see the [roadmap](ROADMAP.md#milestone-overview) |

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
| P2.7 | Adoption readiness | Quickstart, two example strategies, sample dataset with goldens. The timed external cold start that used to close this row went with `G11` |

**Gate:** G3, G4, G5, G7; beta release. (`G11` was withdrawn.)

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
| **Property** | Are the invariants preserved under arbitrary inputs? | Quantity conservation, price-time priority, no crossed book, non-negative margin, monotonic liquidation price — **all checked by `proptest` over generated inputs**. The first run found two things: a notional that wrapped, giving a large enough position no maintenance requirement at all; and a property written **too strongly** — a position posted with less margin than its own requirement genuinely does have a liquidation price above its entry, because it is on the wrong side of the line the instant it opens. That one was the assertion being wrong and the code being right; the property now carries the qualifier and the case it carved out is asserted as a property of its own |
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
- Benchmarks run in CI against a **floor**, and falling below it is a build
  failure. The floor is set far below any real machine on purpose: shared
  runners vary by several times from hour to hour, and a gate that fails on
  noise gets disabled. Precise version-to-version comparison is `cargo bench`
  on one machine, not a CI job.

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
   before 2.0. Partly answered since this was written: OKX is the second, on
   both the capture and the order side, and each side has a conformance suite
   both adapters pass. It found what a second venue is for — Binance answers a
   refusal with an HTTP status, OKX answers one inside a 200 — which is the
   kind of disagreement no amount of design settles. What is still open is the
   third venue's priority, and OKX's signed half, which needs a live account.
4. **Queue model selection policy.** How the framework should choose between
   conservative and probabilistic queue models when calibration data is thin —
   currently a manual setting, arguably should be automatic with a warning.
5. **Python packaging surface.** How much of the Rust API to expose in the
   Python bindings. Exposing everything invites coupling to internals; exposing
   too little forces users into Rust prematurely.
