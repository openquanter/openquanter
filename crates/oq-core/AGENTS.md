# oq-core

The deterministic kernel, the journal-first sequencer, and exact replay.
Backtest, sandbox, and live differ only in who produces events.

## Commands

```bash
cargo test -p oq-core
cargo clippy -p oq-core --all-targets -- -D warnings
```

## Invariants

- **No ambient authority in the kernel.** No clock reads, no RNG, no
  I/O, no threads inside `apply`. Time arrives as `Event::Time`. Every
  one of these is a replay divergence waiting to happen, and a replay
  that diverges is not debuggable — it is a second mystery on top of
  the first.
- **Journal before apply.** `Sequencer::submit` records the event
  durably and only then calls the kernel. Never reorder these. The
  inverse leaves a window in which the core acted on an event that does
  not exist after a crash, and every artifact derived from the journal
  is then quietly wrong.
- **Outputs are values, never callbacks.** Nothing may re-enter the
  kernel mid-decision. If a caller needs to react to a fill, it reacts
  to the returned `Output`.
- **Event discriminants are permanent.** `event::kind` values are
  never reused or renumbered; a journal outlives the build that wrote
  it. New events take new numbers.
- **Decode is strict.** A payload of the wrong length is refused, not
  best-effort parsed. `ReplayResult::undecodable` being non-zero means
  the reconstruction is incomplete and callers must be able to see it.
- **Liquidation is checked on every tick and after every funding
  settlement.** A position can be ended by financing on a path price
  alone would have survived.

## Notes

- `State::without_liquidation()` exists for the control arm of the
  margin deviation experiment. It is not a performance option and not a
  "simple mode": it models an account with unlimited collateral, which
  no venue offers. Zeroing the maintenance table is *not* equivalent —
  such an account is still closed at zero equity.
- The ledger's `apply_fill` handles the case a naive implementation
  gets wrong: a fill crossing through flat realizes on the closed leg
  only, and the new position's entry is the fill price rather than a
  blend with the side just closed.
- The determinism claim is a test, not a comment:
  `sequencer::tests::a_replay_reproduces_the_run_exactly` asserts that
  replaying a journal into a fresh kernel reproduces both the output
  sequence and the final account state, and
  `a_liquidation_is_reproduced_by_replay` covers the path that matters
  most.
