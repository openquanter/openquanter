# The live path

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

Ours derives the id from state the core already holds — instrument, leg,
ladder rung, sequence — so the same intent always produces the same id,
and recovery can ask about it. Uniqueness across reuse comes from our own
dedup store, because the venue does not provide one.

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

**Two orders left the book in the same millisecond, for opposite
reasons.** One had filled; the other had been cancelled and replaced.
The last snapshot before they disappeared reported zero executed
quantity for both, and the venue offers no way to tell them apart from a
snapshot. A reader that labelled both "filled" would have been wrong
about one, and "cancelled" wrong about the other. L2 and L6 say to
report the departure and refuse the cause; this is the instant that
makes refusing the only answer that is not a fabrication.

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
4. **Risk gate.** Inside the core (`D7`), so its decisions are
   deterministic, journaled, and exercised by every backtest rather than
   living beside the path they are supposed to guard.
5. **Assembly and recovery.** Snapshot restore, reconciliation on
   startup, graceful restart.

Each step is testable before the next exists. Step 1 in particular is
worth running for its own sake even if nothing after it is ever built: it
is an independent check on the system currently trading.

## What this does not cover

Balances. Nautilus reconciles orders, fills and positions and leaves
balances to a separate path, and the same split applies here for now: a
balance divergence with matching positions is a fee or funding
accounting question, not an execution one, and answering it needs a model
of both that this does not yet have.

Venues other than one. The design is written against Binance USDT-M
because that is what runs; `DownloadBegin`/`DownloadEnd`, the three-valued
failure classification and the staleness diff are venue-independent, but
nothing here has been tested against a second venue's idea of what an
order report contains.

And the whole of it is a design, not a result. The section above on
Nautilus's 195 reconciliation entries is the honest expectation: the
first version of this will be wrong in ways the survey cannot predict,
and the read-only phase exists so that being wrong is cheap.
