# oq-examples

Teaching examples, and the seeded synthetic market they run on.
Unpublished (`publish = false`): examples are documentation, not a
library anyone should depend on.

## Commands

```bash
cargo run --example hello
cargo run --example ma_cross
cargo run --example martingale_ladder
cargo test -p oq-examples          # includes the golden tests
```

## Invariants

- **Every example is expected to lose money, and none is a strategy to
  run.** Each demonstrates one property of the framework. An example
  tuned to show a pleasant equity curve would teach the wrong lesson
  from a project whose central claim is that backtests flatter you. If
  you find yourself adjusting a parameter because the result looks
  better, stop — that is the search `oq-stats` exists to penalise, and
  doing it here teaches it as good practice.
- **The printed numbers are pinned by `tests/golden.rs`,** because the
  documentation and the video course quote them. A golden failure means
  either the engine's behaviour changed or the change was intended, and
  telling those apart is the whole point. Never relax an assertion to
  make it pass; investigate, then update the constants *and* every place
  that quotes them, in the same commit.
- **The market is generated, never captured.** Exchange data carries
  redistribution terms a public repository cannot take back; a seeded
  series is identical on every machine, which is what golden tests need;
  and a crash of exactly the depth a lesson requires can be scripted
  rather than waited for.
- **The examples run with no trading fees, and that is a known
  omission, not an oversight.** None of them calls
  `RunConfig::with_fees`, so every quoted number is gross of costs. The
  numbers are pinned as they are because the documentation and the video
  course quote them; changing the fee schedule changes all of them at
  once. If you add a fee-charging example, add it *alongside* these
  rather than editing one, and give it its own golden coverage. Saying
  so here matters more than it would elsewhere: this is the crate for a
  project whose central claim is that backtests flatter you.
- **SplitMix64, not a bare LCG.** An LCG consumed at a fixed stride
  leaks lattice structure into anything generated in a loop, which is
  how synthetic "noise" acquires structure nobody intended. This bit the
  project once already; see `crates/oq-stats/AGENTS.md`.

## When adding an example

Ask what property it demonstrates. If the answer is "it shows the
framework works", `hello` already does that. If the answer is "it makes
money", it does not belong here.

Add golden coverage in the same commit. An example whose output is not
pinned will drift, and the first person to notice will be a viewer
following the video course who cannot reproduce what they were shown.
