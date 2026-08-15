# oq-backtest

The backtest host, and the margin deviation report.

## Commands

```bash
cargo test -p oq-backtest
cargo clippy -p oq-backtest --all-targets -- -D warnings
```

## Invariants

- **Strategies return intents; they do not place orders.** They have no
  reference to the engine, no clock, and no I/O. Hard limits are
  enforced between the intent and the book, never by asking a strategy
  to behave.
- **The two arms of a deviation comparison must differ in exactly one
  thing**: whether the venue is allowed to close the account. Same
  strategy type, same ticks, same table, same fees. A fresh strategy
  instance per arm — sharing one would leak the first run's state into
  the second and silently invalidate the comparison.
- **The control arm is `State::without_liquidation`, not a zeroed
  margin table.** A zero table still closes the account at zero equity;
  a margin-free backtest holds through arbitrary drawdown and reports
  the recovery. Getting this wrong makes the report say "no difference"
  when there is one.
- **The report produces no single adjusted number.** Blending a real
  result with an impossible one yields a number describing neither.
- **A run is deterministic.** Same inputs, same result, any machine.
  There is a test asserting it; keep it.

## Notes

- Equity below zero in the margin-free arm is the tell that the account
  it describes never existed. The report counts fills that happened
  after the point a real account would have been closed.
- `CoverLadder` in the tests is a generic teaching example of the
  strategy family this report exists for — averaging down looks
  excellent without a margin model. It is not anyone's production
  strategy and must not become one.
