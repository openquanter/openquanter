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
2.0.0-alpha.N   now — APIs change without notice
2.0.0-beta.N    from the beta milestone — APIs documented, still moving
2.0.0           APIs stable; semantic versioning enforced from here
```

The crates are one release train. They are separable — using one without
the others is a supported and tested property (G0) — but they are
developed, tested and released together, and a reader comparing two
checkouts should not have to reconcile twelve numbers.

## Why 2, when nothing public was ever 1

OpenQuanter 1.x is a closed-source trading platform that ran live for
several years. It was never published and never will be. This project is
its successor, rewritten from scratch, and calling it 2 is simply
accurate about that.

The consequence is a gap in the public record: crates.io and this
repository begin at 2.0.0-alpha, and there is no public 1.x to find.
That is stated here rather than left as a puzzle, because a missing
major version otherwise reads as a mistake.

## Why a pre-release tag rather than 0.x

Both conventions communicate instability. `0.x` is the usual Rust
choice; `2.0.0-alpha.N` was chosen here because the alternative would
have meant two numbering systems at once — a project describing itself
as "2.x" whose crates say `0.0.1` and whose roadmap points at "1.0". A
reader had no way to tell which number was the answer.

Cargo's pre-release semantics are also the behaviour we want.
`2.0.0-alpha.1 < 2.0.0`, and `cargo add oq-core` will not select a
pre-release unless asked. Software that changes APIs without notice
should require an explicit request, and this makes that the default
rather than a warning in a README.

## What each stage promises

| Stage | Promise |
|---|---|
| `2.0.0-alpha.N` | Nothing. Any API may change in any release. Changes appear in the [changelog](../CHANGELOG.md); there is no deprecation period |
| `2.0.0-beta.N` | APIs documented and unlikely to move, but breaking changes are still allowed and will be called out |
| `2.0.0` | Public crate APIs and the Python binding surface are stable. Breaking changes require a major version |

Reaching `2.0.0` is a commitment, not a feature count. Its conditions
are in the [roadmap](ROADMAP.md#road-to-20).

## Things that version separately, on purpose

Not everything follows the crate version, because not everything changes
with it:

| Artifact | Versioning | Where |
|---|---|---|
| Capture file format | `format_version` in the manifest, currently 1 | [Capture Format](CAPTURE-FORMAT.md) |
| Tick file format | `version` in the file header, currently 2 | [Tick Format](TICK-FORMAT.md) |
| Journal frame format | `VERSION` in the frame header | `oq-journal` |

A data format outlives the code that wrote it. Tying a format version to
a crate version would mean either bumping the format on every release,
which makes the number meaningless, or leaving it behind, which makes it
a lie. They are separate numbers because they answer separate questions:
the crate version says what API you are compiling against, and the
format version says what a file on disk contains.

## Where releases go

| Artifact | Registry | State |
|---|---|---|
| `openquanter` (Python) | [PyPI](https://pypi.org/project/openquanter/) | Published, `2.0.0a1` |
| `oq-*` (Rust) | crates.io | Names reserved, nothing published |

The Python package leads, and the Rust crates trail on purpose. A binding
has a small, deliberately-chosen surface — the statistics and the strategy
tier — and its users are people evaluating whether this is worth their time.
A crate exposes every public type in the workspace, and the workspace's
types are still moving. Publishing them now would pin people to a version
about to change under them, and a version yanked from crates.io is still a
version somebody built against.

Note that a PyPI version cannot be re-uploaded either. `2.0.0a1` is
permanent, which is why the metadata that ships with it — the description,
the README, the classifiers — is checked before the upload rather than
corrected after.

## Changing the version

Edit `[workspace.package].version` and the versions in
`[workspace.dependencies]` in the root `Cargo.toml`. Nothing else. If
you find yourself editing a version in `crates/*/Cargo.toml`, something
has drifted back and should be pointed at the workspace again.
