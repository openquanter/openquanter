# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.

## Project shape

- Cargo workspace; crates live under `crates/`. Each crate has its own
  `AGENTS.md` with local commands and invariants (nearest file wins).
- Language: Rust stable. Code comments in English.
- The event core must stay deterministic: no wall-clock reads, no RNG,
  no I/O, no thread spawning inside `apply()`-style state machines.
  Time arrives as injected events.

## Documentation

- Intent: [Requirements](docs/REQUIREMENTS.md) · [Roadmap](docs/ROADMAP.md) ·
  [Implementation Plan](docs/IMPLEMENTATION.md).
- What exists: [Quickstart](docs/QUICKSTART.md) ·
  [Versioning](docs/VERSIONING.md) ·
  [Capture Format](docs/CAPTURE-FORMAT.md) ·
  [Tick Format](docs/TICK-FORMAT.md) (§4 onward is a proposal).
- Full index: [docs/](docs/README.md). The built/not-built split lives in
  one place — the [README](README.md) Status section. Do not restate it
  elsewhere; two copies is how a status section goes stale.
- Docs are authored in English and mirrored in Chinese (`*.zh-CN.md`).
  English is the source of truth; update both sides in the same pull request.

## Commands

```bash
cargo build --workspace
cargo test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` and not `--workspace`: the workspace variant also builds
`oq-py`, whose tests link against a CPython shared library and fail on
a machine whose Python does not match. The bindings have their own CI
job with a pinned interpreter — `cargo clippy -p oq-py` and
`cargo test -p oq-py` — and running them locally needs that interpreter
present.

## Verification anchors

- Engine invariants — quantity conservation, price-time priority, no
  crossed book after matching, non-negative margin usage — are encoded
  as tests beside the code they constrain. Never weaken an invariant to
  make a test pass.
- These are hand-written cases today, not generated ones. `proptest` is
  the intended vehicle and is **not yet a dependency**; adopting it is a
  decision to make deliberately, because the engine crates hold a
  zero-dependency budget (`scripts/check-composability.sh`). Until then,
  a new invariant means new explicit cases, including the adversarial
  ones a generator would have found.
- Golden tests replay sample data and compare full output. Golden baselines
  may only be regenerated with explicit human confirmation in the PR.

## Working on this repository

Branch naming, branch lifetime, squash merges and the review requirement
are in [CONTRIBUTING.md](CONTRIBUTING.md#branches-and-merging) — the same
rules apply to agents and to people, and they live in one place so they
cannot drift apart.

Two that are easy to get wrong from a terminal:

- **Run the full local gate before pushing**, not part of it. `cargo fmt
  --all -- --check` is the one usually skipped, and generated or
  script-edited Rust is what trips it. Then confirm the run actually went
  green (`gh run list --limit 1`) rather than assuming.
- **Never push straight to `main`.** Open a pull request even when your
  token could bypass the protection.

## Boundaries

- This is the public framework repository. Production strategies, exchange
  credentials, proprietary parameters, and collected market data belong to
  private overlay repositories and must never be committed here.
- Never repurpose fields in serialized event formats; add new ones.
