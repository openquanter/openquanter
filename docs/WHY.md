# Why OpenQuanter exists

[English](WHY.md) · [中文](WHY.zh-CN.md)

> Every cent between a backtest and the live run, accounted for.
>
> **P&L you cannot explain is not P&L.**

---

## This did not start on a blank page

OpenQuanter 2.x has a predecessor. That predecessor has **traded real money on
real venues, continuously, for years** — not a demo, not a paper backtest, but
the kind of running where positions are open overnight, leverage is real, and
liquidation risk wakes someone up.

It is still running today.

That fact set this project's character. **2.x is not "we would like a nicer
trading framework." It is "we hit a wall, and there is no way around it."**
The section below describes the wall, and it explains the trade-offs here
better than any feature list could.

If you are evaluating frameworks, that section is also your evidence: **you
will meet these problems eventually. The only question is whether you read
about them first or discover them yourself.**

---

## What the wall looks like

> **The dangerous error is the one that looks right.**

What the six below share is not their severity. It is that **none of them
raises an error.**

### 1. Failure is silent

This is the expensive kind, because it does not behave like a bug.

The predecessor once produced synthetic fills at a price of zero, which made
an entire position ladder compute against nonsense. **Nothing crashed, nothing
alerted, the logs looked normal** — because the path that wrote logs and the
path that raised the error were not the same path. The exception went to
stderr, and stderr went to `/dev/null`. It was found by a human reconciling
accounts afterwards.

The same shape kept recurring: a cancellation loop that never terminated, a
take-profit order that quietly went missing, strategy state that was never
warmed after a restart, a market data feed that spent a week writing 24-hour
rolling volume into a per-trade field. **What they share is that the system
believed it was fine.**

The real danger in a trading system is not an error. It is **being wrong while
looking right**.

### 2. The backtest looks good, the live run differs, and nobody can say where

Every quant knows this problem. Almost nobody can quantify it.

There are at least six candidate causes: slippage, queue position, funding
that differs from the model, latency between signal and venue, maker/taker
misclassification, and events the backtest has no concept of — partial fills,
rejections, venue outages.

**Existing tools tell you how large the gap is. None tells you which part it
came from.** So every discrepancy collapses into "the market changed" — an
explanation that can neither be refuted nor improved upon.

For an unlevered strategy that is a disappointment. For a levered one it means
**you are using leverage to amplify something you do not understand**.

### 3. "Backtest and live share the same code" is usually not true

In the predecessor, the backtest engine and the live strategy were two
implementations. They were *supposed* to agree. Nothing enforced it.

The cost is concrete. A matching defect fixed recently in this project — the
matcher used the running high since the start of the minute rather than the
extreme belonging to the current tick, and so matched against a price that had
not happened yet — **had to be fixed twice, once in each engine**. And it was
not found by code review. It was found because **two independent
implementations disagreed on the same data**.

In a single implementation that bug stays silent forever. It does not raise an
error. It just bends the equity curve upward by about one percent.

### 4. The backtest is slow enough to change how you work

A full two-year run took tens of minutes. That sounds tolerable until you
notice its real consequence: **you stop running the ideas that are probably
useless but worth a look.**

Slowness is not only waiting. It quietly **narrows the questions you are
willing to ask**.

(The same data through the 2.x engine finishes in well under a minute. The
point is not the benchmark. **The point is that it is a different way of
working.**)

### 5. Nothing can show that a past result still holds

Six months ago a backtest returned 20% annualised — an illustrative figure,
not a real result. Since then the code has changed, the data has been
repaired, parameters have moved. **Does that conclusion still hold?**

The predecessor could not say. A result was a number, and it carried no
provenance: which code, over which data, under which configuration. Re-run it
after the inputs moved and you get a different number, and **you cannot tell
"the engine changed" from "the inputs changed"**.

So past conclusions survive on memory and trust. With real money at stake,
**trust is the most expensive form of technical debt.**

### 6. Overfitting has no price tag

Sweep two hundred parameter sets, take the best one. Everybody does this,
everybody knows it is a problem, and **nobody writes down the probability that
the winner is noise.**

It is not that the number cannot be computed. The methods have existed for
years — the deflated Sharpe ratio, PBO under combinatorially symmetric
cross-validation. They are simply **not in the default output**, and an
optional honesty check is not a check. Nobody looking at a beautiful equity
curve goes hunting for the button labelled "tell me this might be fake".

---

## The wall is not only ours

If those six only happened to one project, they would be that project's
problem. They are not.

Lay out the trading frameworks that have accumulated over the years — open
source, commercial, in-house — and **the same set of problems recurs, in almost
the same shape**. 1.x was not special. It was one of them, and like most of
them it had evolved out of an earlier framework before it.

**Every item on the list below, 1.x had in full.** This is not criticism from
outside. It is a description of where we climbed out of.

### The ones a practitioner recognises immediately

- **Backtest and live are two codebases.** Nearly every older framework has a
  `BacktestEngine` and a `LiveEngine` sharing some interfaces, with behavioural
  agreement maintained by hand. Nothing prevents them from diverging, and when
  they do, **nothing raises an error**.
- **Matching is too kind.** Resting orders fill, you get the price you asked
  for, there is no queue, no partial fill, no rejection, and liquidity is
  infinite. The most profitable part of a backtest often comes precisely from
  assumptions that do not exist.
- **The account can never blow up.** Margin and liquidation are missing or
  approximated, so the tail risk of a levered strategy is systematically
  hidden — **and the tail is the only part of a levered strategy that
  ultimately matters.**
- **Fees and slippage are a constant.** No maker/taker split, no tier, no
  size dependence, no rebates.
- **Look-ahead is not preventable.** Bar-based backtests decide on a completed
  bar whose high contains prices that came after the decision; indicators are
  normalised over the whole sample; history is re-run after data is revised.
  These errors **do not crash and do not alert. They just make the curve look
  better.**
- **Data has no identity.** The same script produces a different number this
  month than last, and nobody can say whether the code changed, the data was
  repaired, or a parameter moved.
- **State recovery is a JSON file.** A restarted process reads a file, and when
  the file and the venue disagree the outcome ranges from a reconciliation
  headache to placing orders against a position that does not exist.
- **Incidents cannot be replayed.** Logs are written for humans, not for
  replay. Post-mortems are assembled from fragments and memory.
- **The dependency tree is too large to upgrade.** Installing a framework
  brings in a hundred packages, and one day a transitive dependency changes
  behaviour and costs a day to find.
- **The framework is a platform.** Want only its margin model, or only its data
  layer? You cannot. Take all of it, or rewrite it.

### The ones a beginner hits directly

If you are new to this, the list above may still be abstract. These are the
ones you meet **in the first week**:

- **It will not install.** Dependency conflicts, version hell, a package that
  needs a compiler you do not have. Many people give up here, and **this has
  nothing to do with trading ability.**
- **The tutorial runs; your own data breaks it.** Because the sample data was
  cleaned, and real data has gaps, duplicates, out-of-order rows and timezone
  problems.
- **The backtest return is implausibly good and the live account loses money.**
  This is the one that hurts: you thought you had found a strategy, and you had
  found an assumption in the framework.
- **You cannot tell what you did wrong.** No error, just a nice curve and a
  shrinking balance. So you ask in a chat group, where nobody can tell either.
- **Documentation does not match the code.** The examples use an API from two
  years ago, the error message has no search results, and the only recourse is
  reading source that has no comments.
- **Changing one parameter costs an afternoon.** So you stop trying the ideas
  that are probably useless but worth a look.

**These two lists are the same list.** Beginners meet the symptoms;
practitioners recognise the causes. And many practitioners spent years meeting
the symptoms before they could name the causes — **ourselves included**.

### Why these are hard to fix

To be fair about it: **the maintainers of those frameworks know, and it is not
that they do not want to fix them.**

A framework with users carries three things:

1. **The API cannot be overturned.** Thousands of lines of user code are
   written against it. Change one semantic and somebody's strategy behaves
   differently one morning — possibly while holding a position.
2. **Past results cannot be voided.** Admitting "old backtests are not
   reproducible" means telling users that years of their conclusions need
   re-verifying. No maintainer wants to say that, and it is hard to demand.
3. **What is running cannot stop.** Rebuilding the matching core means every
   user pauses, re-tests and re-deploys. That cost is not paid by the
   maintainer; it is paid by every user.

So a mature framework can only add **increments on top of the burden**: another
optional parameter, a compatibility shim, a paragraph explaining why some
historical behaviour is the way it is. **Every step is rational, and together
they cannot reach the root.**

That is what 1.x hit. We profiled, we took the obvious optimisations, and what
remained **was structural** — no further tuning of that codebase was going to
move it, because the limit was the architecture rather than the code.

---

## So 2.x is an attempt with the burden put down

We do not carry those three things: **no external users, no historical
conclusions to defend, nobody else's live trading on top of us.**

Today that is a disadvantage. No users, no ecosystem, no community. But it buys
one thing a mature framework cannot: **the ability to do it the right way from
day one, even where the right way is more expensive.**

Content-addressed data, reference baselines that pin engine behaviour, a
journal of every decision, overfitting statistics in the default output —
**none of these can be added afterwards.** A framework that decides to want
reproducibility in its third year cannot make its first-year results
reproducible. It is a discipline cost payable only from the beginning, **and we
are still in a position to pay it.**

### We would like to be the guinea pig

Plainly: **we do not know whether this path works.**

"Every cent accounted for" is an aim, not a verified conclusion. The
decomposition may stall partway. The unexplained residual may stay stubbornly
large. The discipline may turn out to cost more than a working team can carry.

But there is one thing we are in a position to do: **try it with real money and
report what happens.**

1.x is running. 2.x will run beside it — same market, same data, same strategy.
**Where they differ, we will measure it line by line, including when the
measurement is unflattering.**

So the promise this project makes is not that we solved these problems. It is:

> **We will walk this path with real money in a real market, and write down
> every wall we hit — including the case where the conclusion is that it does
> not work.**

If it works, the people after us can skip these years.
If it does not, the record of the failure is worth something too — **at least
the next person knows not to try it here again.**

That is probably the only valuable thing a project with no users yet can offer.

---

## What 2.x is for

Those six have one shape in common:

> **None of them raises an error.**

Failure is silent, the gap is unattributable, two implementations diverge
unnoticed, slowness is gradual, conclusions expire quietly, and overfitting
looks like skill.

**So the goal is not "faster" or "more features". It is to make the silent
things visible, measurable, and visible by default.**

In one line:

> **Every cent between a backtest and the live run, accounted for.**

Which means a live session produces not "we made 3.7% less", but:

```
Backtest expected   +12,400
Live actual         +11,940
──────────────────────────
Gap                    -460
  slippage             -148
  queue position       -112
  funding vs model      -96
  latency               -61
  fee tier              -22
──────────────────────────
  unexplained residual  -21   ← this line is the product
```

**That last line is the report card.** It cannot be argued down, only earned
down by genuinely understanding one more cause. Every cause explained is a real
improvement to the model — **the goal carries its own feedback loop, and its
own honesty constraint**.

It will probably never reach zero. Microstructure always keeps something back.
But **no framework today attributes any of these**, and getting two of them
named is already something that does not exist elsewhere.

---

## Who this is for

### People who would choose it

**1. Levered traders with money at stake and someone to answer to.**

Prop desks, small funds, or you. Being wrong about a backtest costs six figures
and up, and for these people **"how much of this return can I not explain"
belongs on the decision table more than the Sharpe ratio does**. A 20x strategy
with 5% of its P&L unexplained and one with 0.3% are not the same instrument.

**2. Teams rewriting or migrating a trading engine.**

A narrow position, but a real one. Anyone rewriting an engine has to answer
"how do I prove the behaviour did not change", and the existing tooling for
that is close to empty. This project's parity instrument **was built before the
thing it measures**, because that is what it was originally built for.

**3. Anyone who has to show their results to somebody else.**

Investors, partners, risk. The difference is this: when a commercial product
says "our backtests are trustworthy", you can only choose to believe it. **When
an open-source project says it, you can run it and check.** The positioning and
the licence are the same shape.

> This section is about **motivation** — who would pay the switching cost. The
> **capability matrix** (which user needs which feature) is in [Requirements
> §2](REQUIREMENTS.md#2-target-users-and-scenarios), and the formal niche and
> differentiators are in [§1 Positioning](REQUIREMENTS.md#1-positioning).

**How the predecessor and this project coexist** is in that document too: the
**private overlay** is a first-class deployment shape — proprietary strategies,
parameters and captured data stay in a private repository with a one-way
dependency on the public crates. **The public repository never contains
proprietary content.** The predecessor described here is exactly such an
overlay. What it contributes is the experience of hitting the wall, not its
strategies.

### Who it is not for

Saying what a project will not do matters more than saying what it will:

- **People who want a large indicator library.** Others have two hundred. This
  does not compete there.
- **People who want fifty broker integrations.** Breadth is the retail play,
  and it conflicts directly with auditability — every unverified adapter
  dilutes the claim that anything here is provable.
- **People who want hosting and one-click deployment.** That turns
  "reproducible" into "trust our servers".
- **People who only want the fastest.** Speed is a prerequisite, not a pitch.
  Fast frameworks already exist.

---

## The longer view

One industry habit deserves questioning: **we demand strategy returns to two
decimal places, and accept "the backtest says so" as the entire basis for
believing them.**

That is not a reasonable standard. No other engineering field — chips, bridges,
drugs — accepts "I tested it, trust me" as a deliverable. They deliver
**reproducible measurements and a known error bar**.

> **A number without an error bar is an opinion, not a measurement.**

The unexplained residual is, in the end, the error bar that trading results have
never been given.

Quantitative trading should be stricter, not looser, because its failures are
immediate, monetary, and irreversible.

So the aim is not to become the most popular trading framework. It is:

> **To turn "this backtest result holds" from a statement requiring trust into
> an assertion a third party can verify.**

And this has one unusual property: **reproducibility cannot be retrofitted.**

A mature framework with many users and a long history of results cannot go
back. Its results from three years ago have no content-addressed data, no
pinned engine behaviour, no journal of decisions. That gap is not closed by
engineering effort. **It is a discipline cost payable only from the first day.**

This project is still small enough to pay it.

### None of this is ours

The three lines in this document — **P&L you cannot explain is not P&L**, **the
dangerous error is the one that looks right**, **a number without an error bar
is an opinion** — are one principle applied to trading. The principle is
Feynman's, from the 1974 Caltech commencement address:

> **"The first principle is that you must not fool yourself — and you are the
> easiest person to fool."**
>
> — Richard Feynman, *Cargo Cult Science*

He was talking about science, but the subject of that talk — a complete ritual,
everything apparently in order, and no result — describes backtesting with
uncomfortable precision.

**Naming the source is worth more than pretending to originality.** We have not
found a new principle. We are trying to carry an old one all the way through, in
a field unusually good at fooling you and unusually quick to charge for it.

---

## Where it actually stands

**This document describes a direction, not a state.** What is built, what is
not, and what has test coverage is in [the README's status
section](../README.md#status), which is written to be deliberately unflattering
— including the admission that "every cent accounted for" currently accounts
for **none of them**: the first link of the chain is connected — the live
process journals its decisions now — and the second is not, because the kernel
is not in the live path, so there is a record and nowhere to replay it into.

What each milestone unlocks, what triggers it, and what its exit gate is, is in
the [roadmap](ROADMAP.md).

**Where this document and the code disagree, the code is right, and the
disagreement is a defect worth reporting.** In a project whose whole claim is
that its results can be checked, documentation that is more optimistic than the
code is the one unforgivable error.
