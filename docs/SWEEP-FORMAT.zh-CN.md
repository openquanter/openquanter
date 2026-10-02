# 扫描文件

[English](SWEEP-FORMAT.md) · [中文](SWEEP-FORMAT.zh-CN.md)

一次参数扫描发现了什么、以及它能不能被信任——放在同一个文件里。

```text
openquanter-sweep 2
label ma-cross calm
equity-every 100
trials 100 340
thresholds 0.35 0.95 0
deflated-sharpe 0.41
pbo 0.25 16 0.3 0.12 0.8
logits -1.2 0.4 …
refusal <sentence>
config <label>	<fills>	<realized>	<fees>	<final equity>	<min equity>	<liquidations>	<sharpe or ->
unscorable <label>
lookahead <label>	<verdict>
```

由 `oq_backtest::sweep_file::render` 写出，由 `SweepFile::parse` 读回。

## 它为什么存在

策略是编译进二进制的 Rust，所以一次扫描跑在调用方自己的程序里，没有一个
`oq sweep` 能接住它的输出。这是另一半：调用方把扫描写进一个文件，任何读它的东西
都能拿到每个配置的结果，**以及**摆在旁边的过拟合统计量——包括某个统计量没能算出的
原因。一张只有胜出者、旁边没有 deflated Sharpe 与回测过拟合概率的表，正是扫描存在
就是要拒绝的东西，所以这个格式没有办法只带其一、不带其二。

## 写出一个

```bash
cargo run --release -p oq-examples --example sweep_100 -- --out sweep.txt
```

`sweep_100` 照常跑完它的一百个配置；带上 `--out FILE` 时，还会以标签
`sweep_100 ma-cross calm`、按默认阈值评判，把结果写到那里。带上 `--ledger FILE` 时还会读写一份[试验账本](#试验账本)。你自己的程序则对扫描
返回的 `SweepReport` 调用 `sweep_file::render(label, &report, thresholds)`，把得到
的字符串写到任何地方。

## 各行

第一行是 `openquanter-sweep 2`。其余每一行是一个事实，由它的第一个词命名；一行里有
多个字段时用制表符分隔，标签里的制表符或换行会被替换成空格，所以断不了行。

| 行 | 承载 |
|---|---|
| `label` | 这次扫描是什么，由调用方命名 |
| `equity-every` | 每个采样收益跨多少个 tick。没有它的 Sharpe 比率不是一个能拿来比较的数 |
| `trials` | 本次扫描的配置数，以及 deflated Sharpe 所用的试验总数——本次的加上[试验账本](#试验账本)里此前所有的，能否打分都算。版本 1 的文件没有这一行 |
| `thresholds` | 评判这次扫描所用的：可接受的最大 PBO、可接受的最小 deflated Sharpe、可接受的最小"样本外对样本内"斜率 |
| `deflated-sharpe` | 最优配置的 deflated Sharpe 比率 |
| `pbo` | 回测过拟合概率、拆分数、样本外亏损概率、样本外 Sharpe 中位数、以及退化斜率 |
| `logits` | 计算 PBO 所用的各拆分 logit，以空格分隔 |
| `refusal` | 这次扫描不得打包的一条理由，一行一条。没有任何拒绝时不出现 |
| `config` | 一个配置：标签、成交数、已实现盈亏、手续费、最终权益、最低权益、强平次数、Sharpe 比率——收益太少无法打分时写 `-`。金额以账户币种计，不是定点单位 |
| `unscorable` | 产出的收益太少、无法打分的配置 |
| `lookahead` | 得分最高那个配置的前视检查：`clean over N signal(s)` 或 `N divergence(s) in M checked` |

没能算出的统计量写成 `<名字> - <原因>`——例如 `pbo - fewer than two configurations
scored`——读回来的是那个原因，绝不是零。

## 读取方拒绝什么

第一行既不是 `openquanter-sweep 1` 也不是 `openquanter-sweep 2` 的会被拒绝（版本 1
就是没有 `trials` 行的版本 2），理由与 [run 文件](RUN-FORMAT.zh-CN.md)
拒绝它不认识的版本一样：只读认得的部分、忽略其余，正是较新的文件被较旧的读取方
误读的方式。第一个词不在上表之列的非空行会被拒绝，少了八个字段中任何一个的 `config` 行
也会被拒绝；错误信息会指出行号。

## 试验账本

Deflated Sharpe 要问的是一共试了多少个配置。只数自己的扫描，会让这个数被压小：把
网格拆成几段跑，或者忘掉那次结果难看的扫描。所以 `sweep` 把每个候选都记进调用方交给
它的 `TrialRegistry`，并按这份登记里的全部试验去 deflate。收益太少、无法打分的配置
也计数：它们确实被试过。

登记以账本文件的形式活过程序本身，一个研究问题一份（一类策略在一批数据上）：

```text
openquanter-trials 1
basis equity-every=64
trial <sharpe>	<observations>	<skewness>	<kurtosis>	<id>
unscored <id>
```

`oq_backtest::ledger::load` 读取它——文件不存在就是空账本，读不了是错误而不是清零——
`ledger::save` 先写到旁边再改名，整份替换。

`basis` 是这些 Sharpe 的采样频率。采样频率不同的扫描不会写入账本，并把 deflated Sharpe
报告为无法计算，闸门因此拒绝它：两种频率下的 Sharpe 没有共同的离散度，而只按本次扫描
deflate 又会忘掉此前那些。

