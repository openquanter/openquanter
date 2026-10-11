# The live path

[English](LIVE-PATH.md) · [中文](LIVE-PATH.zh-CN.md)

How `oq-gateway` connects a venue account to the deterministic core, and
why it is shaped this way. Written before the code, because the survey
below found that every project in this space built the order path first
and the reconciliation afterwards, and every one of them has the same
class of bug as a result.

## The claim this rests on

The core is `apply(State, Event) -> Outputs` with no clock, no RNG, no
I/O. Backtest and live therefore differ only in who produces events. That
is the premise the whole design leans on, and it is already paid for: the
same core reproduces a two-year reference run fill for fill.

So the live path is not a trading system. It is **event production,
durability, and the one thing a file can never test: whether the model of
the account matches the account.**

## What the survey found

Four systems examined in detail (Hummingbot, hftbacktest, Barter-rs, plus
the FIX ancestry all three descend from). Two conclusions shaped this
document more than anything else.

**First: "not found" is where the money goes.** Hummingbot's founding
incident is worth quoting because it is what a naive reconciler produces
on its own:

> it's possible for Binance API to return an order not found error…
> Hummingbot would then create another order to replace it. Because the
> old order is still live on Binance, the new order is actually a
> duplicate order… these duplicate orders would accumulate and deplete
> the trader's available balance.

The mechanism is that a *generic exception handler* concluded an order
did not exist. Barter reaches the same failure by an independent route —
it synthesises a `Timeout` inactive-state event for an order that may
well be live, then removes it. Two codebases, no shared code, same bug.

**Second: nobody in the survey both journals live events and replays
them.** hftbacktest has a fully deterministic backtest and *zero*
journaling live. Barter has a sequence-checked audit stream that is never
persisted. NautilusTrader ships a real event store — redb-backed,
per-entry hashes, durability-immediate commits — and its own
documentation says it is *not* the live restart path: *"Live restart
still uses snapshot-plus-reconcile."* Determinism is claimed widely and
implemented in the backtest only.

A third finding sets the expectation for how this goes. Nautilus's
release history carries **195 reconciliation entries** across roughly a
hundred versions, and the pattern is consistent: in-flight checks
produced re-query storms and premature terminalisation; inferred fills
produced phantom positions, double-counting, zero-price fills and
non-deterministic ids; continuous open-order checks produced false
not-found resolutions and phantom cancels. **Each capability added to
close a gap opened a new bug class.** Every configuration knob in that
project is a scar. This is not a system that gets designed correctly once
and then works.

## Decisions

### L1 — Reconciliation exists before order placement, not after

The read path ships first and runs against a live account for weeks
before anything can trade. This is not caution for its own sake: a
reconciler is the only component that produces evidence about the parts
a backtest cannot reach, and it produces that evidence at zero risk.

It also inverts the failure the survey found. Building orders first means
discovering the reconciliation bugs with money on them, which is exactly
the history above.

### L2 — Three answers to "what is truth", and ours

The survey found three incompatible strategies:

| | Approach | What it costs |
|---|---|---|
| hftbacktest | Cancel everything on connect, refetch position | Cannot mismatch. Destroys the book on every blip |
| Hummingbot | Poll the orders you know about | Cheap. Orphans are undetectable, permanently |
| Barter | Merge a venue snapshot by client id | Clean. Silently wrong — its own doc says the snapshot *replaces*, the code *merges* |

A fourth exists and is better than all three. Roq marks every known
order stale, overwrites with what the venue reports, and then emits
whatever is *still* stale — by construction an order the venue declined
to report, therefore terminal but unknowable. It is emitted **exactly
once**, which makes it an edge-triggered alarm rather than a level
somebody has to remember to poll, and the API refuses to fabricate a
terminal state for it. Their own documentation is honest about why:

> Since we can't know if an order was completed or canceled during
> disconnect, it is necessary to use other means to synchronize e.g.
> positions… A final solution might be to require human intervention
> when seeing stale order updates.

That last sentence is the correct answer, and the three projects above
all reached for an automatic one instead.

Ours is Roq's: **mark stale, overwrite from the venue, and treat the
residue as a fact requiring resolution rather than a state to invent.**
Anything present locally and absent at the venue is surfaced — never
silently dropped, which is Barter's ghost-order leak, and never assumed
still live, which is Hummingbot's orphan.

A snapshot also needs a terminator. Roq brackets the download with
`DownloadBegin`/`DownloadEnd`, which is FIX's `LastRptRequested` under
another name: a client must know a snapshot is *complete* before it
starts diffing, or it will diff a partial answer and conclude that
everything not yet received has vanished.

Position in particular is **fetched, never inferred**. Barter has no
position field in its snapshot at all, so a missed fill is invisible
forever. Folding fills is how you *predict* position; asking the venue is
how you *know* it, and the difference between the two is the signal.

### L3 — A mismatch halts. It is never repaired by invention

At startup, unconditionally: reconciliation failure means the strategies
do not start. NautilusTrader does exactly this and is explicit about it —
the kernel returns before `trader.start()`, and a release note records
fixing a path where startup *continued* after a reconciliation failure.
It is also the rule the 1.x system already runs under.

At runtime the two strongest projects disagree, and the disagreement is
worth stating rather than resolving by preference.

**NautilusTrader repairs.** On a position gap it fabricates a fill — or a
whole synthetic FILLED order — priced by a four-tier hierarchy that
begins with solving for the price that reproduces the venue's average and
ends, if no price data exists at all, with a market order. Its own code
comments the cost: *"there may now be some information loss if multiple
fills occurred to reach the reported state."* After a retry budget it
gives up and logs an error, and then **keeps trading on state it has just
declared unreconcilable.**

**Roq refuses.** It emits the residue as stale, exactly once, and says
plainly that human intervention may be the only correct response.

Ours follows Roq, for a reason specific to what runs here: a martingale's
ladder is anchored on the position's average entry. A position that is
wrong by any amount does not produce a slightly wrong ladder — it
produces rungs at the wrong prices and a take-profit that will not be
reached. Inventing a fill to close the gap invents that anchor.

So: transient order-level differences resolve by re-query, and a
**persistent position divergence halts** rather than being abandoned to
keep trading. The middle option Nautilus takes — log an error, continue —
is the one this cannot use.

Re-query means asking the venue for its **own records** of the orders
this process still believes are resting — each order's status and its
trades, with the venue's trade ids — and booking whatever the stream
never delivered through the same path a streamed report takes. That is
not a repair: nothing is priced, nothing is invented, and the trade id
deduplicates anything the stream did deliver. It exists because a stream
that drops does not replay what it missed; a take-profit that fills
during a reconnect is otherwise a difference nothing can explain, and
the process halts over a fill two requests would have found. A
difference the venue's records do not explain still halts.

A halt withdraws the orders that would **add** to the position and keeps
the ones that would reduce it. Withdrawing nothing leaves a ladder filling
under a process that can no longer see it; withdrawing everything leaves
the position with no exit.

The reason to halt rather than self-correct at all is that a reconciler
which silently repairs cannot tell you it has been repairing the same
discrepancy every ten seconds for a week. The discrepancy is the finding.
Hummingbot's tracker shows the shape of the alternative: **zero issues
exist for "balance mismatch" or "position mismatch", not because it never
happened but because nothing detects it, so there is no vocabulary for
it.**

### L4 — Client order ids must be reconstructible

Binance's uniqueness rule for `newClientOrderId` is *"a unique id among
open orders"* — scoped to the currently-open set, not the account's
lifetime. Once an order closes its id can be reused and will not be
rejected. **It is not an idempotency token.**

All three surveyed projects generate ids from wall clock and process id.
That is anti-idempotent by construction: after a crash you cannot ask the
venue "did my order land?", because you cannot regenerate the id you
would be asking about.

Ours is composed by `broker::IdScheme::compose`: an optional
venue-issued broker code, the process's ownership prefix, and a sequence
number. The sequence comes from a range reserved durably before the
session can send (`oq-live`'s `order_ids.rs`), so a restart never
assigns an id twice — a crash burns unused ids instead — and the id is
journaled before the order is sent (L5), so recovery reads it back and
can ask the venue about it. Uniqueness across reuse therefore comes from
our own records, because the venue does not provide it.

### L5 — Intent is journaled before it is sent

The journal already sits in front of the core (`D2`). The order path
extends that to the network boundary: the intent to send is durable
*before* the socket write.

Hummingbot snapshots state inside order *event* handlers, so the window
between POST and acknowledgement — the only window that matters — is
unprotected. Crash there and the order is live at the venue with no local
record, permanently. That is opportunistic snapshotting; this is a
write-ahead log, and the difference is which side of the network call the
write happens on.

Durable here means fsynced. Every decision — a placement, a withdrawal, a
fill, a reconciliation — is on the device before the session acts on it.
Observations (one tick per aggregation window, the waiting heartbeat)
reach the OS at once and the device with the next decision's fsync: a
power cut can cost a replay its last few ticks, never an order.

### L6 — Terminality requires both channels to agree

REST and the user stream both report that an order is finished, with no
ordering guarantee between them. Erasing on the first report is how a
late message from the other channel resurrects a dead order or drops a
live one.

An order is erased when **both** channels have confirmed it, the strategy
is told exactly once, and a single-confirmed tombstone is collected after
a timeout. hftbacktest is the only surveyed project that does this, and
every project has the problem.

### L7 — State never regresses

Every inbound update carries a rank and a timestamp, and an update that
would move the machine backwards is dropped rather than applied.

FIX defined an explicit precedence over `OrdStatus` in the 1990s for
exactly this. Hummingbot stores `last_update_timestamp` and never reads
it: `current_state = order_update.new_state`, unconditionally. A late
message therefore regresses its state machine.

### L8 — Failures are three-valued, and "not found" needs positive evidence

The strongest single idea in the survey, from NautilusTrader:

```rust
pub enum CommandFailure {
    /// A deterministic local failure proves the command was never transmitted.
    /// A terminal rejection event is valid for this evidence.
    NotSent(String),
    /// The venue outcome is undefined; the command may still have been applied.
    /// A terminal event is never valid for this evidence.
    Ambiguous(String),
    /// The venue explicitly declared the command rejected.
    VenueRejected(String),
}
```

with the distinction that makes it work: *"This axis is independent of
retryability. Retryability answers whether to send the request again;
this answers whether the venue may already have acted on the first
attempt. An error can be both, either, or neither."*

Transport errors, timeouts, disconnects, retry exhaustion and
post-transmission parse failures are all `Ambiguous`. The default for
anything unclassified is `Ambiguous`, and an ambiguous command stays in
flight until a stream update, a query, or reconciliation resolves it —
**a terminal event is never manufactured from it.**

Adopted whole, including their enforcement mechanism: it is a written
per-adapter conformance requirement, not a convention. An adapter that
emits a rejection from a timeout fails its test.

"Not found" therefore comes only from a typed venue response. Never from
a generic exception, never from a local timeout, and never from a count
of transient failures.

Hummingbot built the typed classifier and then wired it into the wrong
path: any exception on an active order routes to `process_order_not_found`,
so four transient HTTP 500s declare a live, working order FAILED to the
strategy. Its counter is cumulative rather than consecutive, so a
long-lived order accumulates false strikes across hours.

Failures that are not positively identified as "the venue says this order
does not exist" are transient by default. Counting is consecutive and
resets on any success.

### L9 — An order is quarantined, never forgotten

When tracking must stop, the order moves to a quarantine that keeps
polling it and keeps accepting fills for it, while the strategy is told
it is gone.

This is Hummingbot's one unambiguously good idea, and it took them three
and a half years and three attempts to arrive at. Adopted directly.
"What the strategy is told" and "what the system stops watching" are
different questions and must not share an answer.

*Status: adopted, not built.* No quarantine exists in the code. The
`unanswered` queue in `oq-live` is a different thing — it follows an
order whose submission got no answer — and does not keep watching an
order once tracking stops.

### L10 — Fills are absolute, not accumulated

Where the venue reports cumulative filled quantity, that is the number
kept. Per-fill deltas are used for fee and price attribution only.

An accumulator needs a per-trade-id dedup dictionary to survive
redelivery; a snapshot is idempotent under redelivery by construction.
Binance's `ORDER_TRADE_UPDATE` carries the cumulative figure, so there is
no reason to reach for the fragile form. Trade ids are still deduped —
redelivery after reconnect is normal, and Barter double-counts both
position and P&L for want of this.

### L10b — Dedup keys are account-scoped, and marked only after the fact

A fill is identified by `(account, instrument, trade_id)`, not by
`trade_id` alone. Venues reuse trade ids across accounts, and Nautilus
carries a release note for exactly that conflation.

The dedup mark is committed **after** the fill reaches the ledger, not
when it is first seen. Marking on receipt looks equivalent and is not: if
the fill is then rejected downstream, the mark permanently suppresses a
fill that was never applied. Nautilus fixed this twice under separate
issue numbers.

### L11 — Disconnected means not trading

While a channel is down the strategy does not get to act on stale state.
Barter generates orders regardless of `connectivity == Reconnecting`, and
its only disconnect hook is a no-op by default.

### L12 — Rate limiting is closed-loop

`X-MBX-USED-WEIGHT` is read, 429 is honoured with backoff, and 418 is
treated as the incident it is: Binance escalates repeat offenders to IP
bans of *"2 minutes to 3 days"*. Hummingbot's throttler ignores all three
headers; the other two projects have no rate limiting at all.

*Status: partly built.* 429 is honoured with `Retry-After` and 418 with a
backoff. `X-MBX-USED-WEIGHT-1M` is read from every response, refusals
included, and shown in `oq-trade`'s `status` answer as `request_weight`
— on Binance and Aster alike, since both are served from one table. It
is not yet used to throttle: the loop is open, and the 429 is still what
stops requests.

### L13 — The clock is asymmetric, so run slow

Binance accepts a request when

```
timestamp < serverTime + 1000 && serverTime - timestamp <= recvWindow
```

A *fast* local clock gets 1000 ms of tolerance no matter what
`recvWindow` says; a slow one gets the full window. The offset estimate
is therefore biased to sit slightly behind the venue, and it is computed
RTT-midpoint on a monotonic base so an NTP step cannot corrupt a
signature mid-flight.

## Shape

```
        venue REST  ─┐                    ┌─ read: account, positions,
        user stream ─┤                    │        open orders, trades
                     ▼                    ▼
              oq-gateway ──────────► reconciler ──► FATAL on mismatch
                     │                    ▲
        journal ◄────┤ (intent, before the socket)
                     ▼                    │
              sequencer ──► core ──► outputs ──► gateway (send)
                                      │
                                  observers read the journal, never the core
```

Read and write live in one crate. A separate read-only crate was
considered — the guarantee "this code cannot place an order" is stronger
when it is structural — and rejected: signing, error classification and
the venue's types would exist twice, and two implementations of request
signing is a worse risk than one implementation with a reviewed boundary.
The boundary is at the type level instead: reading needs no capability to
trade, and the type that can trade is constructed explicitly.

## How this gets tested

Reconciliation is live-only in every project surveyed — *"backtesting
controls both sides"* — which means the entire mechanism above is
untested by the thing we spent months validating. Two answers, both
taken from the survey:

**Property tests that shuffle, duplicate and drop.** Nautilus generates
arbitrary sequences of fills and reports, permutes them, and asserts four
invariants hold regardless of order: final quantity matches the venue
within precision, average price within tolerance, generated fills
preserve unrealised P&L, and synthetic ids are deterministic across
replays. Their repository carries eight checked-in shrunk failing seeds —
real bugs the fuzzer found, pinned permanently.

**A written conformance suite per adapter.** Nautilus's is 2,279 lines
and roughly a hundred cases, each specifying prerequisite, action,
expected event sequence and pass criteria. The ambiguity cases are the
ones that matter: an adapter that turns a timeout into a rejection fails.

Both are cheaper than the alternative, which is discovering the same
things from the release history above.

## What the first live run observed

The reader was pointed at a live account for eight hours, placing
nothing. Six observations are recorded here because each bears on a
decision that until then rested on someone else's release notes.

**Seven orders left the book in the same millisecond. One had filled;
six had been cancelled.** The position closed, the resting ladder behind
it was withdrawn, and the venue reported zero executed quantity for all
seven — there is no way to tell them apart from a snapshot. Labelling
them "filled" is wrong six times. Labelling them "cancelled" is wrong
once, and that once is the only one that moved money: it misses the fill
and reports a position that no longer exists as still open, which is the
state L3 exists to refuse. An earlier instant in the same run had two
orders leave together, one filled and one cancelled and replaced, so
both guesses were wrong in the same breath. L2 and L6 say to report the
departure and decline to name a cause. That was borrowed caution; it is
now the only answer available that is not a fabrication.

**A fill is invisible to polling. Only its consequence is.** The order
that filled did so between two reads. It was never observed partially
filled and never observed terminal — resting, then gone, with the
position changed. When it happened, at what price, in how many pieces:
all of that existed only in the stream. L6 argued this; the run
demonstrates it.

**A partial read is routine, not exotic.** Four of one hundred and
twenty-four reads came back missing a part, each missing a different
one. The next two thousand three hundred were whole. That the failures
were an episode rather than a rate is itself the finding: a standing
three percent would justify a backoff the evidence does not support.
None of the four were diffed, so none reported orders vanishing that had
not.

**The balance moved while nothing else did.** At a funding boundary,
with no fill, no order change and no position change, the account was
charged. Reconciliation that compares only positions and orders is blind
to every event of this shape, and that shape covers every fee,
settlement and transfer the strategy did not cause.

**The venue's average entry matched the computed blend exactly** — to
the last bit of the float, on a position that had just doubled. That
computation had been checked against another implementation of the same
intent, never against a venue.

**The gate reported success on a read that never happened.** Single-shot
mode exited zero when a read came back partial, and single-shot is the
mode a startup gate uses: it would have reported agreement about an
account it had not seen. This is L8's shape — the third outcome that is
neither pass nor fail — surfacing in a place L8 had not been applied to.
It exits distinctly now. The defect came from running the thing, not
from reading it.

## Order

1. **Read path + reconciler.** Runs against a live account, changes
   nothing, reports mismatches. Weeks of this before anything else.
2. **User stream → events.** Still no order placement: the stream is
   consumed, journaled, and folded into a model that the reconciler keeps
   checking against the venue. A missed or duplicated fill shows up as a
   position divergence, which is exactly the test that matters.
3. **Order path.** Journal-before-send, reconstructible ids, dedup store,
   two-channel terminality.
4. **Risk gate.** Planned inside the core (`D7`). As built it is
   `oq-risk`, which the live session consults before every send and
   whose passing check returns a permit carrying the order it approved.
   It is not in the core, and a backtest does not link it, so its
   decisions are not exercised by backtests.
5. **Assembly and recovery.** Snapshot restore, reconciliation on
   startup, graceful restart.

Each step is testable before the next exists. Step 1 in particular is
worth running for its own sake even if nothing after it is ever built: it
is an independent check on the system currently trading.

## What this does not cover

Balances. Nautilus reconciles orders, fills and positions and leaves
balances to a separate path, and the same split applies here for now: a
balance divergence with matching positions is a fee or funding
accounting question, not an execution one. The models are no longer the
obstacle — `Fees` carries a maker and a taker rate and lets the maker
side be negative, and funding is settled into the balance and tracked
apart from it, so the local side can say what it thinks it was charged.
What is missing is the comparison, and the reason it has not been built
is that the venue's balance also moves for reasons no execution model
sees: transfers, cross-collateral, sub-account movement. A divergence
that can be explained by any of those is not evidence about execution,
which is why this reconciles the things that are.

Venues other than one — no longer. The design was written against
Binance USDT-M because that is what ran. OKX now has an account reader
and an event stream too, and `oq-trade` trades Binance, Aster and OKX;
the execution conformance suite drives all seven adapters through the same
cases. The second venue did disagree about what an order report
contains — OKX answers a refusal inside a 200 — which is what the suite
exists to catch.

This was written as a design before it was a result. The section above
on Nautilus's 195 reconciliation entries is still the honest
expectation: the first version of this will be wrong in ways the survey
cannot predict. It is now also exercised end to end without a venue:
`oq_live::sim` runs the whole live process against a seeded simulated
venue that can be told to misbehave, and `tests/dst.rs` holds it to the
same seed producing the same run.

## Recorded ledger evidence

The live record namespace adds two kinds without changing legacy payloads:

| Kind | Payload |
|---|---|
| `EXECUTION` (12) | Length-prefixed client id, u32 little-endian core-payload length, then the canonical `Event::VenueFill` payload: full fill clocks, instrument/order/trade ids, side, offset, liquidity and fee evidence |
| `LEDGER_START` (13) | Observation time and instrument id (i64 LE), price/quantity scales (u8), contract size, price tick, quantity step, minimum notional, cash-per-tick-lot and starting cash (i64 LE), then mode (0 one-way, 1 hedge) |

These are **live record kinds**, not the core event discriminants. The
execution contains the core encoding but does not reuse its outer kind.
Instrument economics must agree with the recorded cash multiplier. New
producers record initial positions even when the observed list is empty;
missing evidence is not flat. `Session::record_fill` remains the legacy,
fee-less interface; `record_execution` and `record_ledger_start` supply the
accounting path. An absent/unreadable commission cannot fall through to a
model's zero fee, whereas `Fee::Reported(Cash::ZERO)` is observed zero.

```bash
oq-replay session.oqj --ledger
oq-replay session.oqj --ledger --require-coverage
```

The ledger reader selects the latest session and uses authoritative live
books. It refuses missing context/adoption, legacy fee-less executions,
unknown records, duplicate economic identities and torn tails. It prints
recorded balance, realized P&L, fees, funding and legs. Exit 0 means that
replay succeeded, not that all venue cash flows were captured; unknown fees
return 2, and `--require-coverage` also returns 2 without explicit funding
coverage. An observed funding amount is not evidence that no settlement
was missed. Strategy decisions, margin projections, transfers and insurance
adjustments need their own evidence. Old files require separately identified
enrichment; never rewrite them to erase missing provenance.

Live books do not synthesize liquidation fills or insurance transfers from
a model mark. They keep confirmed inventory until the account stream closes
it. The shadow/risk path may project liquidation, but that projection is
not a venue event.

### Opening inventory in the shadow

The runner starts books and shadow from the same observed wallet, position
mode, sizes and entry bases. Hedge mode keeps both legs, even when net is
zero; one-way mode rejects two opening legs. `State::with_initial_legs`
seeds the cost basis without manufacturing fills, commissions, realized
P&L or liquidation against a mark not yet observed. The shadow's fill log
therefore contains only model executions after startup. Initial basis has
only the precision supplied by the venue snapshot. This boundary condition
does not reconstruct historical economics or strategy warm-up state, and
cannot establish restart parity or funding/history coverage by itself.

Funding settlements awaiting a rate or ledger are retained after the
30-minute overdue boundary. `FundingWatch::unresolved` exposes their
timestamps, and `asking_since` continues to include the oldest one. A late
answer can be booked once. Within the watch, stable funding ledger IDs
deduplicate overlapping history; conflicting identities or assigning one
ID to two settlement windows keep attribution unavailable. These checks do
not establish complete history coverage, persist pending state across a
restart or rediscover settlements that were never scheduled.

When the next settlement cannot be discovered, the host retries that query
on the same 60-second funding poll, including when there are no pending
settlements. `FundingWatch::schedule_gaps` keeps the injected-time interval
from discovery loss until a future schedule is obtained; it never accepts
a stale next time or extrapolates the venue's interval. Future settlements
resume, but attribution remains unavailable for a run with an unknown
schedule interval. These intervals currently live in the watch; durable
history/coverage records and reconstruction of missed settlements remain
separate recovery work.

The Binance funding ledger and rate readers page through inclusive time
boundaries, deduplicate stable identities and reject conflicting evidence.
They fail rather than return a prefix when a full page cannot progress,
a later request fails or the 50-page bound is exhausted. The shared-time
boundary is never skipped: if a full millisecond cannot be enumerated by
this cursor, it remains unresolved. Reading a terminal short page does not
prove that the venue retains every earlier row or establish persisted
account-history coverage.

New funding settlements use additive `FUNDING_EVIDENCE` (14). Its payload
starts with the unchanged `FUNDING` fields (receive ns, settlement ms,
length-prefixed rate/mark, venue/model cash and a 0/1 verification byte),
then an i64 LE non-negative row count and one `(id, time_ms, amount)` tuple
of three i64 LE values per original ledger line. The decoder rejects torn
rows, invalid counts/flags and trailing bytes. Rows are scoped by the
session's venue/account boundary and ledger symbol. Ledger replay verifies
their exact sum, rejects a repeated ID even across different settlements,
and checks each row lies within 60 seconds of its settlement. It reports
the identified line count, without promoting identity evidence to history
coverage. Old aggregate-only funding remains readable but has no IDs to
verify; its payload is never repurposed.

During an unknown schedule interval, confirmed funding ledger rows can
restore venue cash without reconstructing unobserved model positions.
Polling continues over the oldest recorded gap after schedule rediscovery.
Rate unavailability does not block this ledger-only path, and rows matching
a known pending settlement remain with its captured inventory. Stable IDs
are shared by known-settlement and gap recovery; repeated history is not
booked again and conflicts remain explicit. Only rows within recorded gaps
and no later than receipt are eligible. This is not complete account-history
recovery across restarts or a claim about rows outside those gaps.

`FUNDING_RECOVERED` (15) encodes receive ns, queried lower/upper ms and
non-negative line count as i64 LE, followed by original `(id, time_ms,
amount)` triples. It deliberately has no model-amount field. Replay checks
the range against receipt, validates line placement and rejects duplicate
IDs shared with `FUNDING_EVIDENCE`. `oq-replay --ledger` reports how many
funding rows have no historical model amount. Restoring observed cash does
not repair attribution, prove coverage or close G12.

`FUNDING_STATE` (16) persists the watch's changed obligations separately
from cash. It encodes receive ns and pending count/timestamps as i64 LE,
then an i64 LE gap count, each lower ms and a 0/1 upper-bound presence byte
plus optional upper ms, then a 0/1 uncertainty presence byte plus optional
length-prefixed reason. Counts are bounded by remaining payload before
allocation; flags and trailing bytes are rejected if invalid.

The session writes this state after funding capture and polling, including
when the venue returned no rows. Unchanged states do not generate duplicate
records. Native replay validates temporal bounds and monotone obligations:
pending can disappear only after a recorded settlement, prior gaps cannot
be removed or closed boundaries rewritten, and uncertainty cannot silently
be cleared. Closing a discovery gap does not resolve its historical coverage.
The final report also discharges pending already answered on the economic
tape if a later state write was interrupted. Legacy journals without this
record have unknown obligations. Persistence makes them inspectable across
process exit; automatic restart restoration and complete coverage proof are
separate recovery work.

Shadow attribution also needs a declared model fee policy. The production
environment defaults to unknown; supply `Production::with_model_fees`, or
override `Environment::model_fees` in a library host. The CLI accepts both
`--model-maker-fee-ppm N` and `--model-taker-fee-ppm N` (signed integer ppm,
including explicit zero and negative maker rebates). A missing partner,
duplicate option or overflowing rate is refused. The effective policy,
warm-up length and observation window are bound into the run config hash.
Unknown policy or a supplied model fee total that no longer matches the
shadow's current total makes fee attribution unavailable, so it cannot
produce a numeric residual. Venue commission coverage remains independently
required; specifying model rates does not recover missing venue economics.

For an accepted execution whose fee was unreadable, original per-trade
commission history can append native-ledger evidence. The account query
is scoped to the same symbol; the adapter validates settlement asset and
the binder checks stable trade ID plus direction, one instrument, no future
source and no conflicting duplicates. A rebate and an explicitly reported
zero remain measurements. Unmatched fills retain unknown fees.

`FEE_EVIDENCE` (17) encodes receive ns and source ms (i64 LE), a u32 LE
canonical payload length, and the original core `VenueFill` payload with
`Fee::Reported(amount)`. Native replay requires a prior equivalent typed
execution with an unknown fee, and refuses replacement of known commission
or duplicate recovery. It recomputes final account economics with the
recovered commission and unchanged inventory/realized P&L. The original
execution is preserved; fee evidence is not a second fill. The host polls
pending fees every 60 seconds and at run end. Books validate the exact
pending fill and recover cash once at receipt; duplicate evidence is a
no-op, conflicting amounts and already-priced fills are refused. Partial
recovery keeps fees unknown. Core `FeeRecovered` (14) encodes receive ns,
paid-positive/rebate-negative amount and remaining unknown count (three
8-byte LE integers); it affects cash/fees only. Identity validation belongs
to the host's pending inventory, and its count describes accepted fills,
not complete venue history. No past risk/cash curve is rewritten. Restart
state restoration and complete coverage require separate evidence.

### Comparable position observations

Live startup and periodic reconciliation use `Account::positions_observed`.
The default is unsupported, not a promotion of a legacy vector to a full
snapshot. A `PositionObservation` carries symbol, mode, every expected leg
and client-clock request/receipt bounds. `check` requires exactly one BOTH
row in one-way mode, or one LONG and one SHORT row in hedge mode, including
explicit zero rows. Empty replies, missing/duplicate/foreign legs, invalid
signs or inconsistent raw numeric text do not establish flat positions.
The adapter retains zero rows and requires explicit side fields. A provider
that omits inactive legs cannot satisfy this contract merely by returning
an empty or partial vector.

The receipt window must be ordered, no later than evaluation, and no older
than five seconds from its request start. These are client receipt bounds,
not a guarantee of an atomic exchange account snapshot or an activity-time
freshness test. Local accepted-execution revisions bracket the read; a fill
or round trip changing that revision requires a new cut. All checks are
deterministic with injected boundaries. A refused read is unread evidence,
not agreement or a position mismatch, and clears the prior successful cut.

Quantity agreement is per hedge leg as well as net. Reconnect/expired-stream
actions schedule the same checked comparison rather than adopting an
unqualified risk-view vector. Missed fills are still recovered as original
reports before a persistent difference condemns the stream. No synthetic
fill or P&L correction is introduced. Resume consumes only a fresh positive
cut of the current local revision; a missing, invalidated or expired cut
requests a checked read and refuses that resume attempt. Retry resume once
the new cut has succeeded.

### Closed-window native fee/funding coverage

Library consumers can call `ledger::replay_with_history(path, code, evidence)`
with independent `ledger_coverage::CoverageEvidence`. The attachment names
the native journal data hash, an externally established opaque account-scope
hash, a closed interval beginning at `LEDGER_START`, evaluation time and a
maximum source age. Both `HistoryWindow` sources name the same account and
symbol, cover the requested interval, and explicitly certify completeness
after their producer has established retention and terminal pagination.
`Incomplete(reason)`, unsupported history, stale/future observations and
truncated intervals refuse verification. This contract does not authenticate
an account statement or infer source completeness from a short/empty page;
the producer is responsible for that assertion. Existing `Account` vector
history APIs do not establish it automatically.

`Account::execution_fees_between(symbol, since, through)` and
`funding_charged_between` provide an optional producer contract returning
`BoundedHistory`. Their defaults are unsupported and do not wrap legacy
vectors. Binance pins `startTime/endTime` on every page and limits this
contract conservatively to a closed interval in the last seven days.
Terminal pagination, stable identities, both bounds and settlement assets
are checked; full pages that cannot advance or exceed the paging bound
refuse the whole query. Wrong-scope funding rows are refused rather than
filtered into a plausible empty history. Shared-wire dialects remain
unsupported until their own retention contract is established. Convert
these responses with `HistoryWindow::from_bounded(response, scope_hash)`;
the consumer still establishes the account-scope hash externally. Source
observation time describes the reported history as read, not a guarantee
against delayed venue corrections. A later query with changed rows needs
a new evidence identity.

Every execution must have a reported or valid recovered fee, and its stable
trade ID, side, exchange millisecond and amount must match the history.
Every funding line must match its original ledger ID, time and amount.
Surplus rows (including zero), missing rows, conflicts and equal totals with
different identities are failures. Stable overlapping rows deduplicate.
Legacy aggregate funding cannot prove original-line coverage. Empty histories
are accepted only with an explicit complete window and explicit initial
positions. Out-of-window recorded economics refuse a whole-session proof.

The reader snapshot supplies both the native identity and deterministic
fold. The returned coverage manifest has scope `openquanter native-ledger
coverage v1` and binds journal/context hashes, all canonical source rows,
source metadata and evaluation policy. Changing the evidence invalidates
its baseline; a code-only change stays distinguishable. Use the covered
report's `verify_baseline` to verify this complete evidence identity; it
refuses native-only manifests and unknown code. Plain native replay
continues to report funding coverage as false; a successful covered replay
reports fee/funding coverage for the declared interval only. Coverage does not reconstruct model
positions in schedule gaps, certify other account transfers, restore a
strategy, or supply a numeric G12 residual.

`ledger::capture_history(path, code, account, scope_hash, through_ms, clock,
max_age_ms)` derives the start from the latest native ledger context,
queries both bounded histories, evaluates them with the injected clock and
returns a covered report plus its evidence. Unsupported sources or failed
identity-set checks return no successful attachment. The same reader snapshot
is used throughout, so an appended journal requires new evidence.

Persist a successful attachment with `ledger_coverage::write_evidence` and
read it with `parse_evidence`. The strict text format starts with
`openquanter ledger-history evidence v1`; ordered fields carry journal and
account hashes, requested/evaluation boundaries, then each source's symbol,
bounds, observation, row count and original rows. Row integers and BUY/SELL
are canonical; extra/reordered fields, unreasonable counts, truncation and
incomplete source windows are refused. Exclusive creation plus fsync avoids
overwriting a journal or earlier attachment; a torn file cannot verify.

```text
oq-replay run.oqj --ledger --coverage-evidence run.history-evidence --require-coverage
```

The CLI prints source interval, account scope, row counts and coverage
data/policy hashes. Bad evidence returns 2 even without `--require-coverage`.
`--write-manifest` / `--verify-manifest` still describe the native-only
identity; use the covered report's `verify_baseline` for the coverage identity.
The historical evaluation time in the attachment is replayed, not replaced
with wall time when an offline report is read.

### Covered final live attribution

The production host accepts `--account-scope-hash SHA256`; library hosts
provide `Environment::account_scope_hash` (production builder:
`with_account_scope_hash`). This is an opaque, externally established scope,
not authentication by hashing arbitrary text. After final funding and fee
recovery, a healthy native journal and both bounded sources are required.
Verified histories are exclusively persisted as
`<stem>.<coverage-data-hash>.<coverage-policy-hash>.history-evidence`.
Source or attachment failure leaves the final decomposition incomplete.
Interim control reports likewise have no numeric residual before final
closed-window evidence exists.

`ledger::attribute_with_history` verifies the same immutable native snapshot
and compares its realized-minus-fees-plus-funding result with Books. It also
checks the shadow's clean starting instrument/contract, balance, position
mode and observed legs, and exact canonical venue fill sequence. Actual P&L
comes from the fold, not the explained component sum; model P&L comes from
the independent shadow. A model fee policy must be stated. Model funding
is unavailable for unmodelled recovered lines, unverified settlements,
missing/pending/gapped obligations or stale/inconsistent funding totals.
No historical model inventory is invented to turn a known cash line into
a modelled settlement.

The report scope is `openquanter covered live attribution v1`. Its data hash
binds complete source coverage, and its config hash binds native context,
run policy and effective model seed. Full data/config hashes accompany the
printed report. A code-only change remains distinguishable from changed
inputs/configuration. A synthetic round-trip with a non-zero independently
folded P&L gap checks a fully measured zero residual and refusal of foreign
seeds, fills, fees or totals. Historical production acceptance still needs
its own identified build, original model inventory and covered report.

### Identity-bound native ledger reports

```sh
oq-replay run.oqj --ledger --write-manifest run.ledger-manifest
oq-replay run.oqj --ledger --verify-manifest run.ledger-manifest
```

The report reuses `RunManifest` with the method scope `openquanter
native-ledger manifest v1`. Data hash covers the complete raw snapshot
held by `Reader`, including frames irrelevant to the final account fold;
hashing never reopens a journal that may have changed after decoding.
Native config hash covers the scope, a zero separator and the encoded
`LEDGER_START` context actually used by this session. It is the accounting
context, not the strategy configuration. Torn tails and undecodable frames
remain refused before binding.

The sidecar is four lines: scope header, `code_commit=`, `data_hash=` and
`config_hash=` in that order. Duplicate, missing, extra or malformed fields
are refused. Creation uses a new file and cannot overwrite the journal or
an existing sidecar. Verification rejects changed data/context; a known
code-only change reports `CodeChanged` rather than pretending identical
code. Build-time `GIT_COMMIT` or `GITHUB_SHA` provides the revision. Without
it, the report/sidecar says `unknown` and baseline verification exits 2.
Identity verification never promotes unknown coverage: `--require-coverage`
still independently checks fee/funding evidence, and this native accounting
scope does not certify full strategy/margin replay or G12.
