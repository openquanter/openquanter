# The sweep file

[English](SWEEP-FORMAT.md) · [中文](SWEEP-FORMAT.zh-CN.md)

What a parameter sweep found, and whether it can be trusted, in one file.

```text
openquanter-sweep 1
label ma-cross calm
equity-every 100
thresholds 0.35 0.95 0
deflated-sharpe 0.41
pbo 0.25 16 0.3 0.12 0.8
logits -1.2 0.4 …
refusal <sentence>
config <label>	<fills>	<realized>	<fees>	<final equity>	<min equity>	<liquidations>	<sharpe or ->
unscorable <label>
lookahead <label>	<verdict>
```

Written by `oq_backtest::sweep_file::render` and read back by
`SweepFile::parse`.

## What it is for

A strategy is compiled Rust, so a sweep runs inside the caller's program
and there is no `oq sweep` to hold its output. This is the other half:
the caller writes the sweep to a file, and anything that reads it gets
every configuration's outcome **and** the overfitting statistics beside
them — including the reason a statistic could not be computed. A table
of winners without the deflated Sharpe ratio and the probability of
backtest overfitting next to it is the thing a sweep exists to refuse,
so the format has no way to carry one without the other.

## Writing one

```bash
cargo run --release -p oq-examples --example sweep_100 -- --out sweep.txt
```

`sweep_100` runs its hundred configurations as it always does and, with
`--out FILE`, also writes the result there under the label
`sweep_100 ma-cross calm`, judged against the default thresholds. A
program of your own calls `sweep_file::render(label, &report,
thresholds)` on the `SweepReport` its sweep returned and writes the
string wherever it likes.

## The lines

The first line is `openquanter-sweep 1`. Every other line is one fact,
named by its first word; a line with several fields separates them with
tabs, and a tab or newline inside a label is replaced with a space so it
cannot break a line.

| Line | Carries |
|---|---|
| `label` | What the sweep was, as the caller named it |
| `equity-every` | Ticks per sampled return. A Sharpe ratio without it is not a number anyone can compare |
| `thresholds` | The largest acceptable PBO, the smallest acceptable deflated Sharpe ratio, and the smallest acceptable out-of-sample-on-in-sample slope the sweep was judged against |
| `deflated-sharpe` | The deflated Sharpe ratio of the best configuration |
| `pbo` | The probability of backtest overfitting, the number of splits, the probability of an out-of-sample loss, the median out-of-sample Sharpe ratio, and the degradation slope |
| `logits` | The split logits the PBO was computed from, space-separated |
| `refusal` | One reason the sweep must not be packaged, one line each. None when nothing refused it |
| `config` | One configuration: label, fills, realized P&L, fees, final equity, minimum equity, liquidations, Sharpe ratio — `-` when there were too few returns to score it. Amounts are in account currency, not the fixed-point unit |
| `unscorable` | A configuration that produced too few returns to score |
| `lookahead` | The lookahead check of the best-scoring configuration: `clean over N signal(s)` or `N divergence(s) in M checked` |

A statistic that could not be computed is written `<name> - <reason>` —
`pbo - fewer than two configurations scored` — and read back as the
reason, never as zero.

## What a reader refuses

A first line other than `openquanter-sweep 1` is refused, for the reason
[the run file](RUN-FORMAT.md) refuses a version it does not know: reading
what is recognised and ignoring the rest is how a newer file gets
misread by an older reader. A non-empty line whose first word is not one of the
above is refused, and so is a `config` row without its eight fields; the
error names the line number.
