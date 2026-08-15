# oq-journal

The append-only sequenced journal: audit trail, replay source, recovery
mechanism, and IPC transport in one artifact.

## Commands

```bash
cargo test -p oq-journal
cargo clippy -p oq-journal --all-targets -- -D warnings
```

## Invariants

- **A torn tail is not corruption.** A truncated *final* record means a
  writer died mid-append: replay stops cleanly and reports the resume
  offset. A bad checksum or magic *before* the end is data loss and is
  an error. Never conflate them — one is a normal crash, the other is
  losing records, and they have different recoveries.
- **Sequence numbers are dense.** A gap means records that existed are
  missing; reading further cannot recover from it.
- **The checksum covers the length field.** A corrupted length decides
  how far the reader jumps, so it must fail verification rather than be
  acted on. `MAX_PAYLOAD` bounds what a corrupted length can ask for
  before that check runs.
- **A writer resumes at a clean record boundary.** Reopening truncates
  a torn tail; appending after a tear would hide everything past it
  from readers.
- **Snapshots are taken at sequence boundaries, never wall-clock
  moments.** "State at 03:00" is not well defined in an event-sourced
  system; "state after event N" is exact and verifiable by replay.
- **A damaged snapshot falls back to an older one.** A longer replay is
  strictly better than refusing to start.

## Notes

- Checksums come from `oq-hash`. Do not add a second implementation to
  this crate; a change to that polynomial or table would make every
  existing journal unreadable rather than merely stale.
- The backend is buffered file I/O, not mmap, behind an API that does
  not expose the difference. None of the properties above require mmap.
  Replacing the backend is contained; getting the framing and the
  torn-tail semantics wrong would not have been.
- `SyncPolicy` is a required constructor argument. A durability
  guarantee acquired by accident is one lost by accident.
