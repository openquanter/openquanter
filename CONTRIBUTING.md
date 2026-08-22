# Contributing to OpenQuanter

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

Thanks for your interest! The project is in early development; the best ways
to help right now are trying it, filing precise issues, and discussing design.

## Ground rules

- **License**: Apache-2.0. All contributions are accepted under it.
- **Sign-off**: every commit must carry a DCO sign-off (`git commit -s`),
  certifying you have the right to submit the code. CI checks this on every
  pull request; run the same check locally with:

  ```bash
  scripts/check-dco.sh              # origin/main..HEAD
  git rebase --signoff origin/main  # fix a branch that is missing sign-offs
  ```

- **CLA**: for substantial contributions we will ask you to agree to the
  [Contributor License Agreement](CLA.md) ([中文参考译文](CLA.zh-CN.md)).
  It is a licence, not a copyright assignment — you keep your copyright.
- **Determinism is sacred**: changes inside the event core must not
  introduce wall-clock reads, RNG, I/O, or threading. CI enforces the
  property-test invariants; golden baselines change only with maintainer
  confirmation.
- **No proprietary content**: do not submit exchange credentials, collected
  market data, or trading strategies containing live parameters.

## Development

Rust 2024 edition; minimum supported Rust version is 1.85.

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Documentation is authored in English and mirrored in Chinese (`*.zh-CN.md`).
English is the source of truth; update both sides in the same pull request.

## Where to start

The [roadmap](docs/ROADMAP.md) lists what each milestone unlocks, and the
[implementation plan](docs/IMPLEMENTATION.md) breaks it into tasks with
completion criteria. The most useful contributions right now are precise bug
reports with reproduction seeds, venue adapters, simulation scenarios
describing generic failure modes, and documentation.

Support is best-effort. Please search existing issues before opening new ones.
