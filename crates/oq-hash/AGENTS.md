# oq-hash

SHA-256 and CRC-32. Two consumers with different needs, one crate so
there is exactly one implementation of each in the workspace.

## Commands

```bash
cargo test -p oq-hash
cargo clippy -p oq-hash --all-targets -- -D warnings
```

## Invariants

- **Load-bearing beyond its size.** `crc32` checksums every journal
  record and every capture record; `sha256` establishes the content
  identity that parity baselines are pinned to (D13). Treat any change
  to either as a breaking change to the whole workspace.
- **Never alter a polynomial, a table, or an initial value.** A changed
  checksum does not make old data invalid — it makes old data
  *unreadable*, and every existing journal along with it. There is no
  migration path, because the old files cannot be distinguished from
  corrupt ones.
- **Zero dependencies, and it stays that way.** This crate sits under
  the verification chain. A verification tool that cannot be built from
  the workspace alone is a weak link, and a supply-chain problem here
  would reach every stored byte in the project.
- **Both implementations are checked against their published test
  vectors**, and CRC-32 additionally against every single-bit flip in a
  sample payload. Those tests are the reason anyone can trust the two
  functions; do not weaken them.

## Adding to this crate

A streaming `Crc32` may be needed eventually for large capture files.
Add it **beside** the one-shot function rather than replacing it — the
journal calls the one-shot form on a contiguous region and must keep
producing identical values.

Anything that is not a hash does not belong here. The crate's value is
that its surface is small enough to audit in one sitting.
