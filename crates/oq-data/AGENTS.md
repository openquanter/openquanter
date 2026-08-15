# oq-data

The data plane: dual-timestamp tick streams, leakage-free as-of joins,
and bitemporal reference data.

## Commands

```bash
cargo test -p oq-data
cargo clippy -p oq-data --all-targets -- -D warnings
```

## Invariants

- **As-of joins are strictly before by default.** `AsOf::Strict` matches
  `t < query`. A record stamped at exactly the decision instant is not
  available to that decision. Several popular dataframe libraries
  default to `<=`; the difference is one record and it is the difference
  between research and fiction. `AsOf::Inclusive` exists only for
  after-the-fact reconstruction.
- **Joins key on arrival time, not event time.** A value stamped 08:00
  that arrived at 08:00.4 was not knowable at 08:00.2. `Timeline::Event`
  exists because some data never travelled; using it for venue data
  grants foresight.
- **"Nothing known yet" is `None`, never the earliest record.** Falling
  back would fabricate knowledge the process did not have — the same
  family of error as extrapolating a rule table backwards.
- **Bitemporal queries pin both axes.** `as_believed_at(valid, known)`
  is what a reproducible run uses. `current()` is convenient and *not*
  reproducible across corrections; it is named so that reaching for it
  in a reproducibility-critical path reads as a decision.
- **Both timestamps travel with every tick.** Dropping arrival to save
  eight bytes makes the file permanently unusable for latency-aware
  work, and that cannot be undone after capture. It is not an option.
- **Tick order is verified, not assumed.** Out-of-order records break
  gap-fill in a way that produces *fills* rather than an error, so a
  silently reordered stream would show up as profit.
- **Records are checksummed.** A corrupted price does not crash a
  backtest; it changes the answer, and no later stage makes that visible.

## Notes

- The default build carries no third-party dependencies. Columnar
  archive support goes behind an off-by-default feature: it pulls a
  large dependency tree, and per `docs/CAPTURE-FORMAT.md` §9 a columnar
  file is a post-sealing conversion target rather than anything the
  replay path needs.
- `TickStream::feed_latency_summary` reports whether a dataset carries
  latency information at all. Run it before calibrating any latency
  model: a file whose arrival and exchange timestamps are equal was
  captured without them or synthesized, and cannot support one.
