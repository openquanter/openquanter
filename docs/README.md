# OpenQuanter Documentation

[English](README.md) · [中文](README.zh-CN.md)

Project documentation is authored in English and mirrored in Chinese. Each
document links to its counterpart at the top.

| Document | Contents |
|---|---|
| [Quickstart](QUICKSTART.md) · [中文](QUICKSTART.zh-CN.md) | Clone to a running backtest, three worked examples, how to write your own strategy |
| [Requirements Specification](REQUIREMENTS.md) · [中文](REQUIREMENTS.zh-CN.md) | Positioning, users, functional and non-functional requirements, acceptance goals, fidelity ladder, non-goals |
| [Roadmap](ROADMAP.md) · [中文](ROADMAP.zh-CN.md) | Milestones M0–M5 and 1.0, entry triggers and exit gates, release cadence, risk register |
| [Implementation Plan](IMPLEMENTATION.md) · [中文](IMPLEMENTATION.zh-CN.md) | Architecture, design decisions, crate map, phase-by-phase task plan, testing strategy, performance budgets |
| [Tick Format](TICK-FORMAT.md) · [中文](TICK-FORMAT.zh-CN.md) | The on-disk format a backtest reads: layout, the append-only field rule, integrity versus identity |
| [Capture Format](CAPTURE-FORMAT.md) · [中文](CAPTURE-FORMAT.zh-CN.md) | Record framing, control records, daily sealing, archival verification, volume planning |

## Reading order

New here? Read the [README](../README.md) for the one-page pitch, then the
**Requirements** document for what the framework must do, the **Roadmap** for
when, and the **Implementation Plan** for how.

Contributing? Start with the [Implementation Plan](IMPLEMENTATION.md) §5 task
list and §6 testing strategy, plus [CONTRIBUTING.md](../CONTRIBUTING.md) and
[AGENTS.md](../AGENTS.md).

## Document status

All three documents are **drafts for review**. They describe intent, not
shipped functionality — the workspace is currently a pre-alpha skeleton.
Discussion and corrections are welcome as issues.

Translation policy: English is the source of truth. When the two versions
disagree, the English text governs and the Chinese version is a bug.
