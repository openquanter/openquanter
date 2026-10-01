# Versioning

> [中文版](VERSIONING.zh-CN.md)

One number answers "what version is this". This document exists because
for a while three did.

## The scheme

Every crate in the workspace carries the same version, declared once in
the root `Cargo.toml` and inherited with `version.workspace = true`.
Internal dependencies are declared once in `[workspace.dependencies]`,
so a bump is a single edit and cannot drift between crates.

```
2.0.0, 2.0.1, 2.0.2, …   one release after another, each a tag vX.Y.Z
```

A release is a number in a sequence and a tag on `main`. The next one
is the last one plus one in the third place. The number says *which*
release, and nothing more: there is no alpha, no beta, and no promise
encoded in the digits.

The crates are one release train. They are separable — using one without
the others is a supported and tested property (G0) — but they are
developed, tested and released together, and a reader comparing two
checkouts should not have to reconcile twenty-three numbers.

## Why 2, when nothing public was ever 1

OpenQuanter 1.x is a closed-source trading platform that ran live for
several years. It was never published and never will be. This project is
its successor, rewritten from scratch, and calling it 2 is simply
accurate about that.

The consequence is a gap in the public record: crates.io and this
repository begin at 2.0, and there is no public 1.x to find.
That is stated here rather than left as a puzzle, because a missing
major version otherwise reads as a mistake.

## What a release promises

That it builds, passes the full gate, and is described. Every release
is cut from a green `main`; its notes are its section of the
[changelog](../CHANGELOG.md); and any change to L0 matching semantics,
margin computation or the event schema is called out there.

It does not promise API stability. Any public API may change from one
release to the next, without a deprecation period, until the
[roadmap](ROADMAP.md#api-stability)'s API-stability milestone is
reached. That commitment is a milestone the documentation will
announce, not something read off the version number — so a reader does
not have to decode digits to learn it.

Earlier drafts of this page used `2.0.0-alpha.N` for exactly that
signal. It was dropped on 2026-10-01 for a plain sequence: one scheme
the project actually cuts releases under beats a richer one it never
did. The single pre-release ever published, PyPI `2.0.0a1`
(2026-08-18), sorts before `2.0.0` and stays where it is.

## Things that version separately, on purpose

Not everything follows the crate version, because not everything changes
with it:

| Artifact | Versioning | Where |
|---|---|---|
| Capture file format | `format_version` in the manifest, currently 2 (readers still accept 1) | [Capture Format](CAPTURE-FORMAT.md) |
| Tick file format | `version` in the file header, currently 2 | [Tick Format](TICK-FORMAT.md) |
| Journal frame format | `VERSION` in the frame header | `oq-journal` |
| Run file format | `openquanter-run N` on the first line, currently 1 | [Run Format](RUN-FORMAT.md) |
| Sweep file format | `openquanter-sweep N` on the first line, currently 1 | [Sweep Format](SWEEP-FORMAT.md) |

A data format outlives the code that wrote it. Tying a format version to
a crate version would mean either bumping the format on every release,
which makes the number meaningless, or leaving it behind, which makes it
a lie. They are separate numbers because they answer separate questions:
the crate version says what API you are compiling against, and the
format version says what a file on disk contains.

## Where releases go

| Artifact | Registry | State |
|---|---|---|
| Release notes, Linux binaries | [GitHub Releases](https://github.com/openquanter/openquanter/releases) | One per tag, from `2.0.0` |
| `openquanter` (Python) | [PyPI](https://pypi.org/project/openquanter/) | Published by the release workflow from `2.0.0`; `2.0.0a1` was uploaded by hand |
| `oq-*` (Rust) | crates.io | Names reserved, nothing published |

The Python package leads, and the Rust crates trail on purpose. A binding
has a small, deliberately-chosen surface — the statistics and the strategy
tier — and its users are people evaluating whether this is worth their time.
A crate exposes every public type in the workspace, and the workspace's
types are still moving. Publishing them now would pin people to a version
about to change under them, and a version yanked from crates.io is still a
version somebody built against.

**When the crates go up.** Not on a date. The condition is the one this
section already gives: the workspace's types stop moving. That is the
roadmap's API-stability milestone — it puts API stabilisation after M3
and after external adoption — so the crates publish when the API they expose is
one somebody can build against without being moved off it. Until then
the install path is `git clone`, and
[Quickstart](QUICKSTART.md#1-build) says so rather than offering a
`cargo install` that succeeds and delivers an empty crate.

That contradiction existed. This page said *nothing published* while
the quickstart listed five `cargo install` lines as the first thing to
do, and a reader following the quickstart got placeholders and no
error. Two documents, opposite claims about the same fact.

Note that a PyPI version cannot be re-uploaded either. Every version is
permanent, which is why the metadata that ships with it — the description,
the README, the classifiers — is checked before the upload rather than
corrected after, and why the release workflow builds and tests the wheels
before anything is published.

## Changing the version

Edit `[workspace.package].version` and the versions in
`[workspace.dependencies]` in the root `Cargo.toml`. Nothing else. If
you find yourself editing a version in `crates/*/Cargo.toml`, something
has drifted back and should be pointed at the workspace again.

## Cutting a release

1. In a pull request: set the version as above, and rename the
   changelog's `Unreleased` heading to `X.Y.Z — <date>` in both languages,
   opening a new empty `Unreleased` above it.
2. Merge it, then tag that commit on `main` and push the tag:
   `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`.
3. `.github/workflows/release.yml` does the rest, and refuses if any step
   disagrees: the tag must equal the workspace version, the commit must be
   on `main`, the full gate must pass, and the changelog must have a
   section for the version. It then builds the Linux binaries and the
   wheels, creates the GitHub Release with that section as its notes and
   the binaries and checksums attached, and publishes the wheels to PyPI
   by trusted publishing — no token is stored anywhere.

A tag that fails the workflow publishes nothing. Fix it on `main` and cut
the next number; a version number, once tagged, is not reused.
