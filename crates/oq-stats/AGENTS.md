# oq-stats

Overfitting statistics: deflated Sharpe ratio (`dsr`), probability of
backtest overfitting via CSCV (`pbo`), sample moments (`moments`),
standard normal CDF and quantile (`normal`), trial registry (`trials`).

## Commands

```bash
cargo test -p oq-stats
cargo clippy -p oq-stats --all-targets -- -D warnings
```

## Invariants

- **No dependencies.** This crate sits at the bottom of the workspace.
  The normal CDF and quantile are implemented here on purpose.
- **Sharpe ratios are at the frequency of the input.** Nothing in this
  crate annualizes. Annualizing before deflation inflates the result,
  so the conversion is a reporting decision made by the caller.
- **Kurtosis is non-excess**: 3.0 for a normal sample. The DSR formulas
  assume that convention; changing it silently shifts every probability.
- **Errors, not sentinel values.** Degenerate input returns
  `StatsError`; no NaN is ever returned as if it were a result.

## Testing notes

- `normal` is checked against reference values of the CDF and its
  inverse, including a far-tail value where relative accuracy matters.
  Absolute tolerances are meaningless at 1e-16 magnitudes.
- Statistical tests use a **SplitMix64** generator, never a bare LCG:
  an LCG consumed at a fixed stride leaks lattice structure into the
  columns of a performance matrix, producing synthetic "noise" with
  persistent per-column bias. A test written on such data measures the
  generator, not the estimator.
- The pure-noise PBO test averages over seeds. For a single sample the
  estimate ranges roughly 0.25–0.8 even when the null holds exactly;
  asserting on one seed asserts on that seed's luck.
