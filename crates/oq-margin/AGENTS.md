# oq-margin

Tiered maintenance margin, liquidation pricing, and funding. An
orthogonal overlay: any fidelity tier composes with it.

## Commands

```bash
cargo test -p oq-margin
cargo clippy -p oq-margin --all-targets -- -D warnings
```

## Invariants

- **The liquidation price must be a tick at which the position is
  actually liquidatable.** Floor for a long (closed on the way down),
  ceiling for a short (on the way up). The tests pin this from both
  sides — the reported tick is liquidatable, the tick one step safer is
  not. A half-tick error here is invisible in aggregate and decisive on
  the one path that matters.
- **Brackets are resolved at the mark, never frozen at entry.** A
  falling market moves a long into a lower bracket and a rising one
  into a higher bracket; that is what the venue does.
- **The maintenance amount exists to keep the requirement continuous**
  across a bracket boundary. Dropping it as an implementation detail
  produces a discontinuity and liquidates positions the venue would not.
- **Maintenance is never negative.** A malformed table must read as a
  zero requirement, not as collateral the account does not have.
- **Rules are bitemporal.** `TierSchedule::at` resolves by *event*
  time. A query before the earliest table returns `None` rather than
  extrapolating a margin regime backwards — applying today's rules to
  old data is the same family of error as survivorship bias.
- **Funding sign follows the side.** Positive rate: longs pay. The
  amount is what happens to the position's collateral, so a long's is
  negative. Windows are half-open on the left so nothing settles twice.

## Notes

- `TierTable::example_btcusdt` is named for what it is. It is a worked
  example and a test fixture, not an authoritative schedule; production
  runs load a dated table through `TierSchedule`.
- The liquidation-price derivation is written out in `position.rs` so
  the implementation can be checked against reasoning rather than
  against a copied constant.
