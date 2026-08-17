# openquanter

**Evaluate the backtest you already have.**

This package does not run backtests. It answers the question a backtest
cannot answer about itself: how much of the best result was bought by the
searching.

```python
import openquanter as oq

# One configuration's returns, at whatever frequency you sampled them.
oq.sharpe_ratio(returns)

# The same, minus what trying many configurations bought you.
oq.deflated_sharpe_ratio(sharpes, best_sharpe, n_observations, skew, kurtosis)

# How often the best in-sample configuration is not the best out of sample.
oq.probability_of_backtest_overfitting(columns, n_blocks=16)
```

Deliberately a small surface. The framework is Rust and stays usable
without Python; this exposes the part that is worth having even if you
never migrate — see [D16](https://github.com/openquanter/openquanter/blob/main/docs/IMPLEMENTATION.md)
for why that is the first stage rather than the whole of it.
