# OpenQuanter — Roadmap

> Status: **Draft for review** · [中文版](ROADMAP.zh-CN.md)
> Companion documents: [Why this exists](WHY.md) · [Requirements](REQUIREMENTS.md) · [Implementation Plan](IMPLEMENTATION.md)
> Acceptance review: 2026-10-11. Gate definitions follow [Requirements §5](REQUIREMENTS.md#5-measurable-acceptance-goals).

**What every milestone is ultimately for:** every cent between a backtest and
the live run, accounted for. [Why that is the goal](WHY.md), and what the
predecessor hit that made it the goal, is a separate document; this one is
about the order of the work.

This roadmap describes the path from where the project is today to API
stability, and the research directions beyond it. It is organized by
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
| **M2** | Python tier, margin fidelity reporting, research workflow → beta | 24–41 pw | Acceptance open: G3 needs the workload defined in Requirements; G7's batching trade-off is measured, not waived; G11 was withdrawn |
| **M3** | Live trading: gateways, risk gate, reconciliation | 39–62 pw | Historical review completed; G12 not passed. Resolve the specific accounting/evidence defects; cutover, G6 and adoption gates remain separate |
| **M4** | HFT fidelity: L1 queue/latency, L2 book reconstruction | 54–83 pw | Calibration and input-validity work next; audit available records before asking for new samples, and distinguish testnet execution validation from production-market calibration |
| **M5** | AI extensions: inference, RL environments, feature layer | 62–97 pw | Triggered |
| **API stability** | API stabilization and semantic versioning | — | After M3 + external adoption |

"Largely landed" does not mean the exit gate has passed. The built/not-built
split and available operating evidence live in the [README Status section](../README.md#status).
This roadmap tracks what must be done with that evidence. M0 needs measured
gap rates and storage growth, not an uptime figure. M1's G2 is run-equivalence
tooling, not a promise of equality with an unrelated engine; its definition
has changed in Requirements and applies here too.

One M1 item was deliberately built smaller than planned. The benchmark job
asserts a **floor**, not a tracked baseline: shared CI runners vary by
several times from hour to hour, so a tight comparison would fail on noise
and be switched off within a week. The floor catches the engine becoming an
order of magnitude slower, which is the regression that actually happens.
Comparing two versions precisely is a local `cargo bench` job on one
machine. This is a change to the plan, recorded rather than quietly
dropped.

**M3 implementation preceded its original entry conditions; this review
does not retroactively waive them.** Its next validation step is to audit
existing records, not to restart a generic long-run waiting period. A
point-in-time account match does not establish historical agreement, and
elapsed runtime does not establish complete attribution. Cutover evidence,
release age, external adoption and G6's measurement boundary remain separate
requirements.

**G6 is partly measured, and what is missing is the end of the interval
rather than the beginning.** It asks for p99 latency from journal write
to socket write. The near boundary exists — the live path journals in
`record.rs`, and the kernel is on that path, keeping the account in
`books.rs` — and `oq-live` reports the segment it can see, journal flush
to client call, in every run's summary. The far boundary does not: the
HTTP client does not expose when the bytes reached the socket, so the
number printed is a lower bound and is labelled as not being G6's. What
closes the gate is a client that reports its own write, or a measurement
taken outside the process.

"Committed" means it is the current default plan. "Triggered" means the
milestone has an entry condition stated below and will not be started before
that condition is met.


## Scope, item by item

Broken down from theme to feature. **"Built" means the thing exists in the
repository with tests, not that the milestone's exit gate has passed** — the
gates are a separate matter and live in each milestone's own section.
**"Blocked", "pending audit" and "not built" are different facts**: the first
names an unavailable input, the second means records exist but verification
has not been completed, and the third means code has not been written. A
failed audit names the defect or uncovered interval; it does not become
"waiting for a longer run" without a specific sample requirement.

### M0 — Foundations

| Theme | Item | Status |
|---|---|---|
| Repository | Workspace, Apache-2.0, DCO, public CI | Built |
| Repository | crates.io name reservations | Built and complete: 25 names held, nothing published beyond the 0.0.1 placeholders. Every crate in the tree that can be published has its name, plus `oq-infer` and `oq-lab`, which do not exist yet. `oq-examples` is `publish = false` and needs none. The reservation script had drifted from what was actually reserved — `oq-hash` and `oq-ingest` were held but absent from its list, and `oq-book` was in neither — which is the failure mode a list maintained by hand has: it is only checked when someone publishes, and by then a name can be gone |
| Capture | Incremental depth, BBO, trades, mark price, liquidations | Built, running continuously |
| Capture | Dual timestamps, gap markers, recorded clock offset | Built |
| Capture | Archival and verification (`oq-merge`, `oq-resequence`) | Built |
| Statistics | DSR, PBO/CSCV, trial registry | Built |
| Simulation | Scenario catalogue (7 entries, with reproductions) | Built |
| Release | GitHub Releases and PyPI from a tag | `release.yml`: a `vX.Y.Z` tag on `main` builds Linux binaries and four-platform wheels, publishes the release and the wheels; `2.0.0a1` was the hand-uploaded predecessor |

### M1 — Deterministic core

| Theme | Item | Status |
|---|---|---|
| Types | `oq-types` fixed point, typestate order and position machines | Built |
| Journal | mmap append log, snapshots, replay, torn-tail tolerance | Built — the snapshot store is the primitive; nothing outside `oq-journal` writes one yet, because the kernel does not serialise its state |
| Core | Sequencer, deterministic kernel, injected clock | Built |
| Core | **Sharding by instrument** | Built — `oq_core::Shards`. The requirement reads as a performance decision and is also an accounting one: a balance two instruments draw on **is** shared mutable state, so "shares nothing" and "one balance behind several positions" cannot both hold. Venues have both arrangements and so does this: cross margin is one kernel with several holdings and cannot be sharded by construction; isolated margin is one kernel each, which is what FR-CORE-6 describes. Events route by instrument, an unnamed one reaches a lone shard and is refused past that, and an instrument no shard holds is refused rather than dropped. No threads: FR-CORE-1 forbids the core from spawning any and a scheduler-dependent result would not reproduce from `(journal, seed, commit)`. Running shards on threads is the host's decision, safe because they share nothing — and a test asserts interleaving them changes no fingerprint, which is the basis for it |
| Matching | L0 tick replay, frozen as the regression anchor | Built |
| Margin | Tiered maintenance margin, liquidation pricing, funding | Built |
| Margin | Bitemporal rule schedules | Built |
| Backtest | Run scheduling, funding, accounting, exports | Built |
| Parity | Trade-by-trade diff, difference attribution, identity triple | Built |
| Parity | **Run file format** (a baseline can be archived) | Built |
| Testing | **Property tests** for margin invariants (FR-MARGIN-7) | Built — and the first run found a real defect, below |
| Testing | **Property tests** for matching invariants (FR-MATCH-7) | Built — quantity conservation, price-time priority, no crossed book, no negative fills, plus determinism over generated scripts |
| Release | First public preview tag | Built |

### M2 — Python tier, fidelity reporting, research workflow

| Theme | Item | Status |
|---|---|---|
| Python | Compatibility mode (per-tick callbacks) | Built |
| Python | Throughput mode (batched callbacks, mirrored state) | Built, with its cost measured (up to ~7×, accuracy cost quoted per batch) |
| Python | Four-platform abi3 wheels in CI | Built |
| Margin fidelity | Cross-window tail divergence instrument | Built |
| Margin fidelity | **Methodology published** (G5) | Built (`MARGIN-FIDELITY.md`) |
| Data | Dual timestamps, strict as-of joins, bitemporal reference data | Built |
| Data | Arrow/Parquet columnar layer | Built (optional feature; default build stays at zero dependencies) |
| Features | `oq-features` skeleton (one definition, two paths, consistency metric) | Built |
| CLI | `data`, `replay`, `parity` subcommands | Built |
| CLI | `backtest`, `sweep` subcommands | **Deliberately absent** (a strategy is compiled Rust) |
| Research | Sweeps emit DSR/PBO by default | Built |
| Research | **Strict mode refusing over-threshold results** | Built — `SweepReport::refusals` names every reason, checks them independently so two failures report as two, and refuses an unscored sweep rather than waving it through. Three reasons now: PBO, deflated Sharpe, and an out-of-sample-on-in-sample Sharpe slope at or below zero |
| Fidelity | Fidelity report and participation-rate flag | Built |
| Adoption | Quickstart, eleven examples in `oq-examples`, goldens | Built |
| Adoption | **Classic-strategy catalogue** (RSI, MACD, Bollinger, Donchian, grid, dual-thrust) | Built — teaching references, each one documenting where it breaks rather than claiming an edge |
| Gate | **G3 throughput ≥8× the interpreted/Cython baseline** | **Current-workload verification pending.** Historical same-machine measurement on the public `DoubleMaStrategy` over public exchange archives: 5.04× per tick, 7.13× at the best batch, or 5.67× excluding baseline decode. Strategy arithmetic was 57.6% of the cost; the fastest batch changed the fill sequence. Requirements now calls for negligible strategy arithmetic and a representative workload alongside; this historical result does not pass that revised gate |
| Gate | **G7 strategy portability and mode comparison** | **Acceptance open.** Compatibility and `batch=1` equality are tested. For larger batches, `compare_modes` measures changed decision timing and fills; report that difference through G2 rather than promise equality. G3's current-workload benchmark remains required |
| Gate | ~~**G11 external cold start ≤ 30 minutes**~~ | **Withdrawn** — the reason is in [REQUIREMENTS](REQUIREMENTS.md). It did leave one real finding behind before it went: the quickstart's first instruction was `cargo install oq-cli`, and every published `oq-*` is a `0.0.1` name placeholder, so anyone following it failed in their first minute for a reason that had nothing to do with them. That was found by asking what an outsider would hit, and fixed as a bug — which is the form this kind of thing should take |

### M3 — Live trading

| Theme | Item | Status |
|---|---|---|
| Gateway | Reference perpetuals adapter (market data, orders, user stream) | Built; the whole loop has run against a testnet |
| Gateway | Reconciliation as a first-class object (lost, duplicate, out-of-order) | Built |
| Gateway | A second venue (OKX) | Built; **public half verified against the real venue, signed half unverified** |
| Gateway | **Conformance suite for execution adapters** | Built — all seven adapters driven through it (Aster shares Binance's), and three deliberately-wrong adapters caught by it |
| Gateway | **Broker/referral prefix scheme** | Built — `broker::IdScheme` composes ids carrying a venue-issued code, kept separate from the ownership prefix because they answer different questions |
| Risk | RiskGate: pre-trade checks, kill switch, startup reconciliation | Built |
| Risk | **Limit changes journalled as auditable events** | Built — `VersionedLimits` records which field moved and from what; a no-op does not advance the version |
| Live | Process assembly, snapshot recovery | Built — recovery starts from the venue's account snapshot and asks the venue about every order the journal leaves unresolved; positions are not rebuilt from the journal, by design (`recovery.rs`) |
| Live | **Books kept by the kernel** (one implementation for live and backtest) | Built |
| Live | **A strategy learns whether the venue took its order** | Built — `Strategy::on_placed`, called from the backtest loop *and* the live trader, so a strategy written against one runs on the other |
| Live | **A runnable strategy against a venue** | Built — `oq-live`'s `grid_live` example; `observe` sends nothing and `probe` is a diagnostic, so before it the repository had no way to run a strategy on a venue at all |
| Live | **One process per account, enforced** | Built — an interlock on `(deployment, symbol, id_prefix)`, claimed before startup reconciliation. Host-local, and says so: two hosts against one account is what the `foreign_orders` metric is for |
| Live | **A local control port** | Built — a Unix socket in the process's runtime directory, never in `/tmp`, answering `status`, `orders`, `metrics`, `attribution`, `halt`, `shutdown` and `resume`. Peers are checked by the uid the kernel reports, and every state-changing command is journalled with its reason and origin (`Record::Operator`) |
| Live | Graceful restart | Partial — signal handling, resting orders cancelled, recovery on the way back up; restarting the process is deployment and stays out |
| Attribution | **Gap decomposed by cause, with an unexplained residual** | Built |
| Attribution | Shadow → evidence → report, end to end | Built |
| Attribution | **Run files beside the journal** | Built — `<stem>.live.run` and `<stem>.model.run` (the venue's fills and the shadow's, under one identity) and `<stem>.oqtk` (the observations), rewritten every fifteen minutes and at exit, so `oq-parity` and `oq-parity markout` read a live run as they read a backtest |
| Observability | Latency histograms | Built |
| Observability | **Structured metrics, alert hooks** | Built — a snapshot rendered in the line-oriented form collectors read, and alerts as *judgements* rather than notifications: nothing here sends |
| Simulation | Gateway fuzzing (disconnects, reordering, duplication, partial fills) | Built |
| Simulation | **The whole live process against a simulated venue** | Built — the live loop reads time through an injected clock, and `oq_live::sim` supplies a seeded venue, market and account stream, including hedged accounts and a venue that misbehaves on purpose; `tests/dst.rs` runs the production `run_on` against it, so a seed names one run exactly |
| Cutover | Position-carrying playbook | **Written, never rehearsed**; its §6 lists what is missing before one can be |
| Cutover | Account record/compare tooling (a rehearsal precondition) | Built — both halves: `oq-recon` compares the **venue** against a record, `oq-belief` compares the **new process's own journal** against the same one |
| Entry trigger | 1. Core released ≥ 6 months, no open P0/P1 | **Not met** |
| Entry trigger | 2. Shadow run with every divergence attributed | **Not passed after historical review** — close the accounting and evidence defects listed in README Status; runtime alone does not close attribution |
| Entry trigger | 3. Two successful testnet cutover rehearsals | **Not met** |
| Entry trigger | 4. At least one real third-party user | **Not met** |

### M4 — HFT fidelity

| Theme | Item | Status |
|---|---|---|
| L1 | Queue position (conservative model) | Built |
| L1 | Entry and response latency | Built |
| L1 | Square-root taker impact | Built |
| L1 | Participation-rate alerting | Built (shipped with M2's fidelity report) |
| L1 | **Probabilistic queue model** | Not built — by the stated order, only after calibration |
| L1 | **Latency as distributions rather than constants** | Built — `Delay::Measured` takes p50/p90/p99/p999, which is what a latency measurement produces; nearest-rank and never interpolated, drawn deterministically from the order id so a replay from a snapshot fills the same way |
| L1 | **Feed latency** | Deliberately not in the engine — it belongs to the event producer |
| L1 | **Calibration against recorded fills** | **Pending evidence audit and calibration** — use available fills where their market, timing and depth provenance match; name any missing coverage before collecting more |
| L2 | Book reconstruction, snapshot reconciliation, gap handling | Reconstruction is built and now **measured on a real archive**: 133,910 depth updates, 0 sequence errors, 0 gaps, 0 crossed books, 10,286 levels deep — `RECONSTRUCTS CLEANLY`. **Both sides are now measured from it.** `L2Engine` reads the displayed size at the level an order joins, replacing L1's assumed `QueueAhead`, and walks the levels for taker fills, replacing L1's square-root penalty. Each measurement displaces the policy rather than compounding with it, a fill the book cannot reach keeps the policy, and `swept` / `unswept` report which priced a run. Several takers in one observation deplete one working copy of the depth, so size is not free for the second. Where book and tick disagree the worse price wins, which keeps the ladder monotone. No MBP feed can tell a cancellation ahead of you from one behind, so the queue depletes on trades only and is conservative by construction. **A backtest can now reach it**: `RunConfig::at_tier` selects the tier and `run_observations` takes ticks and depth on one stream, with `book_tiers` as the comparison — an L2 given no depth reproduces L0 exactly, an L1 told to assume 64 lots agrees to the fill with an L2 shown 64, and between 16 ahead and 64 most of a maker strategy stops existing. The two gaps this row used to name — converting an archive into that stream, and depth in the journal — are both closed; see the next two L2 rows |
| Validation | Stylized-facts test set | Built for the generated markets **and now measured on a real one**. `oq-data` reports the four facts for any tick file, so the comparison is a command rather than a study: four days of captured BTCUSDT (5,759 one-minute returns from 15.9M trades) hold three of four per day at excess kurtosis 8–11, against the fixtures' 0.03 / 0.07 / −0.05. The finding that matters is how little it takes to move that — pooled, kurtosis is 951 and ρ(1) fails; drop **two** of the 5,759 returns (one 4.1% minute on 251× median volume, and its rebound) and it is 24.6 with ρ(1) holding. A short sample's verdict is its largest event's verdict, which is why this stays a measurement and not a gate. **Order-flow autocorrelation is built**, and the order flow turned out to be in the archive already — the venue publishes which side crossed, and no adapter had read it. `oq-trade-check` reports it, `conformance` requires every adapter to parse it, and the measurement forced a distinction: over raw **trades** the lag-1 coefficient on an hour of BTCUSDT is 0.83 with a longest same-side run of 3,335 — which spans 14 milliseconds and is one order crossing three thousand resting ones, not three thousand decisions. Collapsed to **orders** (`as_orders`), 179,547 trades become 39,019 and lag-1 is **0.19**, decaying slowly and matching the published figure. Both are reported: the raw series is what a queue faces, the collapsed one is what the literature means |
| L2 | **An archive converted into a run's input** | Built — `fold_into_observations` interleaves ticks and depth in the order the venue produced them, and `oq-tiers` is the command: one strategy, one archive, two tiers. Over ten hours spread across four days of captured BTCUSDT — 1.3M depth updates, **none refused** — a bid resting at the touch filled 13,972 times at L0 and 3,140 at L2. **77.5% of the backtest's fills are trades the queue never reached**, and no hour kept more than 44.6% or fewer than 12.5%. The book is bootstrapped from the first update rather than a REST snapshot, which the archive does not hold, so early queues read shorter than they were |
| L2 | **Depth in the journal** | Built — `Event::Depth` is kind 9 and the first variable-length payload in the schema, carrying its own level counts so decode can refuse a body that disagrees with them. A test replays an L2 run and compares outputs and state against the live run, and fails if the depth is dropped. `Event` loses `Copy`, which a list of levels cannot be. The cost is journal size: a captured hour of one instrument is over a hundred thousand updates, so a run that does not read depth should not be handed any |
| Validation | L0 / L0+margin / L1 comparative report | Built, and now covering L2 as well. `tiers` compares L0, L0+margin and L1 and shows the rungs are **not** ordered by pessimism. `book_tiers` adds L2 against a generated book at a range of thicknesses — an unfed L2 reproduces L0 exactly, and an L1 told to assume 64 lots agrees to the fill with an L2 shown 64. `oq-tiers` runs the same comparison against a captured archive, which is where the 77.5% figure comes from. Fill counts no longer stand alone: `TierDivergence` sets each tier's total return and Sharpe ratio side by side, `FidelityReport::with_tiers` carries it, and `tiers_invalidate` refuses a run whose most faithful tier reverses the sign of the cheapest one's conclusion — on `book_tiers` the same resting bid returns +5.86% at L0 and +0.08% at L2 with 64 lots ahead |

### M5 — AI extensions

| Theme | Item | Status |
|---|---|---|
| Inference | `oq-infer` ONNX and compiled trees | **Not built** |
| Inference | Prediction parity gate | **Not built** |
| Environments | `oq-env` gym-style vectorized environments | Built — `Env` and `VecEnv`, zero dependencies. The inversion is done by taking the backtest loop apart rather than by a thread and a channel: `step` advances one observation, so nothing is concurrent and **G10**'s reproduction test holds by construction. An action is a target position rather than an order, and the cost is stated — an agent trained here cannot learn to work an order, because it never sees one. Reward is the change in equity, which is the one definition needing no parameter and is a poor signal alone; `Step::equity` is there so a caller can shape their own. Batch seeds are derived with SplitMix64, because sequential seeds correlate on the first draw and nothing in one episode shows it |
| Features | `oq-features` production point-in-time layer with drift monitoring | **Drift monitoring built**; the store is not. `drift::Reference` freezes a distribution from the data a model was fitted on and `Monitor` bins live values against it in O(1) per observation, so it runs in the loop that produces the feature rather than in a job that notices tomorrow. Bins are frozen and quantile-spaced, which is the whole trick: bins re-derived per window move with the data, so a distribution that shifted bodily re-bins itself and reports calm — there is a test that fails if that ever becomes true here. Three things are reported apart rather than summed, because a single index hides the two worst cases: values outside anything the reference saw (binning absorbs them into an edge), a feature that has stopped answering (moves no bin at all), and a NaN (a failed computation, not an unusual number). The conventional 0.25 threshold is offered as a starting point and labelled as convention, because it comes from credit scoring and nothing calibrated it to tick features. What is still not built is the store: persistence, registry, materialisation, scheduling |
| Sandbox | `oq-lab` | **Not built** |

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

- **Fidelity report (FR-MATCH-3, FR-MATCH-4).** `oq_backtest::validity`
  reports participation rate, maker/taker split, the tier's latency and
  impact assumptions, and — when a run asks for it — peak margin usage and
  closest approach to liquidation, flagging a run whose peak participation
  says it replayed a market it would have moved.

  Two honest limits. It is a **call**, not a field: `report(&result, &ticks,
  …)` needs the tick series and a streamed run has already consumed it, so
  "every backtest emits a fidelity report" is satisfied by it being one line
  with no configuration rather than by it being automatic — a weaker
  guarantee, and stated as one. And margin tracking is **opt-in** because
  measuring it costs about a fifth of the engine's throughput and cannot be
  sampled: the figure wanted is an extreme, and a closest approach that
  missed the closest approach is worse than none. A run that did not ask
  reports `NotTracked`, never zero.

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
- **G2** — run-equivalence tooling compares two runs trade by trade and
  attributes differences. An equality result means identical time, price,
  quantity and side, with relative P&L error ≤ 1e-6. Equality with a different
  engine is a migration check, not the framework's acceptance promise.
- Property test suite green, including margin invariants.
- **First public release** — a `vX.Y.Z` tag, with sample data and one example
  strategy.

---

## M2 — Python tier, margin fidelity, research workflow

**Entry.** M1 exit gate met.

**Scope.**

- `oq-py`: compatibility mode formalized; throughput mode designed and
  implemented (subscription granularity, batched callbacks, mirrored state).
  Batching is not free and the cost is not hidden: a strategy called every
  `n` ticks cannot act on ticks 1..n-1 of a batch, so its decisions are late
  by up to `n - 1` ticks. `batch=1` is exactly compatibility mode and a test
  asserts the two are identical; for `n > 1` the divergence is **measured**
  by `compare_modes` rather than assumed small. On the example crossover it
  costs 18% of the strategy's edge at `n = 64` and destroys it at `n = 512`.
  What it buys is measured too, because a price quoted without goods is not a
  trade: over 200,000 ticks with a strategy that does nothing, throughput mode
  runs at up to about 7x compatibility mode. A batch of 8 buys 2.8x for 1.3%
  of the edge, 64 buys 5.8x for 18%, 512 buys 6.9x and takes the edge away.
  Which is acceptable is a property of the strategy, so the binding measures
  rather than chooses.
- **Margin fidelity reporting (G5).** Recompute historical stress windows with
  and without the margin overlay and publish the methodology behind the
  **tail-divergence report**, published as
  [MARGIN-FIDELITY.md](MARGIN-FIDELITY.md). For leveraged strategies this is the single most
  valuable deliverable of the entire plan: it quantifies how optimistic
  margin-free backtesting actually is.
- `oq-data`: dual-timestamp Arrow/Parquet layer, bitemporal reference data,
  strict as-of join utilities; `oq-features` skeleton — one definition, two
  execution paths derived from it rather than written twice, and a
  consistency metric tested against the four classic ways they drift apart.
- `oq-cli`: `data`, `replay` and `parity` subcommands, shipped; `oq-stats`
  integrated so sweeps emit DSR/PBO by default. `parity` was recorded here as
  *not coming*, on the grounds that `compare` takes a `RunManifest` and a
  `RunOutput` and neither had a serialised form. That was the honest answer and
  it named the wrong thing as impossible: the **file format** was what was
  missing, `oq_parity::wire` is it, and the subcommand followed in an
  afternoon. `backtest` and `sweep` remain absent for a reason that is a
  property of the design rather than the schedule — a strategy is compiled
  Rust, so no argument can name one and the subcommand could only ever be a
  worse `cargo run --example`. `oq` names both when asked, rather than
  reporting an unknown command.
- **Adoption readiness:** quickstart documentation, at least two example
  strategies, and a sample dataset with golden tests. All three exist. The
  fourth item here used to be a timed cold start by an outsider; `G11` was
  withdrawn and the reason is in [REQUIREMENTS](REQUIREMENTS.md).

**Exit gate.**

- **G3** throughput and **G4** sweep targets met. G4 is met and checked in
  CI by `cargo run --release -p oq-examples --example sweep_100`: 100
  configurations over 600,000 ticks each, with DSR and PBO, in 2.65 s of a
  1,800 s budget on a development machine. G3 has been measured on one
  machine on a representative strategy at 7.13× against a target of 8×.
  That is a historical workload result, not acceptance under the current
  G3 definition: negligible strategy arithmetic, plus a representative
  workload reported alongside. The gate remains open; see below.
- **G5** margin fidelity verified; tail-divergence methodology published.
  **Met.** [MARGIN-FIDELITY.md](MARGIN-FIDELITY.md), with the instrument in
  `oq_backtest::fidelity` and the study in `examples/margin_fidelity`.
- **G7** — a strategy runs unchanged in compatibility mode and, after
  throughput-mode conversion, re-passes parity. **Half met.** The mode half
  is done and tested: a strategy with no framework in it runs in
  compatibility mode, converting it to throughput mode is a change to one
  method, and `compare_modes` asserts `batch=1` produces an identical run
  rather than the documentation asserting it. For `batch > 1`, G2 produces
  a comparison and attribution; it does not promise equality. G7 still
  requires its throughput and portability evidence, and the measured
  timing/accuracy trade-off must accompany any batching recommendation.
- ~~**G11** initial verification~~ — **withdrawn**, not deferred. A
  stopwatch on a first backtest measures the wrong thing: a tool worth
  using is worth more than thirty minutes, and one that is not is not
  worth one. It also measured it with an instrument that cannot work —
  a single reader's trial is n=1 and varies more by who and what machine
  than by anything here. What the goal was reaching for is real and is
  kept as ordinary work: the quickstart, the examples and the goldens
  all exist, and a documented command that does not run is a bug
  to fix when found, not a gate to fail.
- **Beta release** with documented, if still unstable, APIs.

**Historical measurements are not silently promoted to current gates.**
The representative-strategy run measured 7.13×, with 57.6% of its cost in
strategy arithmetic. It motivates G3's current workload definition; a new
same-machine measurement must use that definition. G7's batching comparison
shows a changed fill sequence because orders are submitted at batch end.
That is an attributed semantic difference, not an equality pass. Neither
result is waiting for additional testnet trading data.

What *is* measured, and reported under its own name rather than G3's: the
Python tier's throughput mode runs at up to about 7x its compatibility mode
over 200,000 ticks. That is a different comparison — one binding against
another, not this engine against an interpreted one — and it is recorded
above as what it is.

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
  and for a long time nothing produced that attribution — the requirement named
  a standard without naming an instrument. **The instrument now exists**:
  `oq_parity::attribution` decomposes the gap into slippage, queue position,
  funding against model, latency, and fee tier, and reports what will not
  decompose as an **unexplained residual**. The gap comes from two independent
  sources — the venue's realized P&L and the kernel's — and the components are
  computed separately, so the residual is *subtracted rather than assembled*; a
  test deliberately mis-computes a component and asserts the error lands in it.
  Its prerequisite is that the live process journals its decisions, which it
  does now — `oq-live` depends on `oq-journal` and writes in `record.rs`. The
  second is now met at the kernel: `oq-live` depends on `oq-core` and
  `oq-margin`; `oq_live::shadow` runs the same `Kernel` a backtest runs beside
  the live session, reporting where it and the venue disagree in four named
  kinds plus a position comparison, and handing that on to
  `oq_parity::attribution` through `Shadow::evidence` — the last unconnected
  seam in this project's headline claim, where the shadow produced divergences,
  the decomposition wanted evidence, and nothing turned one into the other.
  Funding and fees are **arguments** to it rather than fields, because a shadow
  does not see them: passing `None` says nobody read the venue's statement, and
  the report then declines to produce a residual rather than reporting a gap
  explained by causes nobody measured. And `oq_core::Matching` closes the half
  that was missing. A kernel can now take fills the **venue** decided rather
  than producing its own, with identical accounting — a test asserts the same
  trade booked both ways leaves the same position, entry, fees, balance and
  equity. That is what makes §1's claim, that backtest and live differ only in
  the event producer, true rather than aspirational: one implementation of the
  books, and only the source of fills moves.

  Two refusals hold it together. Under `Matching::Venue` the matcher never
  fills, because a kernel that both matched and took venue fills would book
  every trade twice and the second copy would look exactly like the first. And
  a venue fill arriving at a `Simulated` kernel is refused rather than applied,
  since a simulated run produces its own. A filled order also leaves the book,
  which is about *replay* rather than the live run: a journal carrying both the
  submit and the venue's fill, replayed by a build whose mode was not set,
  would rest the order and match it too.

  That assembly is done. `oq-live`'s own books (`books.rs`) are a kernel in
  `Venue` mode, so the process keeps its account with the backtest's
  accounting, and the shadow beside them is a kernel in `Simulated` mode;
  every run ends by handing both to `oq_parity::attribution`.
- `oq-sim` at full strength: the entire scenario catalogue plus gateway fuzzing
  (disconnects, reordering, duplication, partial fills). **The gateway half
  exists** — `oq-live`'s `gateway_fuzz` drives the live books through every
  scenario in the catalogue, asserting invariants rather than outputs: a trade
  booked twice moves the account once, reordering reports does not change where
  it ends up, a dropped report leaves a disagreement the reconciler names, and
  nothing the venue can send makes the books invent a fill.

  Its first run found one. The books had no deduplication — the order tracker
  had it and the kernel-backed books added later did not, which is the gap that
  opens when one concern is implemented in two places at two times. A
  redelivered fill doubled the position, and a reconnecting stream repeating
  itself is routine rather than exotic.
- **Position-carrying cutover playbook**, rehearsed end-to-end. The playbook
  itself is written — [CUTOVER.md](CUTOVER.md) — and is explicit that it is a
  skeleton: every step is specified and every command in it exists, and none
  of it has been rehearsed. Its §6 lists what is missing before a rehearsal
  can be run at all, and §7 what each rehearsal must produce.

**Exit gate.**

- **G6** latency target met at the defined boundary.
- Shadow-run agreement rate at target, with every divergence attributed.
- **G12** — replay the live journal through the same kernel and produce
  a cause-by-cause attribution report with a numeric unexplained residual
  alongside its bound `RunManifest`. A missing component prevents this
  gate from passing; the residual threshold remains unset until measured,
  as specified in Requirements.
- Full scenario catalogue passing under fuzzing.
- Staged rollout stable for ≥ 2 weeks.

## Evidence-led validation

The first historical review has completed; its scoped outcomes and issue
links live in [README Status](../README.md#historical-validation-review--2026-10-11).
Execution checks and evidence-enriched accounting replay have been run.
Remaining work now names accounting precision, shadow fill deduplication
and fee/funding provenance, rather than an unspecified lack of runtime.

Use the records described in [README Status](../README.md#status) before
asking for another run. Each audit ends in **pass**, **fail**, or **specific
missing evidence**, with the interval, build identity, data/config hashes,
command and comparison criteria recorded. A deployed build is not assumed
to be the current public release. Existing restart records count as cutover
evidence only if they establish the playbook's required outcomes; ordinary
uptime cannot substitute for a rehearsal. Verify the two-week staged-rollout
window rather than inferring it from the existence of older files.

| Priority | Deliverable | Completion evidence |
|---|---|---|
| P0 | Full-window execution and account audit | Orders, positions and ledger economics reconciled over the selected interval; missed/duplicate fills and recovery episodes accounted for; live journal replay compared against recorded outputs |
| P0 | G12 attribution closure | Recover missing funding/fee statements and recompute, or identify uncovered intervals and their effect; independently computed components and numeric residual bound to a `RunManifest` |
| P0 | L2 input-validity audit | Archive bootstrap snapshots and post-gap reinitialisation reach both the aggregator and matcher; queue sampling uses exchange-arrival depth; reproduction cases cover disconnects, reconnects and in-flight depth changes, and unusable intervals are labelled |
| P1 | Cutover acceptance | Audit earlier evidence against [CUTOVER](CUTOVER.md); complete missing rehearsals, including abort coverage, and record order ownership, position/entry-price agreement and exposure intervals |
| P1 | Reconciliation race coverage | Deterministic cases for stale REST snapshots racing stream fills, bounded history, unknown submissions, delayed reports and external orders; a missing position report is not interpreted as flat |
| P1 | Fill and latency calibration | Fill probability, time-to-fill, partial fills and markout compared by liquidity state; feed, entry and response timing kept distinct and tied to the same market as the fills |
| P1 | Capture quality report | Per-stream gaps, sequence integrity, snapshot coverage, retained-file verification and storage growth; verify daily margin-rule snapshot coverage |
| P1 | Restart convergence | Feature values and decisions compared over aligned common intervals at different warm-up lengths; undefined values and decision changes reported separately |
| P2 | Research reproducibility | Rolling OOS experiments linked to all attempted trials, fidelity reports and deployment manifests through existing manifest and trial-ledger primitives |

Testnet records can close execution, accounting, replay and recovery checks,
and calibrate testnet behavior. They do not establish production-market
maker queue fidelity. Before requesting new calibration samples, list the
specific missing market, liquidity state, timestamp or snapshot coverage.
More elapsed time does not add a missing socket-write measurement for G6.

### Open-source designs to adapt

Reviewed against upstream documentation/source and this workspace on
2026-10-11. These are design references, not new exit gates or dependencies.
Resolve the accounting/evidence defects in README Status first; adapt the
smallest extension to existing primitives rather than another framework.
The reviewed work is tracked in [#356](https://github.com/openquanter/openquanter/issues/356).

- [NautilusTrader reconciliation](https://nautilustrader.io/docs/latest/concepts/execution/reconciliation/):
  extend existing `Unknown`/recovery behavior with report coverage,
  completeness and snapshot freshness. Distinguish explicit flat from a
  missing report, and observed venue events from synthetic reconciliation.
  Synthetic alignment cannot recover historical P&L. Do not import timeout
  terminalization or silent position repair into fatal-on-unknown semantics.
- [HftBacktest queue models](https://hftbacktest.readthedocs.io/en/latest/order_fill.html)
  and [latency models](https://hftbacktest.readthedocs.io/en/latest/latency_models.html):
  compare conservative/probabilistic queues against fills and separate feed,
  entry and response latency. A market-by-price queue is an estimate, not an
  observation of the actual FIFO position. Its
  [probability queue implementation](https://github.com/nkaz001/hftbacktest/blob/master/hftbacktest/src/backtest/models/queue.rs)
  adjusts expected queue size deterministically, without RNG, and subtracts
  previously accounted trades from depth decreases to avoid double counting.
  Adapt the semantics to the fixed-point core. Validate snapshot/gap handling
  and queue sampling at **exchange arrival**, not submission. L2 seals the
  due reading before a later depth update and does not spend pre-arrival
  volume; matching between observation boundaries remains approximate. Preserve a venue
  view and an arrival-time strategy view; reuse `oq-data`'s arrival-based as-of
  joins. Timestamp subtraction alone is not calibrated one-way latency when
  clock offsets are unknown. Replay cannot change the future market path;
  per-observation taker depletion is not a full market-impact model.
  Calibration precedes choosing a richer model.
- [Freqtrade recursive analysis](https://www.freqtrade.io/en/stable/recursive-analysis/):
  upstream compares indicators at the last row, not entry/exit decisions.
  Reuse `oq-features::consistency`, then extend it with aligned common-window
  comparisons at different history lengths and actual decision differences,
  including undefined warm-up values. Do not call indicator convergence a
  strategy-restart parity result or duplicate the existing lookahead check.
- [Qlib experiment recording](https://qlib.readthedocs.io/en/latest/component/recorder.html):
  link data versions, every trial, rolling OOS results and deployed artifacts.
  `RunManifest` and persistent `TrialRegistry` already identify runs and count
  earlier sweeps/unscored trials. The incremental work is artifact lineage,
  run status and rolling-OOS orchestration outside the deterministic core;
  a recorder itself proves neither leakage safety nor overfitting control.
- [Hummingbot executors](https://hummingbot.org/strategies/v2-strategies/executors/):
  reusable order-lifecycle components once at least two strategies need the
  same lifecycle. Adapt cancel/replace and partial-fill behavior into journalled
  event state machines with injected time, shared by backtest and live; the
  upstream asynchronous executor runtime is not an implementation to transplant.

Two additional references remain conditional rather than immediate work:
[LEAN's transaction-model example](https://github.com/QuantConnect/Lean/blob/master/Algorithm.CSharp/CustomModelsAlgorithm.cs)
suggests explicit, versioned venue-rule profiles over the existing fill/fee/
margin components, not another accounting engine. Its sample random fill and
unlimited-buying-power policies are demonstrations, not defaults to import.
[VeighNa's algorithm execution module](https://github.com/vnpy/vnpy_algotrading)
provides TWAP/iceberg lifecycle examples; portfolio or multi-leg ownership
requires a concrete account model and use case before adding another manager.

Public reports use generic reproduction cases and aggregate acceptance
conclusions. Strategies, parameters, account statements, captured data,
deployment details and operational incident records stay outside this
repository, as required by [the boundary policy](REQUIREMENTS.md#8-repository-boundary-policy).

---

## M4 — HFT fidelity

**Entry trigger.** Both of:

1. A concrete maker or high-frequency strategy candidate exists whose edge is
   limited by simulation fidelity — not a hypothetical one.
2. Enough incremental depth has been captured for the question being
   asked. **Not a fixed number of months**, which is what this said and
   could not justify: the figure appeared once, with no argument, next
   to an unrelated "≥ 6 months" about how long the core has been
   released. What L2 needs depends on what it is being used to answer:

   | To answer | Needs | Why |
   |---|---|---|
   | Does the matcher reconstruct and match? | days | only that sequencing, gaps and snapshot reconciliation hold on real data |
   | Is the queue model calibrated? | weeks | enough liquidity states, not enough to span seasons |
   | Does a maker strategy's edge survive? | months | this is where a multi-month window is actually the argument |
   | Is the result statistically significant? | longer still | and this project's position is that one run is not evidence |

   Assess the retained archive against these questions, not the original
   trial's duration. A longer window is useful only after its gaps,
   snapshots and sequence integrity have been audited.

**Scope.**

- **L1**: **a first version exists** — `oq_engine::l1` models queue position,
  entry and response latency, and a square-root taker impact penalty, and
  participation-rate alerting shipped with the fidelity report at M2. What
  remains is the calibration this milestone is actually about: the model takes
  a `Policy` of assumptions, because the tick format carries neither book depth
  nor this deployment's real latency, and turning those assumptions into
  measurements starts with an audit of the available fills and their matching
  market data; missing coverage is stated explicitly. Latency as
  *distributions* rather than constants is built — `Delay::Measured` takes
  four quantiles. Two further pieces are not built: a probabilistic queue model
  (the shipped one is the conservative one, which is the stated order), and
  **feed** latency — which is a property of the event producer rather than the
  matcher, so it belongs to the host loop and putting it in the engine as well
  would delay the same event twice.
- **L2**: order book reconstruction from incremental depth, matching against
  the reconstructed book, snapshot reconciliation and gap handling. Built:
  reconstruction, the measured queue, and the taker's walk up the levels.
  Validate the entire snapshot/gap path before calibration: captured gaps
  and sequence refusals must suspend matching until an explicit snapshot,
  and reconnect snapshots must retain their boundary in the observation
  stream. Reproduction must label suspended intervals and conservative
  priority/first-bar recovery. A legacy empty archive bootstrap is not
  evidence of full initial depth; see the README for current limitations. Beyond them remains reaction — the book is what the venue
  displayed, and
  how the rest of the market answers a trade of ours is a claim nothing in
  it measures.
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
| `oq-env` | ~~L1 fidelity achieved (M4)~~ — **triggered and built** | Gym-style environments with inverted control and native vectorized batching, seeds threaded through every random source (**G10**). `Env` and `VecEnv` ship; the inversion is done by taking the backtest loop apart rather than with a thread, so the reproduction test holds because nothing is concurrent |
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

## API stability

API stability is a **commitment**, not a feature count, and not a version
number: releases are numbered in plain sequence (see
[Versioning](VERSIONING.md)), and this milestone is announced when it is met.
It requires:

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
| Release `vX.Y.Z` | When `main` has something worth shipping | A tag on a green `main`; notes are the changelog section |
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
| The Rust core becomes an implementation detail of a Python library | Medium | The engine must stay buildable and testable with no interpreter present. Enforced rather than intended: per-crate dependency budgets keep eighteen crates at zero, and the composability check builds each crate on its own |
| Scope inflation | High | Committed scope ends at M2; everything later is trigger-gated |
| Proprietary content leaking into the public repository | High | Fresh history; secret and pattern scanning in CI; proprietary material only in private overlays; pre-release manual audit |
| Bus factor of one | Medium-high | Agent-friendly codebase (per-crate `AGENTS.md`, layered verification anchors); behavior encoded in deterministic tests; all design rationale written down rather than remembered |
| Building the framework becomes the goal instead of using it | High | Every milestone states the capability it unlocks; the question "what got measurably better because of this?" is asked at each gate |
| The execution seam has one implementation and no second | Medium | Was: no seam at all. `Execution` now exists and `oq-gateway` implements it for eight venues, so the layers above no longer name one. Both sides now have a conformance suite their adapters pass — driven by payloads each adapter supplies, so each tests the contract rather than one venue's bytes; the market data one caught a wrongly stated convention on its first run, and the order-side one exists because Binance answers a refusal with an HTTP status while OKX answers one inside a 200, which silently mislabels every refusal for a classifier written around the first. What remains is narrower than the seam: OKX's signed half needs credentials and a live account to exercise, and until it is, the second implementation is verified on its unsigned half only |
| ~~The instrument model is split in two, and neither half is in the core~~ | Partly closed | **The identity and precision half is done.** `oq_types::Instrument` is in the core and carries price and quantity scales, contract size, the price and quantity grids, and the venue's minimum notional; `oq_margin::Contract::of` derives the economics from it rather than restating them, so a hand-written `tick_cash` cannot drift from a definition after a relisting changes a precision. `oq-l2feed` no longer has a second one. **The currency half is now half-closed.** `Currency` is a ticker, `Balances` is what an account holds by currency, and `State` keeps one — so a book *settling* in more than one currency is expressible: a payout in something the account does not settle in is held and reported rather than converted at an implied rate of 1 or dropped. *Totalling* across currencies is now built too: `State::equity_across` takes a table of rates with the instant they were read, triangulates nothing, and answers `None` rather than a partial sum when a rate is missing; `equity` still answers in the settlement currency, because that is what a venue liquidates against. `Cash` itself is unchanged and deliberately so — the currency belongs to the slot, as an instrument belongs to a book rather than to each price |
| ~~`Strategy` is defined in the backtest host~~ | Closed | Live execution would either have depended on `oq-backtest` or needed a second strategy trait, and G7 — the same strategy unchanged in both modes — could not hold either way. The trait now lives in `oq-strategy`, below both hosts, and `oq-backtest` and `oq-live` both depend on it |
| ~~L0 is the frozen regression anchor and has no matching seam~~ | Closed | Resolved for L1 without the refactor this entry proposed. `L1Engine` **owns** an `L0Engine` and never modifies it — orders are held outside it until they are entitled to be in it, and its fills are adjusted after it produces them. So L0 needed no seam, no trait and no change at all, and a test asserts a transparent L1 policy reproduces L0's fills exactly. L2 may still need the seam; L1 established that wrapping is enough to try first |


The last four entries were not hypotheses. They were present-tense structural
gaps found by reading the code against this plan — two are closed since and
two narrowed to what their rows say — and they share a shape: each is cheap
to fix now and grows more expensive with every crate, binding or
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
SLA before the API-stability milestone.
