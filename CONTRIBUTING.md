# Contributing to OpenQuanter

Thanks for your interest! The project is in early development; the best ways
to help right now are trying it, filing precise issues, and discussing design.

## Ground rules

- **License**: Apache-2.0. All contributions are accepted under it.
- **Sign-off**: every commit must carry a DCO sign-off (`git commit -s`),
  certifying you have the right to submit the code.
- **CLA**: for substantial contributions we may ask you to sign a
  Contributor License Agreement before merging.
- **Determinism is sacred**: changes inside the event core must not
  introduce wall-clock reads, RNG, I/O, or threading. CI enforces the
  property-test invariants; golden baselines change only with maintainer
  confirmation.
- **No proprietary content**: do not submit exchange credentials, collected
  market data, or trading strategies containing live parameters.

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Support is best-effort. Please search existing issues before opening new ones.
