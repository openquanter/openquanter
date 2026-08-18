# Margin fidelity: how wrong is a backtest with no margin model?

A backtest that does not model liquidation is not missing a detail. It is
answering a different question — *what would this strategy have earned if
the venue could never close the account* — and reporting the answer as
though it were the first one. This document defines the instrument that
measures the difference, states what it can and cannot claim, and shows
the numbers it produces on a strategy chosen because the difference is
large.

It is the methodology behind gate **G5** in [the roadmap](ROADMAP.md).

## The control arm

`MarginMode::Ignored` is not a debugging switch. It is *exactly* the
assumption a margin-free backtest makes silently, so running the same
strategy over the same ticks under `Enforced` and `Ignored` measures one
specific thing: the error that assumption causes. Nothing else differs —
same strategy object, same ticks, same fees, same funding. Two models
being compared would be a much weaker experiment; this is a model
compared against its own absence.

## Why not a mean

The obvious summary is the average difference in return. It is the wrong
statistic, for a reason that is structural rather than statistical.

The two arms are *identical, tick for tick*, until the first liquidation.
Before it, the overlay changes nothing. After it, one arm holds nothing
and the other holds a position no venue would have permitted. So the
difference is zero almost everywhere and enormous in a few places. Every
statistic that averages over the whole series — mean return, Sharpe, even
maximum drawdown — divides that concentration by the length of the run
and reports a small number for a fatal difference.

## Why not the within-run return series either

The natural next thought is to take quantiles of the *return series*
within a run. `tail_divergence` does exactly this, and the result is a
useful negative:

```
tail divergence   paired over 460 return samples
                  arms part at sample 459 of 460

  quantile        enforced      margin-free     overstated by
        1%        -14.6406%       -17.9768%         -3.3362%
        5%         -4.3673%        -4.5896%         -0.2223%
       10%         -1.2137%        -1.2300%         -0.0162%
       25%         -0.0488%        -0.0488%         -0.0000%
```

The arms part at sample 459 of 460 — which is to say, at the liquidation
and not before. The paired quantiles are near-identical *by construction*,
and the small numbers they show are not evidence that margin barely
matters. In this very run the account ended at 61.53 while the margin-free
arm claimed 20,908.11. The damage is entirely outside the paired region,
which is why `Fidelity::paired_until` is reported and why every paired
statistic stops there rather than differencing two series that have
stopped describing the same account.

This is worth stating plainly because the instrument is available and
would be misread: **do not quote within-run tail quantiles as a fidelity
result.**

## The unit is a window

The right observation is one number per *window*: the total return of
each arm over a stretch of market. Windows are paired with themselves,
nothing is truncated, and the distribution across windows has a tail that
means something — most windows are calm and the arms agree; a few contain
a drawdown the account did not survive, and there the arms disagree
completely.

Run over forty windows of a synthetic market, twenty-eight calm and
twelve containing a crash of 22% to 68%, with a martingale ladder — a
strategy chosen because it is the clearest case, not a representative one:

```
  per-window return, by quantile
    quantile      enforced    margin-free       gap
          5%       -97.71%          -2.84%     94.87%
         10%       -96.77%          -2.06%     94.71%
         25%       -94.91%          -1.28%     93.63%
         50%        -1.32%           0.43%      1.75%
         75%         0.41%          99.58%     99.18%
```

Read the 5% row and the 50% row together. At the median the two arms
agree to within 1.75 points: the overlay never bit, and a margin-free
backtest was right. At the 5th percentile the account lost 97.71% and the
margin-free backtest reported a loss of 2.84%.

And then read the 5% row across, rather than down. The margin-free arm's
*worst* windows are ordinary small losses. The windows that ruined the
real account are not in its left tail at all — they are in its right
tail, at +99.58% and above, because a position carried through a hole and
out the other side books the recovery. **A margin-free backtest does not
merely understate these windows. It files them under success.**

## What moves with the window mix, and what does not

The proportion of stressed windows in a study is a choice. Two of the
statistics move with it and one does not, and a study that fails to
distinguish them is quoting numbers it invented:

| statistic | moves with the mix? |
|---|---|
| mean gap | yes — roughly proportional to the stressed fraction |
| worst-decile share | yes — a study of nothing but crashes reports ~10% |
| **return given liquidation** | **no** — conditional on liquidation having occurred |

The conditional statistic is the one to quote alone:

```
  in the 12 windows that closed the account:
    the account got               -96.23%
    the margin-free run said      599.89%
```

That sentence survives padding the study with any number of calm windows,
and `StressReport::given_liquidation` is tested against exactly that:
adding ninety calm windows collapses the mean by more than fivefold and
leaves the conditional pair unchanged.

The mix-dependent numbers are still worth reporting, next to their mix:

```
  windows         40 (28 calm, 12 stressed by construction)
  mean gap        208.83%   <- what a naive comparison reports
  worst decile      61.0%   <- share of the total gap it carries
```

## What this does not establish

- **Not a frequency claim.** The 28:12 mix was chosen to put observations
  in the tail. It is not an estimate of how often markets crash, and no
  number here should be read as a probability of ruin.
- **Not a strategy claim.** A martingale ladder is the clearest
  demonstration, not a typical strategy. A strategy that cuts losses will
  show a much smaller divergence — and a study on such a strategy that
  finds none has learned something real: for it, on this data, a
  margin-free backtest was adequate. `worst_decile_share` is `None` and
  `given_liquidation` is `None` in that case, rather than zero, because
  "the question did not arise" and "the answer was zero" are different
  findings.
- **Not venue-calibrated.** The tier table is
  `TierTable::example_btcusdt()`. Real maintenance-margin schedules differ
  by venue, by instrument, and over time, and a study meant to inform a
  real deployment must use the real table.
- **Synthetic market.** The windows come from a generator, not from
  history. Replacing them with captured data changes only the input to
  `Window::of`; nothing in the instrument assumes synthetic data.

## Reproducing

```text
cargo run --release --example martingale_ladder   # one window, in detail
cargo run --release --example margin_fidelity     # the forty-window study
```

The instrument is `oq_backtest::fidelity`:
[`tail_divergence`](../crates/oq-backtest/src/fidelity.rs) for the
within-run comparison and its limits, [`Window::of`] and [`stress`] for
the study. Both refuse rather than round: a quantile finer than the data
can support returns `Unusable::TooFewSamples` instead of silently
reporting the minimum, and two runs over different tick counts are
refused as not being one experiment.
