# oq-engine

Matching. Fidelity tier L0 (tick replay) is implemented; L1 (queue
position, latency, impact) and L2 (order book) are the rungs above it.

## Commands

```bash
cargo test -p oq-engine
cargo clippy -p oq-engine --all-targets -- -D warnings
```

## Invariants

- **L0 semantics are frozen.** Every regression test in the project
  measures against them. A change that makes L0 *better* is still a
  change that breaks parity; improvements belong in a higher tier.
- **Orders are not eligible on the tick they are submitted.** Same-tick
  eligibility is lookahead: the strategy would be acting on information
  the market had not produced when the order would really have been
  sent.
- **Gap fill is at the order's own price**, and it runs *before*
  ordinary crossing. The market reached that level, so that is where
  the trade happened.
- **Gap fill resolves candidates in arrival order, not price order.**
  This reproduces the reference implementation's insertion-ordered walk
  and therefore its trade identifiers. Price-order here would produce
  identical fills with different ids, and parity compares ids.
- **Limit orders get price improvement**: a buy fills at
  `min(limit, market)`, a sell at `max(limit, market)`.
- **Price-time priority** within the book, with arrival rank as the
  tie-break. Never wall-clock time: two orders accepted in the same
  nanosecond still need a definite, replayable order.
- **Matching is a pure function of state and tick.** No clock, no
  randomness, no I/O.

## Notes

- A deliberate divergence from the reference is recorded in `l0.rs`:
  the reference calls back into the strategy *during* matching, so a
  strategy can cancel an order while it is being processed. Here
  callbacks are outputs applied afterwards, and re-entrant mutation is
  not representable. Parity against real strategies must confirm this
  path is not exercised.
- Zero as the market-order sentinel stops at the boundary. Inside the
  type system, `OrderKind::Market` is explicit and cannot be produced by
  a mistake in price arithmetic.
