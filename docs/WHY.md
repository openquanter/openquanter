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

---

## Where it actually stands

**This document describes a direction, not a state.** What is built, what is
not, and what has test coverage is in [the README's status
section](../README.md#status), which is written to be deliberately unflattering
— including the admission that "every cent accounted for" currently accounts
for **none of them**: the live process does not yet journal its decisions, so
the first link of the attribution chain is missing.

What each milestone unlocks, what triggers it, and what its exit gate is, is in
the [roadmap](ROADMAP.md).

**Where this document and the code disagree, the code is right, and the
disagreement is a defect worth reporting.** In a project whose whole claim is
that its results can be checked, documentation that is more optimistic than the
code is the one unforgivable error.
