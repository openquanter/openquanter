# Market Data Capture Format

> Status: **Draft for review** · [中文版](CAPTURE-FORMAT.zh-CN.md)
> Implemented by: `oq-l2feed` · Related: [Implementation Plan](IMPLEMENTATION.md) §5 Phase 0

A day of market data that was not captured is gone permanently, and a day
that was captured in the wrong format is nearly as bad. This document
specifies how captured data is framed, sealed, verified, and archived.

## 1. Goals

1. **Verbatim.** The bytes the venue sent are the bytes on disk. No
   merging, no downsampling, no re-serialization. Aggregation is the
   consumer's job, and any transformation applied at capture time is one
   that can never be undone.
2. **Replay-grade.** Every record carries both the exchange timestamp and
   the local receive timestamp at nanosecond resolution. This is the
   difference between data you can research with and data you can
   simulate latency with.
3. **Sealed by day.** A completed day is immutable, hashed, and
   self-describing, so it can be compressed, transferred, verified, and
   then trusted for years.
4. **Crash-safe.** A capture process that dies mid-write loses at most
   the final record, and the loss is detectable rather than silent.
5. **Verifiable.** Every archived day carries a manifest with a content
   hash. That hash is also what a parity baseline pins as its data
   identity (see D13 in the implementation plan).

## 2. Layout

```
raw/<venue>/<symbol>/<stream>/<YYYY-MM-DD>.oqcap          daily rotation
raw/<venue>/<symbol>/<stream>/<YYYY-MM-DD>/<HH>.oqcap     hourly rotation
                                          <HH>.manifest.json
```

One file per venue, symbol, stream and **rotation period**. The period
is a day by default and can be an hour; either way a day remains a
browsable unit, as a file or as a directory. The split is deliberate on
every axis:

- **By period** because it gives archival a natural unit: seal, hash,
  compress, transfer, verify, then let retention delete the local copy.
  A day is the natural choice. An hour is for a capture host whose disk
  cannot hold two periods of raw data — **the open file cannot be
  compressed while it is being appended to, so the local peak is always
  about two rotation periods**, and shortening the period is the only
  lever that does not involve buying disk. Measured on four perpetual
  symbols, a day of raw capture is 8 GiB at weekend rates and several
  times that on an active weekday; two of those do not fit on a small
  host, and two hours comfortably do.
- **By stream** because streams have wildly different volumes and
  retention value. Incremental depth is gigabytes a day; the
  maintenance-margin rule table is bytes. Mixing them forces the cheap
  data to inherit the expensive data's handling.
- **By UTC day** because exchange timestamps are UTC, and a "day" that
  depends on the capture host's timezone is a trap for whoever reads the
  archive later.

Rotation happens at the first record whose **exchange** timestamp falls
in a new UTC day, not on a local-clock timer. A file therefore contains
exactly the records belonging to its day even if the capture host's clock
drifts or the process restarts across midnight.

## 3. Record framing

Length-prefixed binary frames, appended sequentially. Little-endian.

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 4 | `len` | Byte length of everything after this field |
| 4 | 1 | `kind` | `0` = venue payload, `1` = control record |
| 5 | 8 | `local_ts` | Local receive time, nanoseconds since the Unix epoch |
| 13 | 8 | `exch_ts` | Exchange timestamp, nanoseconds; `i64::MIN` when the payload carries none |
| 21 | 4 | `crc32` | CRC-32 of the payload bytes |
| 25 | `len - 21` | `payload` | The venue's bytes, exactly as received |

Framing rather than newline-delimited JSON: a length prefix holds any
byte sequence without escaping, so the verbatim rule survives payloads
that contain newlines, invalid UTF-8, or binary protocols. The cost is
that the file is not directly greppable, which is why `oq-l2feed cat`
converts a range of records to NDJSON on demand.

The CRC is per record, not per file: it lets a reader distinguish a torn
final record from corruption in the middle, which are different problems
with different responses.

## 4. Control records

`kind = 1` payloads are UTF-8 JSON emitted by the capture process itself,
interleaved in the stream so their position in time is unambiguous.

| `type` | Emitted when | Contents |
|---|---|---|
| `session_start` | Capture starts or resumes | Capture software version and commit, venue, symbol, stream, subscription parameters |
| `clock_offset` | At session start and hourly | Estimated offset and dispersion against the time source. Latency modeling built on an unverified local clock is built on sand, so the estimate is archived with the data rather than assumed |
| `gap` | Connection lost | Reason, last sequence number seen, wall-clock duration of the outage |
| `snapshot` | After any reconnect | The REST order book snapshot that re-establishes state, with the sequence number it corresponds to |
| `session_end` | Clean shutdown | Record and byte counts for the session |

A `gap` record is never omitted for being inconvenient. A reader must be
able to tell "nothing happened in the market" from "we were not
listening", and only an explicit marker makes that distinction possible.

### A marker's position is part of what it says

The marker does not say "this file has a gap somewhere". It says the
capture stopped listening *at this point in the stream*, and readers use
it that way: `oq-book-check` drops the book where it finds one and treats
the next update as a bootstrap rather than as a sequence error. Presence
alone is not enough. Any tool that rewrites a file must carry every
control record with the data it sat between, or it moves the boundary
between "declared" and "silently lost" without touching either.

This is not hypothetical. `oq-resequence` first wrote the control records
as a block at the front of the output. Four files damaged by a
duplicate-writer incident then reported one undeclared break each, when
in fact the repair had been complete:

| file | before repair | markers written first | markers kept in place |
|---|---|---|---|
| BTCUSDT | 54 | 1 | 0 |
| ETHUSDT | 56 | 1 | 0 |
| BNBUSDT | 52 | 1 | 0 |
| HYPEUSDT | 50 | 1 | 0 |

Undeclared sequence breaks, measured with `oq-book-check`. Every file
carried three gap markers.

The residual break in each was the relocated marker, not a loss. Nothing
from that hour was missing. The earlier record of this incident concluded
that one break per instrument was churn no reordering could recover; that
conclusion was wrong, and it was wrong in the direction that costs most —
a tool reporting damage it had itself introduced, in a file it had just
finished repairing correctly.

## 5. Sealing a day

When rotation occurs, the completed day is sealed:

1. Flush and close the active `.oqcap` file.
2. Scan it once to compute the manifest.
3. Compress to `.oqcap.zst` (zstd level 19 with long-distance matching;
   market data is highly repetitive and typically compresses around 10:1).
4. Write `<day>.manifest.json`.
5. Only then may the uncompressed file be removed.

```json
{
  "format_version": 1,
  "venue": "example",
  "symbol": "EXAMPLEUSDT",
  "stream": "depth",
  "utc_day": "2026-08-15",
  "records": 48213904,
  "bytes_raw": 9187442310,
  "bytes_compressed": 921883104,
  "first_exch_ts": 1786780800000000000,
  "last_exch_ts": 1786867199998000000,
  "first_local_ts": 1786780800000141000,
  "last_local_ts": 1786867199998233000,
  "gaps": 2,
  "gap_seconds_total": 11.4,
  "clock_offset_ns": {"at_start": -412000, "at_end": 318000, "max_abs": 1204000},
  "capture_version": "oq-l2feed 0.1.0",
  "capture_commit": "0000000000000000000000000000000000000000",
  "sha256_compressed": "0000000000000000000000000000000000000000000000000000000000000000",
  "sha256_raw": "0000000000000000000000000000000000000000000000000000000000000000"
}
```

Both hashes are recorded: the compressed one verifies the transfer, the
raw one identifies the *content* independently of how it was compressed.
Only the raw hash belongs in a parity baseline — recompressing an archive
must not invalidate every baseline that depends on it.

## 6. Archival

The capture host is a buffer, not an archive. The pipeline is:

```
seal → hash → transfer → verify remote hash → mark archived → retention deletes local
```

Rules that keep this from quietly losing data:

- **Never delete a local file that has not been verified at the
  destination.** Verification means recomputing the hash on the far side,
  not trusting the transfer tool's exit code.
- **Transfer sealed days only.** The active day is still being appended
  to; copying it produces a file that is neither current nor complete.
- **Batch, don't stream.** Compressed daily archives move as whole files
  on a schedule. A continuously streaming link turns every network
  problem into a capture problem, and capture is the part that cannot be
  redone.
- **Retention is a function of verified archival**, never of age alone.

The destination is deliberately unspecified here: any host, NAS, or
object store that can hold the data and recompute a hash will do. Where
a given deployment archives to is deployment configuration, not part of
the format.

### Staging adds a deadline the direct pipeline does not have

A capture host behind a lossy link often cannot reach the archive at
all, and the archive is usually not reachable from the internet either.
Putting an object store between them lets each side speak only to a
third party: the capture host uploads and forgets, the archive pulls and
keeps. `scripts/archive-capture.py` is the first half and
`scripts/pull-capture.py` the second.

The cost is that a staging bucket has an expiry, and the moment one
exists, **the pipeline has a deadline instead of a backlog**. Direct
transfer degrades safely — an archive host that is down for a month
finds the data waiting, because retention is a function of verified
archival. Through staging, an archive host that is down past the expiry
finds nothing, and capture cannot be redone.

So the pulling side is where the failure mode moved, and it needs two
things the pushing side does not:

- **A schedule with margin.** The interval must be short enough that
  several consecutive failures still fit inside the expiry. The run is
  idempotent — an object already local at the same size is skipped — so
  a frequent schedule costs nothing when there is nothing to do.
- **A status that outlives the run.** Silence is the failure mode here,
  and a log nobody reads is silence. `scripts/pull-capture-cron.sh`
  leaves `.last-success` / `.last-failure` in the archive root so
  "when did this last work?" is answerable without reading a log.

That wrapper branches on the pull's exit status, which is part of its
interface: `0` every object is local, `1` some object could not be
fetched, `3` another run holds the lock and this one did nothing. The
third is not a failure — a first backfill outlasts the interval that
starts the next run, so overlap is the normal state of an archive that
is catching up, and reporting it as loss trains whoever is watching to
ignore the alert that matters.

## 7. Crash safety

The active file is append-only. A process that dies mid-write leaves a
truncated final frame, detected by a short read or a CRC mismatch on the
last record. Readers must:

1. Treat a torn final record as end-of-file rather than corruption.
2. Treat a CRC failure anywhere earlier as corruption and refuse to
   proceed silently.

On restart, the capture process appends to the existing day file and
emits a `session_start` control record, so the seam is visible in the
data rather than inferred from file modification times.

## 8. Volume planning

Order of magnitude for one liquid perpetual instrument's full incremental
depth stream: **5–15 GB per day raw, 0.5–1.5 GB per day compressed**,
i.e. roughly 200–550 GB per year per symbol after compression. Best bid
and offer, trades, and mark price streams together are a small fraction
of that. Rule tables and periodic REST polls are negligible.

Two consequences worth internalizing before starting: storage grows
monotonically and forever, and the local buffer on the capture host must
survive the longest plausible archival outage — a full disk stops
capture, and a stopped capture is a permanent hole.

## 9. Why not other formats

| Alternative | Why not at capture time |
|---|---|
| NDJSON | Requires escaping to stay verbatim, and re-serializing JSON changes bytes. Available as an *export*, not as the archive |
| Parquet / columnar | Excellent for analysis, wrong for capture: it buffers, imposes a schema on data whose schema the venue controls, and cannot represent a message it fails to parse. Convert after sealing |
| Database ingestion | Couples capture uptime to database uptime, and makes the archive a backup problem rather than a file |
| Compressed stream written live | Interleaves compression latency with receive latency and makes a torn tail unrecoverable rather than truncating cleanly |

## 10. Venue behaviour worth knowing

Three findings from the first live capture. They are recorded here
because each one is invisible until data is already being collected, and
each one produces an archive that looks healthy while being wrong.

### A subscription that succeeds proves nothing

The venue's `SUBSCRIBE` confirms any stream name without validating it.
A name invented for a test — `btcusdt@thisDoesNotExist` — returns
success and then silence, exactly like a real stream in a quiet market.

Half the documented streams were found delivering nothing at all while
their raw counterparts worked: aggregated trades, klines, tickers, mark
price and the array fan-outs were silent; incremental depth, best bid
and offer, and raw trades were fine.

**Consequence for capture:** never infer health from a successful
connection. A stream that has produced no records for longer than its
expected quiet period is a fault, and the capture process should be able
to say so. Where a stream has no working form, poll the REST endpoint
that carries the same data and record the polls through the same path —
a failed poll is a disconnect, and belongs in the archive as a gap.

### The publish cadence can be fixed, so volume scales with message size

Incremental depth on this venue arrives on a fixed cadence — measured at
26 or 28 ms across 4411 consecutive messages, with a single 34 ms
outlier and no other value. The book does not push on every change; it
publishes a batch on a clock.

**Consequence for planning:** an active market does not send *more*
messages, it sends *larger* ones. Capacity estimates must scale the
bytes per message, not the message rate, and a rate measured in a quiet
period is a floor rather than an average.

### Buffered data is not captured data

A writer that flushes on a record count alone will hold a low-rate
stream in memory for a long time — at one message a second and a
thousand-record threshold, over sixteen minutes. A crash there loses
messages that were *received*, so no gap marker records their absence
and the archive is silently short.

**Consequence:** flush on a timer as well as a count. The capture
process should also be observable from outside — a file that never grows
must mean a fault, not a buffer.

### Not every record on the trade stream is a trade

Binance publishes `{"e":"trade",…,"p":"0","q":"0","X":"NA",…}` among the
real ones — 19,725 in one day of BTCUSDT against 5.4 million trades.
They carry trade ids and belong to the id chain, so a completeness check
that follows those ids is right to count them; a price of zero is still
not a price.

The damage is not where it looks. Storing them costs nothing and they
are part of the record. What breaks is the conversion: a window's low is
the minimum of the prices in it, and one zero makes that zero. Real
capture, before this was found, produced 1355 of 1409 minutes with a low
of `0.00` and the high right beside it. A resting buy is triggered by
the low, so a backtest reading it fills orders no venue would have
filled — and the same parse runs in the live loop.

**Consequence:** an adapter's parse must return "no trade" for a record
declaring none, and callers must count those separately from records
they could not read. The two look identical downstream and mean opposite
things: one is the venue reporting nothing happened, the other is this
build disagreeing with the venue about the format.
