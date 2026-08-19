# Tick File Format

> Status: **Proposed v3** · [中文版](TICK-FORMAT.zh-CN.md)
> Implemented by: `oq-data` · Related: [Capture Format](CAPTURE-FORMAT.md) · [Implementation Plan](IMPLEMENTATION.md)

The format a backtest reads. Distinct from the capture archive, which
holds verbatim venue bytes, and from columnar exports, which are for
analysis. This one exists for a replay loop that wants the next
observation with as few instructions as possible.

## 1. Where it sits, and why that caps the risk of changing it

Three formats, three jobs:

| Format | Job | Can it be regenerated? |
|---|---|---|
| `.oqcap` | Verbatim capture. The venue's bytes, unmodified | **No. Irreplaceable** |
| `.oqtk` | Normalized engine input | **Yes, by re-importing from `.oqcap`** |
| Parquet | Analysis and interchange | Yes, exported on demand |

This ordering is the most important property of the design. **The tick
file is derived, not archival.** A breaking change to it costs an import
pass, not data. The format that must never change incompatibly is
`.oqcap`, and it is specified separately and deliberately conservative.

Keep it that way. The moment someone treats a `.oqtk` file as the only
copy of something, the risk profile of this document changes completely.

## 2. Design constraints

1. **Sequential scan is the whole workload.** A backtest reads millions
   of records and does little per record, so cost is dominated by copies
   and per-record decode work. Fixed-width, little-endian, no schema
   branching.
2. **Both timestamps always travel.** Dropping arrival time to save
   eight bytes makes the file useless for latency-aware simulation
   later, and that cannot be undone after the fact. It is not an option,
   not a flag.
3. **Fields are added, never repurposed** (D5). The format must make
   that cheap, because v2 already proved fields get added — `volume` was
   one.
4. **Truncation is detectable before allocation.** Count and checksum
   live in the header, not a trailer.

## 3. What v2 got right, and the one thing it did not

v2 is a 32-byte header and 64-byte records of eight `i64`s. Fixed width,
little-endian, cache-line sized, dual timestamps mandatory. All correct.

The problem: **eight fields exactly fill 64 bytes, so a ninth field
requires a new version and a rewrite of every file.** With one field
already added since v1, more will follow — open interest, a sequence
number, a funding rate. Each one costing a format break and a migration
is a tax on doing the right thing.

v3 exists to remove that tax. It should be the last structural change.

## 4. v3 layout

### Header — 64 bytes, one cache line

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | `OQTK` |
| 4 | 2 | version | 3 |
| 6 | 2 | `header_len` | 64 |
| 8 | 2 | `record_len` | Bytes per record. **The extensibility mechanism** |
| 10 | 2 | `field_count` | `record_len / 8`, stored for validation |
| 12 | 4 | flags | Reserved, must be zero |
| 16 | 8 | `record_count` | |
| 24 | 8 | `instrument` | Opaque to this crate |
| 32 | 8 | `first_exch_ts` | Lets a catalogue answer "which file covers this day" without scanning |
| 40 | 8 | `last_exch_ts` | |
| 48 | 4 | `crc32` | Over the record region |
| 52 | 12 | reserved | Must be zero |

### Records — `record_len` bytes, all fields `i64` little-endian

Field *k* lives at offset `8k`, **forever**. The table is global and
append-only.

| # | Offset | Field | Since |
|---|---|---|---|
| 0 | 0 | exchange timestamp, ns | v1 |
| 1 | 8 | arrival timestamp, ns | v1 |
| 2 | 16 | last price, ticks | v1 |
| 3 | 24 | high, ticks | v1 |
| 4 | 32 | low, ticks | v1 |
| 5 | 40 | bid, ticks | v1 |
| 6 | 48 | ask, ticks | v1 |
| 7 | 56 | cumulative volume, lots | v2 |

### Zero is not a price

`last` is never zero in a written record. The other price fields use zero
for "unknown"; this one cannot, because the kernel takes it as the mark
price without a guard, and a position marked at nothing liquidates at a
leverage of 1x or more and understates minimum equity by the position's
notional below that — silently, in both directions.

A producer with no price does not write a record. `oq-ingest` carries the
previous price across windows with no trade of their own, and writes
nothing before the first trade. A reader may rely on this; a writer owes
it.

This became true when it was measured. A twelve-hour live capture carried
`last = 0` on 56.4% of its ticks, and 8.0% of them had all three prices.

### The extensibility rule

**A new field is appended and `record_len` grows. Nothing else changes,
and the version does not move.**

A reader determines presence arithmetically: field *k* is present when
`8 * (k + 1) <= record_len`. Older files are read by newer code with the
trailing fields absent; newer files are read by older code by ignoring
the bytes past the record length it knows.

This is why `record_len` is in the header rather than implied by the
version. It is the difference between "adding a field is a migration"
and "adding a field is a write".

Two rules keep the property true, and both are absolute:

- **Never reorder, never resize, never repurpose an existing field.** A
  field whose meaning changed while its offset stayed is the failure
  mode behind some of the most expensive incidents in electronic
  trading, and it is unrecoverable after the fact because old files
  cannot be distinguished from new ones.
- **A version bump means an incompatible change**, which should now
  never be necessary. If one is proposed, the burden is to show why
  appending a field cannot do the job.

### Reading

Two paths, both correct:

- **Zero-copy fast path.** When `record_len` equals the reader's own
  record size and the mapping is 8-byte aligned, the record region casts
  directly to a slice with no per-record work.
- **Field-wise path.** Otherwise, read the known fields at their fixed
  offsets and skip the remainder. Costs a few loads per record and keeps
  every file readable by every build.

Records are 8-byte aligned. 64 bytes is the sweet spot because it is a
cache line, but padding to preserve that as fields are added is not
worth the space — a sequential scan with prefetch barely notices a
straddled line, and this format is scanned, never randomly accessed.

## 5. Integrity and identity

Two different jobs, deliberately not conflated:

| Question | Mechanism |
|---|---|
| Are these bytes damaged? | `crc32` in the header, over the record region |
| Are these the same bytes as before? | SHA-256, in a sidecar manifest |

CRC-32 is fast and catches corruption. It is not a content identity: a
parity baseline pinned to a CRC is pinned to something an adversary or
an unlucky bit-pattern can collide. Identity uses SHA-256 and lives
beside the file, matching the capture archive's manifest convention:

```
ticks/<venue>/<symbol>/<YYYY-MM-DD>.oqtk
ticks/<venue>/<symbol>/<YYYY-MM-DD>.manifest.json
```

The manifest records the source `.oqcap` files, the importer version and
commit, the record count and time range, and `sha256_raw`. That last
field is what a parity baseline pins (D13), so re-importing the same
capture with the same importer produces the same identity, and importing
with a *different* importer is visible as a changed baseline rather than
as a mysterious behavioural difference.

## 6. What this format is not

- **Not an archive.** It is derived. Delete it and re-import.
- **Not an interchange format.** Export Parquet for that; a columnar
  file is the right shape for analysis and the wrong shape for a replay
  loop, and trying to serve both makes a format that is bad at each.
- **Not self-describing about instruments.** `instrument` is an opaque
  identifier. The mapping to a symbol belongs in the catalogue, not in
  every record of every file.
- **Not compressed.** The engine reads it hot; compression trades scan
  speed for disk, and disk is the cheaper of the two. Compress the
  capture archive instead — that is where the volume is.

## 7. The columnar export

`oq-data` writes the same ticks as Parquet, behind the optional
`parquet` feature. This is the "export Parquet for that" of §6, made
real.

```text
cargo run -p oq-data --features parquet --bin oq-data -- ticks.oqtk --parquet out.parquet
```

Eight `Int64` columns — `exch_ts`, `local_ts`, `last`, `high`, `low`,
`bid`, `ask`, `volume` — zstd-compressed, with the instrument id and a
schema version in the file's key-value metadata under
`openquanter.instrument` and `openquanter.tick_schema`.

Three decisions worth stating, because each has an obvious wrong answer:

- **Both timestamps, neither optional.** Their difference is feed
  latency. An export that keeps one leaves a reader unable to tell a slow
  feed from a slow market, and the loss is silent.
- **Integers, not scaled floats.** A float column reads more nicely and
  is wrong; the scale belongs to the instrument, not to the price. Files
  round-trip exactly, and a test holds every column to `Int64`.
- **The feature is optional because the tree is ~90 crates**, more than
  the rest of the workspace combined. `oq-data`'s default build still
  carries zero third-party dependencies, and CI checks that separately
  from the feature build.

On 7.3 hours of captured BTC perpetual data — 262,365 ticks — the export
is 4.06 MB against 16.79 MB native, or 24%, and reads in pandas without a
custom reader:

```python
df = pd.read_parquet("out.parquet")
latency_ms = (df.local_ts - df.exch_ts) / 1e6   # median 89.8, p99 102.1
```

The checksum is not carried across. Parquet has page-level integrity of
its own, and asserting ours over bytes we no longer control would be a
claim we cannot keep; `read_parquet` rebuilds a `TickStream`, which
recomputes it.
