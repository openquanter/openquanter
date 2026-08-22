# Changelog

[English](CHANGELOG.md) · [中文](CHANGELOG.zh-CN.md)

Referenced by [Versioning](docs/VERSIONING.md), which is where the
release stages and their promises are defined. This file records what
changed; that one records what a version number means.

Two rules, from [Roadmap](docs/ROADMAP.md#release-cadence) and
[Implementation Plan](docs/IMPLEMENTATION.md) §6, and they are the
reason this file exists at all:

- **Any change to L0 matching semantics, margin computation, or the
  event schema gets an explicit entry here**, plus a parity report
  showing the behavioral delta. A silent semantics change makes every
  earlier result unattributable.
- **Golden baselines are regenerated only with human confirmation
  recorded in the pull request.** If an entry below changes a number the
  documentation quotes, it says so.

`2.0.0-alpha.N` promises nothing about API stability. Entries are listed
so a reader can see what moved, not because anything is deprecated
gracefully — before 2.0 there is no deprecation period.

## Unreleased — 2.0.0-alpha.1

The first version-stamped state of the workspace. Nothing has been
tagged or published to crates.io yet, so everything below is "since the
repository started" rather than since a previous release.

### Engine

- `oq-types` — domain types, `i64` fixed-point arithmetic, typestate
  order and position state machines.
- `oq-hash` — SHA-256 and CRC-32, shared by the journal, capture and
  parity.
- `oq-journal` — append log with snapshots, replay, and torn-tail
  tolerance.
- `oq-core` — journal-first sequencer and deterministic kernel. Replay
  reproduces both the output sequence and final account state exactly,
  asserted by test including a liquidation path.
- `oq-engine` — L0 tick-replay matching with gap fill, price improvement
  and price-time priority. Frozen as the regression anchor. Gap-crossed
  fills are stamped with the previous tick.
- `oq-margin` — tiered maintenance margin, liquidation pricing derived
  rather than copied, funding with spike injection, bitemporal rule
  schedules.
- `oq-backtest` — run host, strategies observe their own fills, and the
  margin deviation report that runs a strategy twice and quantifies what
  a margin-free run overstates.
- `oq-data` — dual-timestamp ticks, leakage-free as-of joins, bitemporal
  reference data. Tick files stream rather than buffer whole. `.oqtk`
  format at v2; ticks carry traded volume.
- `oq-parity` — fill-by-fill run comparison with difference attribution;
  baselines identified by the (commit, data hash, config hash) triple, so
  a stale baseline reports itself instead of masquerading as a
  regression.
- `oq-stats` — deflated Sharpe ratio, PBO via CSCV, trial registry.

### Fees

Maker/taker trading fees are charged in the kernel. A maker rate may be
negative, because rebates exist and a model that floors at zero cannot
represent the strategies that live on them. **Fees default to zero and
must be set deliberately** — the examples do not set them, so every
number the documentation quotes is gross of costs.

### Capture

- `oq-l2feed` — verbatim record framing, UTC-day rotation, sealing with
  content-hashed manifests. Survives the day boundary; flushes on a timer
  rather than only on a record count.
- `oq-capture` — live client for Binance perpetual streams, capturing the
  streams the venue actually serves (some accept a subscription and then
  send nothing; see [Capture Format](docs/CAPTURE-FORMAT.md)).
- `oq-book-check` — replays an archive back into an order book and reports
  whether it reconstructs. Bytes on disk prove the messages arrived; only
  a reconstruction proves they can be used. This is archive verification,
  **not** the L2 fidelity tier.

### Examples and performance

- `oq-examples` — three teaching examples on a seeded synthetic market,
  with `tests/golden.rs` pinning every number the documentation quotes.
- `criterion` benchmarks, plus a CI job asserting a throughput **floor**
  rather than a tracked baseline. Shared runners vary by several times
  from hour to hour, and a gate that fails on noise gets disabled;
  precise comparison is `cargo bench` on one machine.

### Project

- One version across the workspace, declared once in the root
  `Cargo.toml`. See [Versioning](docs/VERSIONING.md).
- CI: build, test, `fmt --check`, `clippy -D warnings`, secret and
  deployment-detail scanning over the working tree and history, a
  dependency-budget and standalone-build check
  (`scripts/check-composability.sh`), and the throughput floor.
- DCO sign-off enforced; CLA for substantial contributions.
- Bilingual documentation: requirements, roadmap, implementation plan,
  quickstart, versioning, capture format, tick format.
