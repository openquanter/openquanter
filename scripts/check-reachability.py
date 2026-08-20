#!/usr/bin/env python3
"""Report public API that nothing in the workspace reaches.

    scripts/check-reachability.py            # print the report
    scripts/check-reachability.py --check    # fail on anything not allowed

Why this exists
---------------
Six defects in two days had one shape: **one half present, the other
half absent, and the absent half silent.**

  Record::Reconciled          matched in one binary, constructed nowhere
  windows_before_first_trade  a counter with no assignment on one path
  Tick::volume                journalled and never rendered
  Shadow                      built, tested, never constructed
  Books::on_submit            implemented, tested, never called
  PboReport's four extras     computed, discarded at `.map(|r| r.pbo)`

Each was found by accident while looking at something else. None was
found by a test, because every one of them had passing tests: the unit
was right and nothing invoked it. `dead_code` says nothing either — the
items are `pub`, and a library is allowed to export things it does not
use itself.

That last point is why this **reports** rather than fails by default. A
published crate has consumers, and unreachable-from-here is not the same
as dead. What it is, always, is a claim: someone wrote this and nothing
demonstrates it works end to end. The allowlist is where that claim gets
written down with a reason, so the next person meets a sentence instead
of a silence.

What it looks for
-----------------
- `pub enum` variants that are matched but never constructed
- `pub struct` fields that nothing reads

**Not** `pub fn` that nothing calls. That was tried: fifty-two findings,
almost all of them library functions a consumer is supposed to call, to
surface one real gap. A check whose output nobody triages is a check
somebody disables, and the two kept here run at about one real finding
in four — `windows_before_first_trade`, `PboReport::performance_degradation`,
`FeedLatency::negative` and `Trade::commission` would each have shown.

What it deliberately does not look for: anything needing type resolution.
This is text. It over-reports, it says so, and every entry it prints is
either a finding or an allowlist line — never a silent pass.
"""

import re
import subprocess
import sys
from pathlib import Path

ALLOW = Path("scripts/reachability-allow.txt")


def load_allow():
    """`{entry: reason}` from the allowlist."""
    if not ALLOW.exists():
        return {}
    out = {}
    for line in ALLOW.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, reason = line.partition("#")
        out[name.strip()] = reason.strip()
    return out


def sources():
    files = subprocess.run(
        ["git", "ls-files", "*.rs"], capture_output=True, text=True, check=True
    ).stdout.split()
    return {f: Path(f).read_text(encoding="utf-8") for f in files}


def strip_tests(text):
    """Drop `#[cfg(test)]` modules.

    A test is not a use. Every one of the six defects above had tests
    that passed, and counting them as reachability is what let the
    absence stay silent.
    """
    out, depth, skipping = [], 0, False
    for line in text.split("\n"):
        if not skipping and re.match(r"\s*#\[cfg\(test\)\]", line):
            skipping, depth = True, 0
            continue
        if skipping:
            depth += line.count("{") - line.count("}")
            if depth <= 0 and "}" in line:
                skipping = False
            continue
        out.append(line)
    return "\n".join(out)


def variants(src):
    for f, s in src.items():
        for m in re.finditer(r"pub enum (\w+)\s*\{(.*?)\n\}", s, re.S):
            name, body = m.group(1), m.group(2)
            for line in body.split("\n"):
                line = line.strip()
                if not line or line.startswith(("//", "#")):
                    continue
                v = re.match(r"([A-Z]\w*)", line)
                if v:
                    yield f, name, v.group(1)


def constructed(text, enum, variant):
    """Whether the variant appears anywhere that is not a pattern.

    A construction is `Enum::V` followed by `(`, `{`, or the end of an
    expression; a pattern is the same thing to the left of `=>` or after
    `matches!`, `if let`, `Some(`. Distinguishing them exactly needs a
    parser. This asks the weaker question — does it appear on a line
    with no `=>` after it and no pattern keyword before it — and the
    weakness is why the result is a report.
    """
    pat = re.compile(r"(?<![A-Za-z_])(?:" + enum + r"|Self)::" + variant + r"(?![A-Za-z0-9_])")
    for line in text.split("\n"):
        m = pat.search(line)
        if not m:
            continue
        after = line[m.end():]
        before = line[: m.start()]
        if "=>" in after and "=>" not in before:
            continue  # match arm
        if re.search(r"\b(matches!|if let|while let)\b", before):
            continue
        if before.strip().startswith("use "):
            continue
        return True
    return False


def fields(src):
    """`pub` fields of `pub struct`s."""
    for f, s in src.items():
        for m in re.finditer(r"pub struct (\w+)\s*\{(.*?)\n\}", s, re.S):
            name, body = m.group(1), m.group(2)
            for fm in re.finditer(r"^\s*pub (\w+):", body, re.M):
                yield f, name, fm.group(1)


def read(text, field):
    """Whether the field is ever read as `.field`.

    Written-only counts as unread on purpose. `windows_before_first_trade`
    was assigned on one path and printed by nobody, and
    `PboReport::performance_degradation` was computed on every sweep and
    dropped at the call site — both would have shown here.
    """
    for line in text.split("\n"):
        for m in re.finditer(r"\." + field + r"(?![A-Za-z0-9_])", line):
            after = line[m.end():].lstrip()
            if after.startswith("=") and not after.startswith("=="):
                continue  # a write
            return True
    return False


def main():
    check = "--check" in sys.argv
    src = sources()
    prod = {f: strip_tests(s) for f, s in src.items()}
    joined = "\n".join(prod.values())
    allow = load_allow()

    findings = []
    for f, enum, v in variants(src):
        key = f"{enum}::{v}"
        if not constructed(joined, enum, v):
            findings.append((key, f, "matched but never constructed outside tests"))

    for f, st, fld in fields(src):
        if not read(joined, fld):
            findings.append((f"{st}.{fld}", f, "written or computed and never read outside tests"))

    unexplained = [x for x in findings if x[0] not in allow]

    for key, f, why in findings:
        mark = "allowed" if key in allow else "FINDING"
        note = f"  # {allow[key]}" if key in allow else ""
        print(f"{mark:8} {key:44} {why}{note}")
        if mark == "FINDING":
            print(f"{'':8} {f}")

    print()
    print(
        f"reachability: {len(findings)} item(s) unreachable from the workspace, "
        f"{len(findings) - len(unexplained)} explained"
    )
    if unexplained and check:
        print()
        print("Each of these is a claim nothing demonstrates. Either reach it,")
        print(f"remove it, or add a line to {ALLOW} saying why it stays.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
