# Execution

> [中文版](EXECUTION.zh-CN.md)
> Companion documents: [Live Path](LIVE-PATH.md) · [Roadmap](ROADMAP.md)

How an order reaches a venue, how its fate comes back, and what the
framework does when those two disagree.

## The shape the venue forces

A venue speaks over two transports, and they are not alternatives.

| | Direction | Carries | Authentication |
|---|---|---|---|
| **HTTPS/JSON** | Client asks | Place, cancel, query, account | Signed per request |
| **WebSocket** | Venue pushes | Order updates, fills, account changes | Once, via a key fetched over HTTPS |

Neither half is sufficient. Over HTTPS alone the only way to learn that
an order filled is to ask again, which turns a push into a poll and
loses both the ordering of events and the latency that made the strategy
worth running. Over WebSocket alone nothing can be sent.

So an execution adapter is two halves that have to agree, carried on two
connections that fail independently. **That is the central fact of this
design.** A gateway that models the venue as one channel is a gateway
that will one day believe an order was never placed because the socket
that would have said so was down.

## What an order actually is

An order is not a request that returns a result. It is a claim submitted
to a system that will act on it whether or not the answer arrives.

Three outcomes, and the third is the one that matters:

- **Accepted** — the venue said yes and named the order.
- **Rejected** — the venue said no and said why. The order does not
  exist, and this is a clean, final answer.
- **Unknown** — the request timed out, the connection dropped, or the
  response was unreadable. **The order may or may not exist.**

Folding the third into "error" is the defect that produces duplicate
positions. A caller that retries a timed-out placement has, half the
time, placed two orders; a caller that gives up has, half the time,
abandoned a live one. `Unknown` is therefore a variant of the success
type, not an error, so the compiler makes every caller decide what to do
about it.

What to do about it is defined, not left to the caller's judgment: every
order carries a **client order id** chosen before the request is sent,
so the question "did it land?" is answerable by asking the venue about
that id. An id chosen by the venue cannot answer it, because the whole
problem is that the venue's answer never arrived.

Idempotency is therefore not a feature of this design; it is the only
reason the design is safe.

## The seam

```
                 ┌──────────────────────────┐
   intents  ───► │  risk gate (oq-risk)     │ ── refused ──►
                 └────────────┬─────────────┘
                              │ permitted
                 ┌────────────▼─────────────┐
                 │  Execution (trait)       │
                 └────────────┬─────────────┘
                     ┌────────┴────────┐
             HTTPS/JSON             WebSocket
             place/cancel           fills/account
                     └────────┬────────┘
                 ┌────────────▼─────────────┐
                 │  reconciliation          │
                 └──────────────────────────┘
```

`Execution` is a trait because the venue behind it changes and the layers
above it must not. Market data already has this seam — two exchanges are
implemented against it and the second needed no change to the capture
binary. The order path had none, and the risk register records why that
was the more expensive half to leave open: incidents originate at the
venue boundary far more often than in the matching kernel.

## What stays read-only

`oq-gateway` could not place an order, and said so in its first line.
That property is now qualified rather than abandoned:

- Reading an account remains reachable without any type that can trade.
- Order entry lives behind `Endpoint`, a type with two values, and the
  live one has to be named. A misconfigured string cannot silently
  become production; there is no string.
- Every order-placing method takes the client order id as an argument
  rather than generating one, so a caller cannot place an order it has
  no way to ask about later.

The point of the original boundary was that trading should be a visible
decision rather than an accident. A typed endpoint and a mandatory
client id keep that true while allowing the thing to trade.

## Sequence, and where it goes wrong

```
strategy    risk gate     REST            venue          WS user stream
   │  intent   │           │                │                  │
   ├──────────►│           │                │                  │
   │           │ permit    │                │                  │
   │           ├──────────►│  POST /order   │                  │
   │           │           ├───────────────►│                  │
   │           │           │◄───────────────┤ accepted         │
   │           │           │                ├─────────────────►│ NEW
   │           │           │                ├─────────────────►│ FILLED
```

Every arrow can fail on its own:

| Failure | Symptom | Answer |
|---|---|---|
| POST times out | `Unknown` | Query by client order id |
| Response unreadable | `Unknown` | Same |
| Stream drops | No fills arrive, account drifts | Reconnect, then reconcile open orders and positions against the venue |
| Listen key expires | Stream closes | Renew before it can; treat expiry as a gap, not as silence |
| Fill arrives twice | Duplicate accounting | Deduplicate by venue trade id |
| Fill arrives out of order | Position goes negative and back | Order by venue sequence, not arrival |

The last three are why the stream is treated as a feed with gaps rather
than as a reliable log. The capture path already learned this: a stream
that has been quiet is indistinguishable from a stream that has died,
and only an explicit liveness signal separates them.

## Reconciliation is the resting state

The framework does not assume its own books are right. Positions and
open orders are compared against the venue on a schedule, at startup,
and after any `Unknown`. `oq-gateway::reconcile` already does this
comparison and reports differences rather than repairing them silently,
because a repair that is wrong is worse than a difference that is
visible.

Startup is the strictest case: an unrecognised position at startup is
fatal, not a warning. A process that begins trading beside a position it
does not know about is a process whose risk limits mean nothing.

## Two things a running system taught the predecessor

Both were found by reading the platform this project replaces, which
traded live for years. Neither is derivable from the API documentation.

**A hedged account refuses an order that does not name its leg.** A
venue can carry one net position per contract or two. The modes take
different parameters, and an order built for the wrong one is refused
with a message about a position side the caller never set. The mode is
therefore asked for at connect, not assumed. `reduceOnly` and a hedged
leg are mutually exclusive at the venue, so they are refused together
here, where the answer can explain itself.

**An open socket is not a delivering socket.** The connection stands,
the reads time out, and the account has been moving the whole time —
indistinguishable, from inside, from a quiet account. The only
resolution is a second source: the venue's own view of the positions,
fetched on a schedule and compared against the view the stream has
built. Three consecutive disagreements condemn the stream, not one,
because a fill in flight is visible to one side before the other and a
check that acted on the first difference would reconnect constantly
under load — which is exactly when it must not.

## What is not here yet

- The risk gate itself (`oq-risk`): limits, kill switch, and the
  fatal-on-unknown-state startup check.
- Order entry over WebSocket, which the venue also offers. REST first
  because its failure modes are the ones described above and are already
  understood; adding a second write path before the first is proven
  would double the surface without a reason.
- Any venue other than Binance USD-M. The trait exists so that the
  second one is an implementation rather than a rewrite; that claim is
  unproven until a second one exists.
