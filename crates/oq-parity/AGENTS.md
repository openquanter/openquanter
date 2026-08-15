# oq-parity

Fill-by-fill comparison of two runs, with difference attribution and
baseline identity management. Built before the engine it measures.

## Commands

```bash
cargo test -p oq-parity
cargo clippy -p oq-parity --all-targets -- -D warnings
```

## Invariants

- **A stale baseline is stale, not violated.** If the input data hash or
  configuration hash differs, `compare` returns `Invalidated` and reports
  *no* differences. Never soften this into a warning attached to a
  difference list: the whole point is that a reader cannot mistake
  "the inputs moved" for "the engine regressed". See D13 in
  `docs/IMPLEMENTATION.md`.
- **A differing code commit is comparable.** That is the case parity
  exists to measure — a port must be allowed to differ in code and
  required not to differ in behavior.
- **Prices and quantities compare exactly.** They are fixed-point
  integers. Tolerance applies only to derived monetary values, and it is
  relative, never absolute.
- **Report the first divergence, not a count.** One early divergence
  cascades into thousands of downstream differences; the tail carries no
  information. `first_divergence` and `matched_prefix` lead the report.
- **Field differences carry a signed delta** where the field is numeric.
  A one-tick price difference and a thousand-tick one are different
  findings.

## Notes

- SHA-256 is implemented in this crate rather than taken as a dependency:
  a verification tool that cannot be built from the workspace alone is a
  weak link. It is checked against the standard test vectors, and the
  streaming path is checked against the one-shot path at many chunk
  sizes — the buffering seam is where such implementations break.
- The aligner resynchronizes within a 32-fill window. Beyond that the
  runs are treated as structurally different rather than shifted.
