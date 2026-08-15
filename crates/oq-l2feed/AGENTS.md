# oq-l2feed

Market data capture: verbatim record framing, UTC-day rotation, sealing
and manifests. Implements `docs/CAPTURE-FORMAT.md` — read that first;
this file only covers what is easy to get wrong in code.

## Commands

```bash
cargo test -p oq-l2feed
cargo clippy -p oq-l2feed --all-targets -- -D warnings
```

## Invariants

- **Verbatim payloads.** The venue's bytes go to disk unchanged. No
  merging, no downsampling, no re-serialization. Any transformation at
  capture time is one that can never be undone, and the tests assert
  that newlines, invalid UTF-8 and NUL bytes survive a round trip.
- **Rotation follows the exchange clock**, never a local timer. A file
  holds exactly its own UTC day even if the host's clock drifts or the
  process restarts across midnight. A record belonging to an already
  closed day is refused, not written into the wrong file — losing data
  and mislabelling data are both worse than an error the caller sees.
- **Gap markers are mandatory.** A reader must be able to tell "nothing
  happened in the market" from "we were not listening". Gaps go into the
  stream *and* into the manifest count.
- **Sealing hashes what is on disk**, not what was intended: the
  manifest describes the artifact. `sha256_raw` is the content identity
  a parity baseline pins, which is why compression is a separate step —
  recompressing an archive must not invalidate every baseline that
  depends on it.
- **Restart appends, never truncates**, and writes a `session_start`
  control record so the seam is visible in the data.
- **The writer never compresses and never deletes.** Capture is the part
  that cannot be redone, so it does the least work it can. Compression,
  transfer, remote verification and retention live outside this crate.

## Notes

- Zero dependencies beyond `oq-hash`. A capture host should be able to
  build this with nothing else present.
- Manifest JSON is hand-written for the same reason. The schema is fixed
  by the format document; if you add a field there, add it here.
- Torn-tail semantics live in `frame::decode_all`: a truncated *final*
  record means the writer died mid-append and reading stops cleanly; a
  checksum failure anywhere earlier is corruption and errors.
