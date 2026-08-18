# OpenQuanter Documentation

[English](README.md) · [中文](README.zh-CN.md)

Project documentation is authored in English and mirrored in Chinese. Each
document links to its counterpart at the top.

| Document | Contents |
|---|---|
| [**Why OpenQuanter exists**](WHY.md) · [中文](WHY.zh-CN.md) | **What it is for, who for, the wall a predecessor hit after years live, and where it stands** |
| [Quickstart](QUICKSTART.md) · [中文](QUICKSTART.zh-CN.md) | Clone to a running backtest, three worked examples, how to write your own strategy |
| [Versioning](VERSIONING.md) · [中文](VERSIONING.zh-CN.md) | One version across the workspace, why it starts at 2, and what each stage promises |
| [Requirements Specification](REQUIREMENTS.md) · [中文](REQUIREMENTS.zh-CN.md) | Positioning, users, functional and non-functional requirements, acceptance goals, fidelity ladder, non-goals |
| [Roadmap](ROADMAP.md) · [中文](ROADMAP.zh-CN.md) | Milestones M0–M5 and 2.0, entry triggers and exit gates, release cadence, risk register |
| [Implementation Plan](IMPLEMENTATION.md) · [中文](IMPLEMENTATION.zh-CN.md) | Architecture, design decisions, crate map, phase-by-phase task plan, testing strategy, performance budgets |
| [Tick Format](TICK-FORMAT.md) · [中文](TICK-FORMAT.zh-CN.md) | The on-disk format a backtest reads: layout, the append-only field rule, integrity versus identity. **§4 onward specifies a proposed v3**; `oq-data` implements v2 |
| [Run Format](RUN-FORMAT.md) · [中文](RUN-FORMAT.zh-CN.md) | **What a run produced and the identity it produced it under, in one file** — why the manifest is inside it, and why a truncated baseline is refused rather than compared |
| [Cutover](CUTOVER.md) · [中文](CUTOVER.zh-CN.md) | **Moving a live strategy between systems without flattening the position**: the one-system invariant, the exposed interval, and what an abort is — a skeleton, unrehearsed, and explicit about which |
| [Margin Fidelity](MARGIN-FIDELITY.md) · [中文](MARGIN-FIDELITY.zh-CN.md) | **How wrong a backtest with no margin model is**, why the answer is a cross-window tail rather than a mean, and which of its numbers survive a change of window mix |
| [Execution](EXECUTION.md) · [中文](EXECUTION.zh-CN.md) | The venue-independent order contract: the three-state outcome, client order ids, and what a lost answer costs |
| [Live Path](LIVE-PATH.md) · [中文](LIVE-PATH.zh-CN.md) | Journal-before-send, recovery from a killed process, and what the supervisor is allowed to decide |
| [Capture Format](CAPTURE-FORMAT.md) · [中文](CAPTURE-FORMAT.zh-CN.md) | Record framing, control records, daily sealing, archival verification, volume planning |
| [Changelog](../CHANGELOG.md) · [中文](../CHANGELOG.zh-CN.md) | What changed; where a semantics or event-schema change must be recorded |

## Reading order

New here? Read the [README](../README.md) for the one-page pitch, then the
**Requirements** document for what the framework must do, the **Roadmap** for
when, and the **Implementation Plan** for how.

Contributing? Start with the [Implementation Plan](IMPLEMENTATION.md) §5 task
list and §6 testing strategy, plus [CONTRIBUTING.md](../CONTRIBUTING.md) and
[AGENTS.md](../AGENTS.md).

## Document status

These documents do not all describe the same thing, and mixing them up is
the easiest way to misread the project:

| Document | Describes |
|---|---|
| Quickstart, Versioning, Changelog | **What exists.** Commands that run today; numbers pinned by `crates/oq-examples/tests/golden.rs` |
| Capture Format | **What is implemented**, by `oq-l2feed` |
| Tick Format | §1–§3 what `oq-data` implements (v2); §4 onward a **proposed v3** |
| Requirements, Roadmap, Implementation Plan | **Intent.** Drafts for review — what the framework must do and how it will be built, not what is shipped |

For the built/not-built split, the [README](../README.md) Status section is
the single answer. Discussion and corrections are welcome as issues.

Translation policy: English is the source of truth. When the two versions
disagree, the English text governs and the Chinese version is a bug.
