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

### Semantics and event schema

Recorded here because the rules above require it, and they were not
followed at the time — each of these changed behaviour and landed
without an entry. Reconstructed after the fact, which is worse than
writing it down and better than leaving it out.

- **`Contract::notional` and `Contract::unrealized` saturate rather than
  wrap.** *Changes results.* Both computed in `i128` and converted with
  `as i64`, which truncates: a notional past the range came back
  negative, put the position in the first tier, produced a maintenance
  requirement of zero, and made it unliquidatable. Found by a generated
  input under `FR-MARGIN-7`. **Behavioural delta:** any run whose price
  times quantity times tick value exceeded `i64::MAX` now has a
  maintenance requirement where it previously had none, and may liquidate
  where it previously could not. Runs inside the range are unchanged, and
  every golden in the repository was inside it — none needed
  regenerating.
- **A record declaring no trade is no longer parsed as one.** *Changes
  results.* Binance publishes `"p":"0","q":"0","X":"NA"` records among
  real trades — 19,725 in one day of BTCUSDT against 5.4 million. They
  were parsed into `Trade { price: 0 }` and folded into ticks, and a
  window's low is the minimum of its prices, so one zero made it zero:
  1355 of 1409 minutes of real capture reported a low of `0.00` with the
  high right beside it. A resting buy is triggered by the low.
  **Behavioural delta:** any tick built from a venue that emits such
  records changes — lows become real, and windows previously dropped as
  "before the first trade" are emitted. The generated fixtures contain
  no such record, so every golden in the repository is unchanged. The
  rule is now the adapter contract (`parse_trade` returns a positive
  price and quantity or `None`) and `conformance` holds every adapter to
  it, so the same fault cannot return through a new venue.
- **`Event::Depth` (kind 9) added to the event schema, and `Event` is no
  longer `Copy`.** *Changes results for L2 runs, and only for those.* An
  L2 run's fills depend on the book its orders queued in, and a journal
  without it replayed the orders into a different market — reproducing
  what was asked for rather than what happened. That was the one place
  in this kernel where replay was not faithful, and it now is.
  **Behavioural delta:** none for L0 or L1, which read no depth; an L2
  replay now reproduces its run's fills instead of a lower tier's.
  Additive on disk: journals written before it contain no such record,
  so replaying an existing one is unchanged, and a journal written
  *with* it cannot be read by an earlier build — which the per-kind
  length check makes loud rather than silent. **This is the first
  variable-length kind.** Every other payload is a fixed size and
  decode refuses anything else; a depth update carries its own level
  counts and decode checks the byte count against them, which is the
  same rule stated against a declaration instead of a constant. `Copy`
  goes because a list of levels is not one; the six call sites that
  needed it were mechanical.
- **`Event::VenueFill` (kind 8) added to the event schema.** A fill the
  venue decided, as opposed to one the matcher produced. Additive:
  journals written before it contain no such record, so replay of an
  existing journal is unchanged. A journal written *with* it cannot be
  read by an earlier build, which is what the per-kind length check
  exists to make loud rather than silent.
- **`State::matching` added, defaulting to `Matching::Simulated`.** Under
  `Venue` the matcher holds resting orders and never fills one; fills
  arrive as `VenueFill`. The default is today's behaviour, so no existing
  run changes.
- **`RejectReason::NotVenueMatched` added.** A venue fill arriving at a
  simulated kernel is refused rather than applied.
- **`oq_gateway::OrderUpdate` gains `side` and `maker`.** The venue was
  sending both and the parser discarded them. `maker` decides the fee,
  which on some venues is the difference between a rebate and a charge.
  Absent means taker.
- **`binance::classify` is now total over responses**, handling 2xx by
  delegating to `ack_from`. Its contract surface was a pair where OKX's
  was one function, so no single conformance suite could drive both.
  `place` is unaffected — it never hands a 2xx to that function.
- **`RunResult::margin_usage` replaces nothing and adds a field**;
  `RunConfig::track_margin` defaults to off, so no existing run changes
  or pays for it.

### Documentation

- The `sweep_100` benchmark ran on `MarketShape::trending(600_000)`,
  whose drift compounds per observation: the price ended at exactly
  `i64::MAX`. **Every statistic that gate has printed was arithmetic on a
  saturated price**, including the PBO of 0.4975 quoted in its own
  output. It runs on a calm market now and asserts the market it got. No
  documentation quoted those figures, so nothing else needed changing.

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

**The classics catalogue's levered numbers moved.** *Changes results.*
`GridTrader` used to advance its ladder when it submitted a rung; it now
advances on the fill, anchored on the price actually paid, with at most
one rung outstanding. The grid's levered result went from 4.06 to 4.46
and its margin-free arm from −513.74 to −508.12. QUICKSTART quotes both,
in two languages, and nothing failed when they changed — the catalogue
was never pinned in `tests/golden.rs`. It is now, every levered row plus
the claim the documentation actually makes: unlevered, the two arms agree
for all six. Verified by putting 4.06 back, which fails with
`expected 4.06, got 4.46`.

- **`L2Engine` — the queue and the taker's cost read from the venue's
  book.** It wraps `L1Engine` the way that wraps `L0Engine`, so L0 stays
  frozen by construction. The size displayed at the level an order joins
  is the queue ahead of it, replacing L1's assumed `QueueAhead`; a taker
  fill walks the levels and pays the weighted price of the walk,
  replacing L1's square-root penalty. Each measurement displaces the
  policy rather than compounding with it, a fill the book cannot reach
  keeps the policy, and `swept` / `unswept` say which priced a run.
  Where the book and the tick disagree the worse price wins, so climbing
  a tier can never make a backtest look better. **Nothing feeds it yet**
  — a tick file carries a best bid and a best ask, not a book.
- **`oq-book` — order book reconstruction as its own crate**, extracted
  from `oq-l2feed` so a matcher does not inherit a TLS stack to look at
  a price level. `oq-l2feed` re-exports at the old paths, so no call site
  moved.
- **A backtest can match against reconstructed depth.**
  `RunConfig::at_tier` selects L0, L1 or L2, and `run_observations`
  takes ticks and depth updates on one stream — one stream because two
  means the caller writes the merge, and a merge in the wrong order
  matches an order against a book from the future. The snapshot is its
  own arrival: an incremental stream says what changed, and a book with
  nothing to change refuses every update. `RunResult` gains `tier` and
  the three depth counts, because fills without a named matcher are
  numbers with no provenance and depth handed to a tier that ignores it
  is a run reporting a lower tier's answer under a higher tier's name.
  Two gaps stay open and are in the roadmap: converting an archive into
  that stream is the caller's loop, and depth is not in the journal, so
  an L2 run's journal replays its orders but not the book.
- **`Matcher` in `oq-core`** — the kernel holds a tier rather than being
  one. `State.engine` was an `L0Engine` by name, which is why nothing
  could reach L2. An enum rather than a trait: the tiers are a closed
  set, dispatch is on the hot path, and a snapshot has to name what it
  restored into. Throughput on a clean runner is unchanged, 14.64 →
  14.67 M ticks/s.
- **`limit_order` and `market_order` as free functions**, so a tier
  cannot construct an order differently from the tier below it.
- **`oq-data` reports stylized facts** for any tick file, so "does this
  behave like a market" is a command rather than a study. Four days of
  captured BTCUSDT hold three of four per day at excess kurtosis 8–11,
  against the generated fixtures' 0.03 / 0.07 / −0.05.

### Live trading

Nothing here has traded real money, and the entry triggers in
[Roadmap](docs/ROADMAP.md) §M3 say what would have to be true first.

- `oq-gateway` — execution adapters for two venues. Placement is
  three-state: accepted, rejected, and **unknown**, which is not an error
  because an error lets a caller `?` past the one case that must be
  handled. A conformance suite drives both adapters through the same
  cases and is itself checked against three deliberately-wrong adapters.
  `broker::IdScheme` composes client ids carrying a venue-issued referral
  code, kept separate from the prefix that answers *is this order mine*.
- `oq-risk` — pre-trade gate, kill switch, startup reconciliation.
  `VersionedLimits` records which limit moved and from what; a change
  that alters nothing does not advance the version.
- `oq-live` — process assembly, snapshot recovery, and the account kept
  by the **same kernel** the backtest uses, so there is one book
  implementation rather than two that agree until they do not. Reconciles
  against the venue at startup and refuses to run beside a position it
  was not told about, and runs a shadow backtest on the same events, so
  every run ends with the gap decomposed by cause and an explicitly
  unexplained residual. Fees come from the venue's own
  trade records via `Account::fees_charged`, whose default is `None` —
  an adapter that has not implemented it says so rather than answering
  zero, because zero is a measurement. Funding is still *unavailable*:
  the venue reports it on an endpoint no adapter reads. The residual
  carries what is missing and the report names it.
- Metrics are a **snapshot value** rendered in the line-oriented form
  collectors read, and alerts are judgements rather than notifications:
  nothing in this workspace sends anything anywhere.

**`Outcome::Unresolved` split from `Outcome::Refused`.** *Changes live
behaviour.* A submission that was sent and never answered was reported
to the strategy as `accepted = false` — telling it the order does not
exist when it may be resting, which invites the one action that turns
*maybe one order* into *certainly two*. Unanswered submissions are now
their own outcome and are not reported through `Strategy::on_placed` at
all. Console output and the end-of-run summary distinguish them. No
backtest result changes: a simulated matcher answers every submission.

**`Strategy::on_placed` added**, defaulted to empty and called from both
the backtest loop and the live host. A strategy that treats *I asked*
as *it is resting* believes it holds exposure it does not have.

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
- **A compressed archive can now be found, not only read.** An earlier
  change taught every tool to read `.oqcap.zst`; nothing taught them to
  find one, and `oq-ingest` filtered on an extension that a compressed
  file does not have — so a full day of capture reported "nothing to
  convert", which reads exactly like an empty directory. Naming now goes
  through `archive::stem`, beside `archive::read`.
- **`pull-capture-cron.sh`** — the half of the archive pull a schedule
  needs and a manual run does not. Blocked-by-lock exits `3` rather than
  sharing `1` with a transfer failure, because a first backfill outlasts
  any sensible interval and overlap is the normal state of an archive
  catching up; a monitor that cannot tell the two apart either raises on
  healthy overlap or stays quiet through real loss. Leaves
  `.last-success` / `.last-failure` in the archive root, because silence
  is the failure mode and a log nobody reads is silence.

### Examples and performance

- `oq-examples` — teaching examples on a seeded synthetic market, with
  `tests/golden.rs` pinning every number the documentation quotes. Plus
  a catalogue of six classic strategies — RSI, MACD, Bollinger,
  Donchian, grid, Dual Thrust — at their published parameters, untuned.
  Every one is decades old and traded by enough people that whatever
  edge it had is not waiting in a public repository; they are here so
  the framework can be learned by recognising something. Each documents
  where it breaks rather than claiming an edge.
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
