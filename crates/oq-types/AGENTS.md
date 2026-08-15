# oq-types

The vocabulary every other crate speaks: fixed-point money, the
typestate order lifecycle, dual timestamps, identifiers.

## Commands

```bash
cargo test -p oq-types
cargo clippy -p oq-types --all-targets -- -D warnings
```

## Invariants

- **Money is integers.** Prices count ticks, quantities count lots,
  cash counts 1e-8 units, ratios count parts per billion. Floating
  point enters only at the reporting boundary. Two runs that agree must
  agree *exactly*, and floating-point addition is not associative while
  compilers are free to reassociate.
- **Illegal order transitions must not compile.** `Order<Filled>` has
  no `cancel`. Transitions consume `self`. If a change makes an illegal
  transition expressible, the change is wrong even if the tests pass.
- **Over-fills are refused, never clamped.** An over-fill means the
  caller's view of the book disagrees with the book; clamping hides
  that until it surfaces as a position break.
- **Both timestamps are required.** A constructor that defaulted one
  would be used, and the resulting data would be indistinguishable from
  correct data until someone tried to model latency with it.
- **Serialized layouts are append-only.** A field never changes
  meaning between versions.

## Notes

- Cash arithmetic saturates rather than wraps. A balance pinned at the
  maximum is an obviously wrong number that fails the next assertion; a
  wrapped balance is a plausible-looking number that does not.
- `IdAllocator` is deliberately not thread-safe and not global. Ids are
  assigned inside the deterministic core, where there is one writer; a
  shared atomic would make id assignment depend on thread interleaving
  and destroy replay reproducibility.
- Compile-fail expectations for the typestate are documented in
  `order.rs` tests. A `trybuild` suite should pin them once the crate
  has a dev-dependency budget.
