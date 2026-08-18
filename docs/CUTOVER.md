# Position-carrying cutover

Moving a live strategy from one system to another **without flattening
the position first**.

Flattening is the easy cutover and it is not free: closing and reopening
pays the spread twice, prints the intent to the tape, and — for anything
holding overnight — realises a tax event and a funding leg that the
strategy did not choose. For a small position none of that matters. For
a position large enough that the cutover is being planned at all, it
does. So the procedure below carries the position across, which is
harder in exactly one way: for a period, two systems know about the same
money.

This is requirement **FR-OPS-4** and one of M3's four entry triggers,
which asks for two successful testnet rehearsals before any production
use.

> **Status: skeleton.** Every step below is specified and every command
> in it exists. **None of it has been rehearsed.** A procedure that has
> not been executed is a hypothesis about a procedure — the steps that
> turn out to be wrong are never the ones the author expected, which is
> why the rehearsal is an entry trigger rather than a formality. Section
> 7 records what each rehearsal is required to produce, and section 6
> lists what is known to be missing before one can be run at all.

## 1. The invariant

**At every instant, exactly one system may send orders for this
account.**

Not "should". The whole procedure is arranged so that violating it
requires two mistakes rather than one, because the failure it produces
is the worst one available: two systems each sizing against a position
the other is also changing, converging on double the intended exposure,
in a market that is moving.

Everything else here is bookkeeping. This is the thing.

## 2. Preconditions

None of these are checks to perform during the cutover. They are
conditions to establish before the window opens, because each one is a
reason to postpone rather than a step to work around.

| # | Condition | How it is established |
|---|---|---|
| P1 | The new system has run in shadow against this account long enough to compare, and every divergence is attributed | Shadow run log; M3 entry trigger 2 |
| P2 | Both systems agree with the venue *now* | `oq-recon` against the account, from both hosts, within the same minute |
| P3 | The client order id prefixes are disjoint | `oq-trade --id-prefix` on the new system; the old system's prefix read from its config, not assumed |
| P4 | The new system's risk limits are set, and are not the defaults | Limits nobody set refuse everything, which fails safe — and fails the cutover |
| P5 | The abort path has been rehearsed more recently than the cutover path | See section 5 |
| P6 | A person who can decide to abort is present and is not the person executing | The executor is the worst-placed person to judge whether to stop |

**Do not open the window** during a funding settlement, in the minute
either side of a scheduled venue maintenance, or while the position is
within 20% of a liquidation price. The first two add a second moving
part; the third removes the time to abort.

## 3. The window

The steps are ordered so that **the position is never unmanaged and
never doubly managed**. Each names what it does, how you know it worked,
and what abort means at that point.

### Step 1 — Freeze the old system's new entries

The old system stops opening. It keeps managing what it holds: stops,
take-profits, and reduce-only exits stay live.

- **Done when:** the old system's log shows the freeze accepted and no
  new opening order is sent for one full decision interval.
- **Abort:** unfreeze. Nothing has moved.
- **Why first:** the position stops changing shape from here on, so
  every subsequent step compares against a fixed thing.

### Step 2 — Record the truth

Take the venue's own view, not either system's:

```text
oq-recon <SYMBOL> --record cutover-$(date +%Y%m%dT%H%M).txt
```

Record: net position per leg, average entry per leg, every resting order
with its client id, account equity, and the timestamp. This record is
what the whole cutover is checked against, and it comes from the venue
because both systems are about to disagree with each other.

- **Done when:** the file exists and its contents match what the old
  system believes it holds.
- **Abort:** unfreeze. Nothing has moved.

### Step 3 — Withdraw the old system's resting orders

Cancel every order the old system placed. Not "adopt" them: an order
placed by another system carries that system's client id, and the new
system cannot say what it means — whether it is a stop, a scale-in, or a
leftover — without being told, and being told is not the same as
knowing.

- **Done when:** `oq-recon` shows zero resting orders under the old
  prefix, confirmed against the venue rather than the old system's log.
- **Abort:** the old system replaces them. This is the last step whose
  abort is free.
- **The position is now naked.** This is the exposed interval and it is
  the reason for the whole ordering: from here until step 5, a
  fast-moving market has no automated protection. Keep it short, keep a
  manual flatten command ready, and do not begin this step if the
  position is near a liquidation price.

### Step 4 — Stop the old system

Not "pause". The process exits.

- **Done when:** the process is gone — checked by pid, not by log line —
  and its account stream is disconnected.
- **Abort:** restart it with `--adopt-existing` against the step 2
  record. It is now in the same situation the new system is about to be
  in, which is a useful thing to have discovered on the way out rather
  than on the way in.
- **Why an exit and not a pause:** a paused process is one signal away
  from sending an order, and the invariant in section 1 is not defended
  by an intention.

### Step 5 — Start the new system, adopting the position

```text
oq-trade --symbol <SYMBOL> --id-prefix <NEW> --adopt-existing \
         --max-position <LOTS> ...
```

`--adopt-existing` is how an operator states that the position is known
and intended. Without it the run refuses to start beside a position it
was not told about, which is the correct default and the wrong one here.

- **Done when:** the new system's startup reconciliation reports the
  step 2 position, leg by leg, and its risk gate is armed.
- **Abort:** stop the new system, restart the old one with
  `--adopt-existing`. The position has not changed; only which process
  is watching it has.
- **Check before proceeding:** `oq-recon <SYMBOL> --against <the step 2
  file>` must exit zero. The new system's view must equal the record
  *exactly*. A difference in average entry is not cosmetic — it
  is the number every subsequent P&L and every stop distance is computed
  from.

### Step 6 — Re-establish protection

The new system places its own stops and working orders, under its own
prefix.

- **Done when:** `oq-recon` shows the expected resting orders, all under
  the new prefix and none under the old.
- **The exposed interval ends here.**

### Step 7 — Watch

Do not walk away. For one full decision interval, or fifteen minutes,
whichever is longer:

- `oq-recon` at the start and end of the interval, and both must agree.
- No order under the old prefix appears on the account stream. One that
  does means the old system is alive somewhere, and the response is to
  abort, not to investigate.
- The new system's `foreign()` count stays at zero.

## 4. What the operator writes down

At each step, in a file, with wall-clock timestamps: the step number,
the command run, the output's decisive line, and the decision taken. Not
because a form is useful, but because the rehearsal's whole product is a
record of where the procedure was wrong, and memory is not that record.

## 5. Abort

**Abort is the default, not the exception.** If a check does not pass,
abort. Investigating with a naked position is how a fifteen-minute
window becomes an afternoon.

The abort path is the same at every step: **restore the last state in
which exactly one system was managing the position.** Before step 4 that
is the old system, unfrozen. After step 5 it is the new one. Between
them — the exposed interval — abort means choosing one, starting it with
`--adopt-existing`, and confirming with `oq-recon` before anything else.

Two things that are *not* abort:

- **Flattening the position** is a separate decision with its own cost,
  and it is the right one when the market is moving faster than the
  procedure. Deciding that is the second person's job (P6).
- **Starting both systems "just to be safe"** violates section 1. There
  is no situation in which it is the safe option, and it will look like
  one at 3 a.m.

## 6. What is missing before a rehearsal can happen

Stated because a playbook that lists only its steps reads as though it
were ready.

- **No freeze command.** Step 1 assumes the old system can be told to
  stop opening while continuing to manage. Whether it can, and how, is a
  property of that system and is not recorded here.
- ~~**No adoption verification tool.**~~ Closed. `oq-recon --record FILE`
  writes the account at step 2 and `oq-recon --against FILE` compares a
  later reading, exiting non-zero on any difference — a leg that moved, an
  average entry that moved, an order that appeared or vanished. The
  timestamp is deliberately not compared, because the second reading is
  later by construction. What is still missing is the same tool pointed at
  the *new system's* view rather than the venue's: today step 5 compares
  the venue against the record, which catches the position changing but not
  the new system misreading it.
- **No timing data.** Every "keep it short" here is unquantified. The
  first rehearsal's main product is how long the exposed interval
  actually is.
- **`oq-live` is not yet a supervised process.** M3's scope opens by
  noting that `oq-live` depends on neither `oq-core` nor `oq-margin`,
  so what step 5 starts is not yet the process this procedure assumes.

## 7. What a rehearsal must produce

A rehearsal that produces only "it worked" has produced nothing. Each
one records:

1. Wall-clock duration of every step, and of the exposed interval.
2. Every check that passed on the second attempt rather than the first.
3. Every command whose output an operator had to interpret rather than
   read — each of those is a tool that should exit non-zero and does not.
4. The step at which an abort was *tested*. At least one rehearsal must
   abort deliberately, mid-window, and the abort must be the one in
   section 5 rather than an improvised one.
5. What in this document was wrong.

Two rehearsals are required, and they are not two runs of the same
script: the second incorporates what the first found, and a second
rehearsal that finds nothing new is evidence that it was not run
differently enough.
