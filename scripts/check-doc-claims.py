#!/usr/bin/env python3
"""Check every hand-written dependency claim in the documentation.

    scripts/check-doc-claims.py

Why this exists, and why doc-facts.sh was not enough
---------------------------------------------------
`doc-facts.sh` generates a dependency block and compares it. That works
where a document embeds the block, and it did nothing for the sentences
stating the same facts in prose somewhere else. On 2026-08-19
`docs/ROADMAP.md` said at line 100 that `oq-live` depends on neither
`oq-core` nor `oq-margin`, and at line 449 that it depends on both. One
document, two answers, and a paragraph of reasoning built on the wrong
one. `docs/CUTOVER.md` had inherited the same sentence.

The unit of analysis is a **sentence**, not a line. A first attempt read
line by line and reported that `oq-live` does not depend on `oq-live` —
a self-claim, which is what a mis-parse looks like: the Chinese
translation had wrapped one claim across two lines, so the continuation
line's first crate name was taken as its subject. Any checker that
reports nonsense confidently is worse than none, so lines are joined
into paragraphs and split into sentences before anything is read.

What cannot be parsed is **reported as unchecked** rather than passed
over. A checker that silently skips what it does not understand reads as
thorough and is not.

What this does not check: judgement. "The wiring is missing" is not
derivable from the tree. Only "A depends on B" is.
"""

import os
import re
import subprocess
import sys

# A claim is a crate, a verb, and one or more crates — with the subject
# *adjacent* to the verb. An earlier version took the first crate in the
# sentence as subject and everything after as objects, which read a list
# of crate names ("Cargo pulls a tree only for `oq-l2feed`, `oq-ingest`,
# `oq-gateway` …") as three dependency claims and reported all three as
# failures. Adjacency is what separates a claim from an enumeration.
VERB = re.compile(
    r"(?P<neg>depends on neither|depend on neither|does not depend on|"
    r"do not depend on|cannot depend on|never depends on|"
    r"depends on|depend on|依赖)",
    re.IGNORECASE,
)
NEGATED = ("neither", "not depend", "cannot depend", "never depends")
# Conditional and hypothetical clauses state what *would* be true under a
# design that was not chosen. They are claims about nothing.
HYPOTHETICAL = (
    "would", "could", "should", "if ", "rather than", "instead of",
    "要么", "否则", "本可以", "将会", "会",
)
CRATE = re.compile(r"`(oq-[a-z0-9-]+)`")
# How far either side of the verb a crate name still counts as part of
# the claim. Wide enough for "`oq-live` now depends on", narrow enough
# that the next sentence's subject is not swept in.
BEFORE, AFTER = 48, 96
# A sentence ends at one of these. The Chinese full stop and the
# semicolon are included because the translations use them where the
# English uses a full stop.
SENTENCE = re.compile(r"(?<=[.。；;])\s+|\n")


def workspace_deps(root):
    """`{crate: {deps}}` over normal edges only.

    Dev and build dependencies are deliberately out. A document saying
    "A depends on B" means a consumer of A gets B, and a dev-dependency
    is not that — `oq-live` dev-depends on `oq-examples` and no reader
    should be told those ship together.
    """
    crates = sorted(
        d for d in os.listdir(os.path.join(root, "crates"))
        if os.path.isdir(os.path.join(root, "crates", d))
    )
    out = {}
    for c in crates:
        try:
            text = subprocess.run(
                ["cargo", "tree", "-p", c, "--edges", "normal", "--prefix", "none"],
                cwd=root, capture_output=True, text=True, check=True,
            ).stdout
        except (subprocess.CalledProcessError, FileNotFoundError):
            return None
        found = {w for line in text.splitlines()
                 for w in [line.split()[0] if line.split() else ""]
                 if w.startswith("oq-")}
        found.discard(c)
        out[c] = found
    return out


def sentences(path):
    """`(line_number, sentence)` for a markdown file.

    Paragraphs are joined first, so a claim wrapped across two lines is
    one sentence rather than two fragments with the wrong subject. Code
    fences are skipped: a dependency line inside one is a literal, not a
    claim.
    """
    para, start, fenced = [], 1, False
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for i, line in enumerate(lines, 1):
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        if not line.strip():
            if para:
                yield from _split(start, " ".join(para))
                para = []
            continue
        if not para:
            start = i
        para.append(line.strip())
    if para:
        yield from _split(start, " ".join(para))


def _split(start, text):
    for s in SENTENCE.split(text):
        if s.strip():
            yield start, s.strip()


def install_claims(root):
    """Docs must not offer an install of something not published.

    `VERSIONING.md` states the publication state of the Rust crates in a
    table. The quickstart offered five `cargo install oq-*` lines while
    that table said *nothing published*, and the two stood in the
    repository together for weeks: `cargo install` succeeds against a
    name placeholder, produces an empty crate, and reports no error, so
    a reader following the first document had no way to learn the second
    one existed.

    Offline on purpose. Asking crates.io would make CI depend on a
    network call to tell it something the repository already states, and
    a flaky check is a check somebody disables.
    """
    versioning = os.path.join(root, "docs/VERSIONING.md")
    if not os.path.exists(versioning):
        return []
    with open(versioning, encoding="utf-8") as f:
        text = f.read()
    published = "nothing published" not in text.lower()
    if published:
        return []

    docs = subprocess.run(
        ["git", "ls-files", "*.md"], cwd=root,
        capture_output=True, text=True, check=True,
    ).stdout.split()
    out = []
    for doc in docs:
        # The two pages whose subject *is* the contradiction may name it.
        if doc.startswith("scripts/") or "VERSIONING" in doc:
            continue
        with open(os.path.join(root, doc), encoding="utf-8") as f:
            for n, line in enumerate(f, 1):
                stripped = line.strip()
                if stripped.startswith("cargo install oq-"):
                    out.append(
                        f"{doc}:{n} offers `{stripped}` while VERSIONING.md says "
                        "nothing is published"
                    )
    return out


def main():
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()

    # The install check is text and needs nothing built, so it runs
    # first and runs regardless. Putting it after the cargo probe would
    # make a text check unavailable whenever an unrelated tool was
    # missing — which is the same shape as every defect this file was
    # written for: one half silent because the other half was absent.
    failures = install_claims(root)

    deps = workspace_deps(root)
    if deps is None:
        for f in failures:
            print(f"FAIL {f}")
        print("check-doc-claims: cargo not found, so only the text checks ran")
        return 1 if failures else 2

    docs = subprocess.run(
        ["git", "ls-files", "*.md"], cwd=root,
        capture_output=True, text=True, check=True,
    ).stdout.split()

    unchecked, checked = [], 0

    for doc in docs:
        # This file describes the check; its examples are not claims.
        if doc.startswith("scripts/"):
            continue
        for lineno, s in sentences(os.path.join(root, doc)):
            low = s.lower()
            m = VERB.search(s)
            if not m:
                continue
            if any(h in low or h in s for h in HYPOTHETICAL):
                # A conditional. Not skipped silently: a reader would
                # take it as a statement, so it is worth being able to
                # count how many of them there are.
                if CRATE.search(s):
                    unchecked.append((doc, lineno, s))
                continue
            before = s[max(0, m.start() - BEFORE):m.start()]
            after = s[m.end():m.end() + AFTER]
            subjects = CRATE.findall(before)
            objects = [n for n in CRATE.findall(after) if n in deps]
            if not subjects or not objects:
                if CRATE.search(s):
                    unchecked.append((doc, lineno, s))
                continue
            subject = subjects[-1]
            objects = [n for n in objects if n != subject]
            if subject not in deps or not objects:
                unchecked.append((doc, lineno, s))
                continue
            negated = any(n in m.group("neg").lower() for n in NEGATED) or any(
                z in s for z in ("既不依赖", "都不依赖", "不依赖")
            )
            for dep in objects:
                checked += 1
                actual = dep in deps[subject]
                if actual and negated:
                    failures.append(
                        f"{doc}:{lineno} says {subject} does not depend on {dep} — it does"
                    )
                elif not actual and not negated:
                    failures.append(
                        f"{doc}:{lineno} says {subject} depends on {dep} — it does not"
                    )

    for doc, lineno, s in unchecked:
        print(f"?    {doc}:{lineno} — {s[:78]}")
    for f in failures:
        print(f"FAIL {f}")

    print()
    print(f"doc claims: {checked} pair(s) checked, {len(unchecked)} sentence(s) not parsed")
    if unchecked:
        print("Sentences marked ? name one crate and a dependency word without a")
        print("second crate to compare it against — usually a hypothetical. They")
        print("are listed rather than ignored so the count is honest, and they do")
        print("not fail the check.")
    if failures:
        print()
        print("A dependency claim contradicts Cargo.toml. Fix the sentence, or move")
        print("the fact into the generated block (scripts/doc-facts.sh).")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
