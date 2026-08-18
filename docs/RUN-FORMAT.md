# The run file

What a run produced, and the identity it produced it under, in one file.

```text
openquanter-run 1
body-sha256 7464039f4b07baadc895de05b289c48890bf29a81696dc9644d3fed06e6aa44d
code-commit abc123
data-sha256 f13fcc00812c2632bc6e265a6bc6861ac27cbc38433309a6a5fbb2bb45e48b1c
config-sha256 e67d23e7820c49a8051dac2831f38290f5e72f66c8db5079eeb60d82f14894c0
label L0
pnl 123.456
fills 5
# ts symbol side price qty tag
fill 1700000000000000000 BTCUSDT buy 6000000 5 -
fill 1700000060000000000 BTCUSDT sell 6001000 5 exit
...
```

## What it is for

[`WHY.md`](WHY.md)'s fifth wall is that nothing can show a past result
still holds. The instrument for that — the identity triple, the
`baseline invalidated — rebase required` verdict — was built and had
nowhere to write its answer, so a parity result lived as long as one
process and then became a memory.

Three things were blocked by the absence of a file, and it is worth
being precise about that because the roadmap had recorded the *wrapper*
as the missing piece:

- a baseline could not be kept, so there was no regression guard
- `oq parity` could not exist, because there was nothing for a command
  line to name
- an attribution report had nothing to bind to (`FR-ATTRIB-5`)

## Four decisions

**The manifest is inside the file.** This is D13's whole point: a
baseline separated from its identity is a number without an experiment,
and two files that must be kept together eventually are not. A reader
that finds fills without a manifest is looking at something that cannot
be compared, and the parser refuses it rather than comparing it.

**Text, line-oriented.** A baseline is written once and read years
later, possibly by somebody arguing a result was never reproducible, so
it has to be readable without this program. `grep`, `diff`, and
`pandas.read_csv` on the fill section all work; the columns are named in
the file rather than in a document somebody has to find. A binary format
would be smaller and would make the archive depend on a decoder that has
to still exist and still agree.

**The body is hashed.** Not tamper resistance — anybody who can edit the
file can edit the hash — but a baseline truncated by a full disk is the
realistic failure, and one that compares as "matching for the part that
survived" is worse than no baseline at all. The hash is in the header
rather than a trailer so a reader can check it while streaming.

**A version this build does not know is refused.** Not migrated, not
partially read. Reading what is recognised and ignoring the rest is how
a baseline written by a newer engine gets compared against an older one
and reports a regression that is a format difference.

## Distinctions the format keeps

| | |
|---|---|
| a run that made no trades | writes `fills 0` |
| a file that never finished writing | has no `fills` line, and is refused as truncated |
| a fill with no tag | writes `-` |
| a fill tagged with the empty string | writes it, and it is not `-` |

A count that disagrees with the rows is caught separately from a bad
hash, because they are different defects: the first is a writer bug, the
second is a transport one, and the file is internally consistent in the
first case.

## Reading it

```text
oq parity baseline.run candidate.run
```

Exit codes: `0` the runs agree, `1` they differ, `2` bad arguments, `3`
the baseline is invalidated or could not be read. **Three outcomes and
not two**, for the same reason `oq-recon` has three: "I could not check"
is not "I checked and it is fine". A CI job that treats an invalidated
baseline as a pass is a CI job whose regression guard silently stopped
guarding.

## What is deliberately absent

No compression: a run output is kilobytes to a few megabytes and
compresses well with any ordinary tool. No schema evolution machinery:
the version line is a refusal, not a migration path. No equity curve
yet — `FR-RESEARCH-4` asks for one and this format carries fills and
realized P&L only; adding it is a version bump, and the version line
exists so that bump is a refusal on old readers rather than a silent
misreading.
