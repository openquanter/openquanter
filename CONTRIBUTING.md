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
cargo test
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` and not `--workspace`: the workspace variant also builds
`oq-py`, whose tests link against a CPython shared library and fail on
a machine whose Python does not match. The bindings have their own CI
job with a pinned interpreter — `cargo clippy -p oq-py` and
`cargo test -p oq-py` — and running them locally needs that interpreter
present.

Documentation is authored in English and mirrored in Chinese (`*.zh-CN.md`).
English is the source of truth; update both sides in the same pull request.

## Branches and merging

Trunk-based: `main` is always releasable, and branches are short-lived.

| | |
|---|---|
| **Naming** | `feat/`, `fix/`, `docs/`, `chore/` + a short subject |
| **Lifetime** | Merge within a day or two |
| **Merging** | Squash, so one pull request becomes one commit on `main` |
| **After merge** | The branch is deleted automatically |

A branch's cost grows with its age, and not linearly. A day-old branch
rebases cleanly; a week-old one has to be reconciled against work that
assumed it did not exist, by someone who has forgotten why it was written.
If a change is too large to land in a couple of days, land it in pieces
behind whatever makes each piece safe on its own.

`main` is protected: every change arrives by pull request with an approving
review from a code owner, and CI must pass before it can merge. This applies
to everyone; maintainers are not exempt from review by convention, only by
mechanism, and the convention is the part that matters.

Keep one pull request to one concern. A review that has to hold three
unrelated changes in mind finds fewer problems in all three.

## Tracking work

Anything worth doing later belongs in an issue, not in a conversation.
A decision that lives only in a chat log is invisible to the next person
and to your future self, and it will be made again — differently.

## Where to start

The [roadmap](docs/ROADMAP.md) lists what each milestone unlocks, and the
[implementation plan](docs/IMPLEMENTATION.md) breaks it into tasks with
completion criteria. The most useful contributions right now are precise bug
reports with reproduction seeds, venue adapters, simulation scenarios
describing generic failure modes, and documentation.

Support is best-effort, and there is no SLA before 2.0. Please search existing
issues before opening new ones.
