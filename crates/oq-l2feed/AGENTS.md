# oq-l2feed

Market data capture, and the tools that prove a capture is usable:
verbatim record framing, UTC-day rotation, sealing and manifests, plus
depth parsing and order book reconstruction. Implements
`docs/CAPTURE-FORMAT.md` — read that first; this file only covers what
is easy to get wrong in code.

Two halves, and the split matters:

- **Writing** (`frame`, `writer`, `day`, `manifest`, `disk`, `session`,
  `ws`, `venue`, `stream`) — gets bytes to disk and never transforms
  them.
- **Reading back** (`depth`, `book`, `bin/oq-book-check`) — replays
  those bytes into an order book to establish that they reconstruct.
  This half has no live counterpart and never runs during capture.

## Commands

```bash
cargo test -p oq-l2feed
cargo clippy -p oq-l2feed --all-targets -- -D warnings

# Capture a short sample, then check it rebuilds a book.
cargo run --bin oq-capture -- --root ./archive --symbol BTCUSDT \
  --stream depth --minutes 10 --floor-gb 10
cargo run --bin oq-book-check -- --file ./archive/<...>.oqcap
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
- **Reconstruction refuses rather than guesses.** `book::Book` will not
  apply an update it cannot place in sequence, and it says which rule
  broke. A book that quietly absorbs an out-of-order message produces
  plausible prices that are wrong, and nothing downstream can tell.
  After a gap the book is *reset*, never merged into: state across a
  gap is unknown, and merging into unknown state looks right and is not.
- **A declared gap is not a sequence error.** `oq-book-check`
  distinguishes a break the capture marked from one nobody recorded.
  Only the second fails the run — conflating them makes the tool cry
  wolf on every reconnect, and a check that always fails gets ignored.
- **Decimals never go through `f64`.** `depth::parse_fixed` reads digits
  directly. A price that does not fit the configured scale is refused,
  not rounded: rounding changes which side of a limit a price falls on
  and the caller cannot tell it happened.

## Notes

- Third-party dependencies live here and nowhere else in the workspace:
  `tungstenite` and `ureq` (with their TLS stack), because this is the
  crate that has to speak to a venue. The budget is declared in
  `scripts/check-composability.sh` and enforced in CI; the engine crates
  stay at zero and must not inherit this tree.
- Everything except `ws.rs` and the two binaries is reachable without
  the network: framing, sealing, depth parsing and reconstruction are
  pure functions over bytes, and their tests need no venue.
- Manifest JSON is hand-written for the same reason. The schema is fixed
  by the format document; if you add a field there, add it here.
- Torn-tail semantics live in `frame::decode_all`: a truncated *final*
  record means the writer died mid-append and reading stops cleanly; a
  checksum failure anywhere earlier is corruption and errors.

## Building for deployment

Set `OQ_BUILD_COMMIT` when building anything that will write an
archive:

```sh
OQ_BUILD_COMMIT=$(git rev-parse --short HEAD) \
  cargo build --release -p oq-l2feed
```

It lands in every manifest as `capture_commit`, and it is the first
thing anyone asks when a file looks wrong six months later. Without it
the field reads `unknown`, which is worse than it sounds: the archive
then cannot say which build produced it, and a capture bug becomes
impossible to scope to the window it affected.
