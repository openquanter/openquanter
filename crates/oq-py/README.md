# openquanter

Python bindings for [OpenQuanter](https://github.com/openquanter/openquanter),
a Rust quantitative trading framework whose defining choice is that a
backtest models the thing that ends accounts: **the venue closing your
position.**

Two things are exposed. They are useful independently, and the first one
does not ask you to migrate anything.

## Evaluate the backtest you already have

The question a backtest cannot answer about itself — how much of the best
result was bought by the searching:

```python
import openquanter as oq

oq.sharpe_ratio(returns)
oq.deflated_sharpe_ratio(sharpes, best_sharpe, n_observations, skew, kurtosis)
oq.probability_of_backtest_overfitting(columns, n_blocks=16)
```

Every refusal says why — too few observations, zero variance, a matrix
that will not split — because the reason is the half you act on.

## Run a strategy on the Rust engine

A strategy with no framework in it, driven by the engine:

```python
class Cross:
    name = "cross"

    def on_tick(self, ctx):
        if self.crossed_up(ctx.last) and ctx.position == 0:
            return [oq.Order("buy", 1)]
        return None

result = oq.run_backtest(Cross(), ticks, balance=100_000)
print(result)   # RunResult(strategy='cross', ticks=50000, fills=178, …)
```

If the venue would have closed the account, the result says so — in its
`repr`, not only in a field, because the repr is what gets read.

### Throughput mode, and what it costs

`batch=n` calls the strategy once per batch and mirrors the account onto
the strategy object, which runs up to about **7x** faster. It is not
free, and this package measures the cost rather than asserting it is
small: a decision made after seeing a tick cannot be placed before that
tick was seen, so batched decisions are late.

```python
oq.compare_modes(Cross, ticks, balance=100_000, batch=64)
```

On the example crossover: `batch=8` buys 2.8x for 1.3% of the strategy's
edge, `batch=64` buys 5.8x for 18%, `batch=512` buys 6.9x and takes the
edge away. Which of those is acceptable is a property of your strategy,
so the binding measures and does not choose. `batch=1` is exactly
compatibility mode, and a test asserts the two runs are identical.

## Status

**Alpha.** The APIs are documented and not yet stable. The Rust core is
pre-alpha and specific about it — see the
[status section](https://github.com/openquanter/openquanter#status) for
what is built and what is designed, and
[docs/WHY.md](https://github.com/openquanter/openquanter/blob/main/docs/WHY.md)
for what the project is for.

The framework stays usable without Python: the engine builds and tests
with no interpreter present, and this package is a binding rather than
the way the framework is used. That is
[D16](https://github.com/openquanter/openquanter/blob/main/docs/IMPLEMENTATION.md),
along with why the small surface came first.

Apache-2.0.
