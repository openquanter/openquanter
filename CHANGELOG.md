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
- **`Event::Tick` names an instrument, and `kind::TICK_ON` (10) is
  added to the schema.** *Changes nothing for a single-instrument
  account.* An observation has to reach the holding it is of, and an
  account can hold several now. `None` means "the account's only one",
  which is what every journal written before this says — truthfully,
  since an account with one instrument had no other answer. A kernel
  holding several refuses it: guessing would mark one instrument's
  position at another's price, which is a wrong number rather than a
  missing one. **Behavioural delta:** none for an account with one
  holding. Two kinds for one variant because a named tick is 68 bytes
  and an unnamed one 64 — widening the payload in place would make an
  older reader decode a newer record as a tick with a wrong volume
  rather than refusing it. The first 64 bytes are byte-identical, so
  one decoder reads both and the kind decides whether the tail is
  there.
- **`Event::FundingCharged`, and `kind::FUNDING_CHARGED` (13), are added
  to the schema.** *Changes nothing that does not write one.* Funding
  booked at the amount the venue charged, as its ledger states it, where
  `Event::Funding` carries a rate and a mark for the kernel to work the
  amount out. The distinction is the one between a venue fill and a
  matched one: a live account has the venue's figure to the last place,
  and recomputing it would round the venue's mark to the contract's price
  grid, which the venue does not. The payload is 16 bytes, or 20 with an
  instrument.
- **`Event::Submit` names an instrument, and `kind::SUBMIT_ON` (11) is
  added to the schema.** *Changes nothing for a single-instrument
  account.* The same move as `TICK_ON`, for the same reason: an order
  that does not say which instrument it is for cannot be routed to the
  holding — or the shard — it belongs to. `None` means "the account's
  only one", which is what every journal written before it says. The
  older payload is a prefix of the newer, so one decoder reads both.
- **`Event::Funding` names an instrument, and `kind::FUNDING_ON` (12) is
  added to the schema.** *Changes results for an account holding
  several instruments.* A named settlement is charged to the holding
  whose rate it is, with that holding's contract, and to no other; an
  unnamed one with several holdings is refused, as an unnamed tick is,
  where it used to be charged to the first holding whichever instrument
  its rate belonged to. A flat holding now pays nothing whatever the
  other holdings hold, where before only an account flat everywhere
  paid nothing. **Behavioural delta:** none for an account with one
  holding; journals written before it contain no such record.
- **`Kernel::settle_unreported` books the fills a matcher still holds
  back when a run ends, and the backtest calls it.** *Changes results
  for L1 and L2 runs with a response latency.* A fill made inside the
  last response latency happened at the venue but had not yet been
  released to the account, so a run that ended inside that delay left
  it out of the result. **Behavioural delta:** such a run now includes
  those fills in its fills and equity. L0 holds nothing back and is
  unchanged. Not an event: nothing in a journal replays "the run ended".
- **L1 queueing is symmetric, and a zero-latency order queues behind the
  multiple.** *Changes L1 results.* A window with no trades has a high
  and a low of zero, and read as prices every buy looked passed through
  and skipped its queue while no sell ever did — optimism on one side
  only. Such a window now lets no order through its queue. And an order
  submitted with no entry latency was given a volume-multiple queue
  sized from no volume at all, a queue of zero while the report still
  claimed the multiple; it is now sized from the last observation.
  **Behavioural delta:** L1 buys resting through trade-less windows fill
  later or not at all, and zero-latency L1 orders wait behind a real
  queue. L0 and L2's measured queue are unaffected.
- **A tick out of order no longer ends a backtest or settles funding
  twice.** *Changes results for data with a backwards timestamp.*
  `FundingSchedule::between` holds nothing for an interval that runs
  backwards, where it used to slice out of bounds and end the run, and
  the run's funding mark only moves forward, so the stretch between an
  earlier and a later tick is not settled a second time. **Behavioural
  delta:** none for data whose timestamps never go backwards.
- **`Intent::Limit` and `Intent::Market` name an instrument.** An
  implicit "whichever instrument this callback was about" makes the
  same line of code place different orders depending on where it ran,
  with nothing at the call site showing it. `Context` builds intents
  for the instrument it is about, so a single-instrument strategy never
  writes one down. An order for an instrument the run does not hold is
  refused, reported through `on_placed`, and counted — never sent to
  the only holding there is, which would fill an order the strategy did
  not place on a book it was not looking at.
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
- **`Record::Submitted` gains a trailing `leg` field, and
  `Belief` reconstructs position leg by leg.** *Changes what a
  reconstruction reports, not what a run does.* A hedged account was
  rebuilt as one net number, so long 0.004 and short 0.008 read back as
  short 0.004 and every hedged reconciliation disagreed with the venue.
  The net could not simply be split: on a hedged account the reduce-only
  flag is dropped (the venue refuses it there), so without the leg a
  close of the short and an open of the long look the same. Journals
  written before the field still decode, with the leg unknown; a hedged
  fill whose leg is unknown is counted as undecodable rather than
  guessed. Record comparison also stops calling a float's rendering a
  difference (`83794.9` against `83794.90000000001`) and treats a
  one-way `BOTH` leg as the direction it holds.
- **`Record::Operator` (live journal kind 10) added, with a local
  control port.** *Adds a record; changes no existing one.* A process
  started under systemd with a runtime directory listens on a Unix
  socket there for `status`, `orders`, `metrics`, `attribution`, `halt`,
  `shutdown` and `resume`, acted on inside the loop like any other event. Peers are
  checked by the uid the kernel reports; there is no port without a
  runtime directory, never one in `/tmp`. Every state-changing command is
  journalled with its reason, the authenticated origin and what came of
  it. `resume` is off unless the process is started with
  `--control-allow-resume`, and refused while the last position check
  disagreed or the journal cannot record. An operator's clean shutdown
  exits with status 98 so a supervisor can be told not to restart it.
- **`Record::Cancelled` (live journal kind 9) added, and
  `Belief::from_journal` subtracts it.** *Changes what a reconstruction
  reports, not what a run does.* The live journal recorded a
  submission, its outcome and its fills, and nothing at all when the
  venue withdrew an order. A reconstruction could therefore only read
  an accepted, unfilled, never-ending order as resting. On a real
  deployment that came out as **many times the resting orders the
  account actually held**, the whole difference being cancellations
  the journal never had a record type for. `oq-belief` is what step 5
  of [the cutover
  playbook](docs/CUTOVER.md) compares against, so the error was
  load-bearing. **Behavioural delta:** none for any run — the record is
  written where the venue confirms the withdrawal and nothing reads it
  back inside the loop. A journal replayed by `oq-belief` or
  `oq-replay` now reports resting orders that a venue would recognise.
  Additive on disk: journals written before it contain no such record
  and replay unchanged, and a journal written *with* it is skipped
  frame-by-frame by an earlier build rather than misread, which is what
  the explicit kind numbering is for.


- **`Event::VenueFill` carries the fee the venue stated.** *Appended to
  the record's payload, not inserted into it:* a journal written before
  this is 55 bytes and still decodes, with `Fee::Unsaid` — which is the
  truth about a record that never carried one. The variant is no longer
  a tuple, so a matcher over it changes shape.

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
  — a tick file carries a best bid and a best ask, not a book. *(Since
  superseded: `run_observations` below feeds it, and `oq-tiers` does so
  from a captured archive.)*
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
  an L2 run's journal replays its orders but not the book. *(Since
  closed: `oq_ingest::fold_into_observations` converts an archive, and
  `Event::Depth` above puts the book in the journal.)*
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

- `oq-gateway` — execution adapters for two venues *(eight since:
  Binance, OKX, Aster, Kraken Futures, Deribit, Hyperliquid, Backpack,
  Bitget — see [Venues](docs/VENUES.md))*. Placement is
  three-state: accepted, rejected, and **unknown**, which is not an error
  because an error lets a caller `?` past the one case that must be
  handled. A conformance suite drives both adapters through the same
  cases and is itself checked against three deliberately-wrong adapters
  *(it drives all seven adapters now; Aster shares Binance's)*.
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
- **A live run leaves run files and a tick file.** Beside its journal,
  every fifteen minutes and at exit, it writes `<stem>.live.run` and
  `<stem>.model.run` — the venue's fills and the shadow's, under one
  identity, in the format `oq-parity` reads — and `<stem>.oqtk`, the
  observations, for `oq-parity markout`. Each is written aside and
  renamed, so a reader never sees half of one. The control port answers
  `attribution` from the shadow's evidence mid-run.
- **The shadow sees every order as it was sent, and every withdrawal.**
  *Changes what the shadow reports.* Only the tick path told the shadow
  about orders, and it sent every one without a price, so a resting
  ladder reached the model as market orders and filled at once; orders
  withdrawn by a halt or by the trader itself stayed resting in the
  model and could fill later. Its fills — and every divergence and
  attribution computed from them — described orders the venue never
  held. Every outcome now passes through one function that tells the
  books and the shadow alike, including orders placed from fills and
  orders the venue ended. No backtest changes.
- **The control port's `status` says more:** the contract's
  `price_scale` and `qty_scale`, so a reader can show orders in the
  venue's units; a `pnl` object (realized, fees, funding, net and equity
  since the run started); and a `limits` object with the risk limits the
  run trades under.
- **`oq-recon --watch --latest FILE`** keeps the newest reading in
  `--record`'s format, rewritten atomically after every read, so a
  journal can be reconciled against the venue without anyone pasting a
  record.
- **Hyperliquid's order status reads the order, not its envelope.**
  *Changes what the Hyperliquid adapter reports.* The `orderStatus`
  answer nests the order inside an envelope with its own `status`, and
  read flat it gave every order the state `order`; it also reported the
  quantity still open as the quantity filled, so a full fill read as
  nothing filled. The order's own state and `origSz` less `sz` are read
  now, and only `unknownOid` is taken as "no such order" — anything else
  unreadable is an error rather than a licence to resend. The
  conformance suite drives Hyperliquid, and it now checks the state and
  filled quantity a status answer gives rather than only that the state
  is not empty, which is why it had passed the flat reading.

- **A live run measures funding on both sides.** *Changes the live
  run's P&L and what attribution reports.* At every settlement the run
  crosses, both books' legs are recorded; once the venue publishes the
  settlement, its funding ledger lines are booked to the live books as
  they stand, and the model's legs are charged at the settlement's rate
  and mark by the venue's own arithmetic — quantity times mark times
  rate, truncated toward zero at eight places, the mark unrounded. The
  same arithmetic on the live legs must reproduce the venue's lines
  exactly; checked against a testnet account's ledger before release, it
  reproduced all nine lines across five settlements, four of which
  rounding would have got wrong. When the check fails, or a settlement
  goes unanswered, funding is unavailable for the run with that reason.
  Each settlement is journalled (record kind 11, `funding`). The run's
  P&L now includes funding, which it claimed to and did not. Venues
  whose adapters do not read funding report it unavailable, as before.
- **Attribution says why funding is unavailable.** `Evidence` gains
  `funding_unavailable`, rendered in place of "no funding was recorded",
  which was wrong for a settlement still waiting and for a check that
  failed.
- **`oq-recon --funding SINCE_MS`** lists the settlements since then: the
  venue's rate and mark, and the account's funding ledger lines. Read
  only.
- **The shadow's net position adds its legs.** It subtracted the short
  leg, which is held negative, as the books once did before it was
  caught there; nothing in the live loop read it, so no report changed.

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


- **A live run books the commission the venue charged.** The books
  charge from a fee schedule and a live run is built without one, so
  `fees` was zero for every live run — a number reported beside realized
  and funding as though it had been measured. The user data stream
  states the commission and the asset on every fill; `fill_of` books it,
  and a fee in another asset — or one that is not a decimal amount — is
  `Fee::Unreadable`, which marks the run's total incomplete rather than
  falling through to a schedule of zero.
- **The fee a run reports is checked against the venue's ledger.** The
  books' total and a final read of the venue's own trade records are
  compared where both are final — at the end of the run — and a
  disagreement leaves the attribution's fee component unavailable
  rather than publishing either number. Funding has been checked this
  way at every settlement since it was added.
- **The journal is durable.** `EveryRecordNoFsync` survives a process
  crash and not a machine one, and the failure the record-before-send
  ordering exists to rule out is a live order the journal has never
  heard of.
- **A book update's sequence number is placed or refused.** OKX's
  `prevSeqId` reached `p + 1` unchecked, and this workspace turns
  overflow checks on in release: a message could stop the process, and
  the shutdown that withdraws resting orders is a call after the loop,
  not a `Drop`. A `seqId` that is not a number the venue could have sent
  is refused rather than clamped to zero, which read as the chain
  starting over.
- **A venue figure of `NaN` is not agreement.** `NaN` compares false
  against everything, so every check in `reconcile` passed it.
- **A fill's side is buy or sell, or it is unbookable.** The order
  stream's side field is optional upstream, and the mapping read
  anything else as a sell — a short on a flat account, caught by the
  next reconcile and wrong until then.
- **A prefix cannot swallow another process's orders.** Where the venue
  puts nothing between the prefix and the sequence, `oq` and `oq2` are
  different interlock locks and the same ownership check: the `oq`
  process counted the other's orders against its limits and withdrew
  them on shutdown. Refused at startup now.
- **The belief knows when the run began.** A reader comparing a journal
  against a venue reading has to know which run the reading is of;
  `to_record` stamps a time on the journal's final state rather than
  rewinding to one.

### Fees

Maker/taker trading fees are charged in the kernel. A maker rate may be
negative, because rebates exist and a model that floors at zero cannot
represent the strategies that live on them. **Fees default to zero and
must be set deliberately** — the examples do not set them, so every
number the documentation quotes is gross of costs.


**The venue's own commission is what a live run charges.** The live path
is built without a schedule, so the kernel's figure was zero for all of
them; the commission on each fill is read to the last place — `Cash`,
not a float rounded through `as i64` — and summed as integers.

### Capture

- `oq-l2feed` — verbatim record framing, UTC-day rotation, sealing with
  content-hashed manifests. Survives the day boundary; flushes on a timer
  rather than only on a record count.
- `oq-capture` — live client for Binance perpetual streams *(and OKX
  swaps since, selected by `--venue`)*, capturing the streams the venue
  actually serves (some accept a subscription and then
  send nothing; see [Capture Format](docs/CAPTURE-FORMAT.md)).
- A keepalive tick hands control back to the capture loop instead of
  waiting inside the source. A source now says whether its silence is a
  disconnect; a connection with an answered keepalive says no. Waiting
  inside the read kept the connection alive and the loop asleep, so a
  quiet stream could not see its own shutdown flag — measured as three
  minutes of ignoring SIGTERM on a live capture host.
- A control record is flushed when it is written. The capture loop
  flushes when a message arrives, so on a stream that received none the
  session marker sat in the buffer indefinitely and the file on disk
  said the stream had never started.
- Capture fixes found by verifying a live archive against its manifests:
  a stream whose silence is ordinary now carries its own keepalive
  instead of being reconnected every read timeout; a gap marker reports
  the outage it measured rather than how long the connection had been
  up; a polled payload's own timestamp is read as its event time; and a
  manifest reports `null`, not the epoch, where a file carries no such
  timestamp. The last is a **manifest schema change** — readers that
  parsed these four fields as numbers must accept `null`.
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


- **An hour of an archive is read once.** A capture written before the
  archive step has both `<name>.oqcap` and its `.zst`, and the original
  is not always removed: `load_hour` read both and counted every record
  twice, which reads as a market that got busier.

### Examples and performance

- `oq-examples` — teaching examples on a seeded synthetic market, with
  `tests/golden.rs` pinning every number the documentation quotes. Plus
  a catalogue of six classic strategies — RSI, MACD, Bollinger,
  Donchian, grid, Dual Thrust — at their published parameters, untuned.
  Every one is decades old and traded by enough people that whatever
  edge it had is not waiting in a public repository; they are here so
  the framework can be learned by recognising something. Each documents
  where it breaks rather than claiming an edge.
- **A sweep's result as a file.** `oq_backtest::sweep_file` renders a
  sweep — every configuration's outcome, the deflated Sharpe ratio and
  PBO beside them, the refusals, and the reason for any statistic that
  could not be computed — in a line-oriented format headed
  `openquanter-sweep 1`, and parses it back. `sweep_100 --out FILE`
  writes one. See [Sweep Format](docs/SWEEP-FORMAT.md).
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
