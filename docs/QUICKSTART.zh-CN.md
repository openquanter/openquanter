# 快速上手

> [English](QUICKSTART.md) · 目标：从干净的机器开始，30 分钟内跑起一次回测。

## 1. 构建

Rust 2024 edition，最低版本 1.85。不需要起服务，不需要下载数据——示例的行情由
种子自己生成。

Cargo 只为需要和交易所通信的 crate 拉依赖树——采集侧的 `oq-l2feed` 和
`oq-ingest`、读账户的 `oq-gateway`——外加 `oq-examples` 用于跑基准的
dev-dependency `criterion`。引擎本身——类型、journal、内核、撮合、保证金、回测、
数据、parity、统计——是纯 std Rust，这一点由 `scripts/check-composability.sh`
在 CI 里强制。只要引擎的话，`cargo build -p oq-core` 什么都不拉。

命令行工具随这些 crate 一起发布，不是独立的包，所以 crates.io 上没有一个叫
`oq-capture` 的东西可装：

```bash
cargo install oq-cli      # oq —— 一个名字找到其余全部
cargo install oq-l2feed   # oq-capture、oq-book-check、oq-trade-check、oq-merge、oq-resequence
cargo install oq-ingest   # oq-ingest
cargo install oq-gateway  # oq-recon、oq-order-check
cargo install oq-live     # oq-trade
```

`oq` 单独执行会列出每个工具和它的用途，`oq <工具>` 则把参数原样转发给它。
**它值得第一个装：它是唯一一个会告诉你其余工具存在的。**

```bash
git clone https://github.com/openquanter/openquanter
cd openquanter
cargo test --workspace
```

测试通过，下面的内容就都能跑。

## 2. 跑第一个示例

```bash
cargo run --example hello
```

```text
strategy      buy-and-hold
observations  2000
fills         1
final equity      10861.94 USDT
lowest equity      9993.38 USDT
liquidations  0
```

二十行策略：买一次，然后一直拿着。它的作用是让你看到这个循环转起来——策略收到
观测、返回意图，宿主负责撮合与记账。先读
[`crates/oq-examples/examples/hello.rs`](../crates/oq-examples/examples/hello.rs)，
一屏之内就是全部 API 面。

## 3. 跑那个解释了整个项目的示例

```bash
cargo run --example martingale_ladder
```

```text
                        enforced      margin-free
final equity             61.53     20908.11
lowest equity            61.53    -30302.14
fills                        4                6
liquidations                 1                0

martingale-ladder: LIQUIDATED 1x, margin-free equity 20908.11 vs real 61.53
(overstated by 20846.58); 2 fills in the margin-free run happened after the
account was already closed
```

同一个策略、同一段行情，跑两遍：一遍启用强平，一遍关掉。**关掉强平的回测——也就是
多数开源回测器给你的东西——声称赚了 20908 USDT，而真实账户最终只剩 61.53。**

真正的破绽是最低权益那一行：**−30302**。权益为负不是回撤，是这个账户已经不存在了。
之后的每一笔成交，都是一个交易所早已关闭的账户下的单，报告里把它们数了出来。

这就是保证金叠加层存在的理由，也是保真度阶梯为什么把账户真实度单列成一个维度。

## 4. 跑 API 导览

```bash
cargo run --example ma_cross
```

均线交叉：指标、翻仓、`on_fill`。它的参数是**运行之前就定好的整数、事后没有调过**
——因为一个调过参的示例，本质是把过拟合的教训包装成教程。如果你改了窗口发现结果
变好了，那你刚做的正是 `oq-stats` 要惩罚的那种搜索。

### 上面这些数字漏掉了一件事

**这些示例不收手续费。** 本页每一个数字都是不含成本的毛数，因为没有一个示例
设置了费率表。手续费**是被建模的**——maker 与 taker 两档，而且 maker 费率可以为负，
因为返佣是存在的——但它默认为零，必须显式要：

```rust
use oq_backtest::Fees;
use oq_types::Ratio;

let config = config.with_fees(Fees::flat(Ratio::from_ppm(500))); // 0.05%
```

默认为零，是为了让"一次没有费率表的运行"**显然就是**一次没有费率表的运行，而不是
一次悄悄用了某个没人选过的、看起来还挺合理的数字的运行。对交易频繁的策略，这个
差别不是装饰性的；这样一个项目的文档应该把它说出来，而不是让你自己撞见。

## 5. 写你自己的

一个策略就是一个 trait，只有一个必须实现的方法：

```rust
use oq_backtest::{Context, Intent, Strategy};
use oq_types::{OrderId, QtyLots, Side};

struct Mine;

impl Strategy for Mine {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if ctx.position.0 == 0 {
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(1),
            });
        }
    }

    fn name(&self) -> &str { "mine" }
}
```

策略没有时钟、没有 I/O、也拿不到引擎的句柄。这是刻意的：它**无法引入不确定性**，
也**绕不过风控层**——因为它手里没有任何可以伸过去的东西。

## 6. 数据从哪来

示例跑在**生成的**行情上，seed 固定，所以每台机器产出的序列完全一致——这也是上面
那些数字可以被引用、并且能被 golden 测试钉死的原因。

要用真实数据，`oq-l2feed` 会原样采集交易所的流：

```bash
cargo run --bin oq-capture -- \
  --root ./archive --symbol BTCUSDT --stream depth --minutes 10 --floor-gb 10
```

然后检查采到的东西是不是真的能用：

```bash
cargo run --bin oq-book-check -- --file ./archive/<venue>/BTCUSDT/depth/<date>.oqcap
```

它把每一条深度更新重放进订单簿，报告结果——应用了多少条、采集端**声明**了多少
个 gap、有多少个**没人声明**的序列断裂、订单簿有没有交叉过——最后给出一行结论：
`RECONSTRUCTS CLEANLY`，或者具体是哪条规则没过。计数取决于你自己的采集，所以
这里不引用具体数字。

**第一天就跑，别等半年。** 文件躺在磁盘上只能证明消息到过；只有把它们重放进订单簿
才能证明它们**可用**。一个采集缺陷——重连没处理好、序列号字段读错了、某个流其实是
被合并过的——在磁盘上看起来完全健康，而等它暴露出来时，被它毁掉的那段窗口已经无法
重采。归档名不副实时这个命令会以非零码退出，所以它应该被放进采集之后的流程里。

归档布局、密封与校验流程、以及**交易所实际提供了什么**（包括那些接受订阅之后
什么都不发的流），见[采集格式规范](CAPTURE-FORMAT.zh-CN.md)。

## 7. 用你采到的数据跑回测

归档还不是引擎能读的东西。`oq-ingest` 把采集到的深度和成交折叠成回测重放的 tick
格式：

```bash
cargo run --bin oq-ingest -- \
  --archive ./archive/binance-perp/BTCUSDT --day 2026-08-16 --out btc.ticks
```

它会报告构建出了什么——产出多少窗口、其中多少个带成交、看到多少 gap marker、多少
条无法解析——**让转换太稀薄这件事可见，而不是悄悄地小**。`--window-ms` 设定窗口
长度，默认一秒。

**转换是有意有损的。** 一个窗口的 L2 深度只留下最优买价和最优卖价，背后的簿被丢弃。
这个取舍能成立，只因为归档被保留着：采集才是记录本身，这只是它的一个投影，服务于
投影能承载其决策的那类策略。**需要簿本身的策略需要 L2 保真层**，那一层还不存在——
更丰富的 tick 替代不了它。

如果你要直接读产出，有两个约定必须知道。**极值属于自己的窗口**：`high` 和 `low`
是**这个窗口内**成交的最高最低，绝不是向前滚动的最大值。**成交量是累计的**，所以
单窗口成交量是相邻两个 tick 的差；差值为负意味着交易所重置了计数器，而不是成交被
撤销了。

报价精度来自交易所的品种表而不是默认值，因为**精度错了不会报错——它会静默地把每个
价格缩放掉**。品种未知时这个工具会停下，而不是去猜。

## 接下来读什么

| 你想 | 读 |
|---|---|
| 知道现在有什么、还没有什么 | [README 当前状态](../README.zh-CN.md#当前状态) |
| 知道这个框架承诺什么 | [需求规格说明](REQUIREMENTS.zh-CN.md) |
| 知道每个里程碑解锁什么 | [路线图](ROADMAP.zh-CN.md) |
| 理解架构 | [实施方案](IMPLEMENTATION.zh-CN.md) |
| 知道 `2.0.0-alpha` 承诺了什么 | [版本规则](VERSIONING.zh-CN.md) |
| 读写归档与 tick 文件 | [采集格式规范](CAPTURE-FORMAT.zh-CN.md) · [Tick 格式规范](TICK-FORMAT.zh-CN.md) |
| 参与贡献 | [CONTRIBUTING.zh-CN.md](../CONTRIBUTING.zh-CN.md) |

## 关于这些示例

**每个示例都预期是亏钱的，没有一个是拿来跑的策略。** 它们演示的是框架的性质。
一个立身之本是"回测会骗你"的项目，不适合用漂亮的权益曲线招揽人。

## 你已经认识的那些策略

```bash
cargo run --release -p oq-examples --example classics
```

六个经典策略——RSI 反转、MACD、布林带、唐奇安突破、网格、Dual Thrust——用它们
各自**已发表的参数**，没有在这里调过。

**没有一个是推荐。** 每一个都有几十年历史、被足够多的人交易过，它们曾经有过的
边际不会在一个公开仓库里等着。它们在这里，是为了让框架可以**靠认出一个已知的
东西**来学，而不是同时学两样新东西。

这个例子不会打印一条资金曲线就结束。它把每个策略在**建模强平**和**不建模强平**
两种模式下各跑一遍并同时打印——因为一条永远不会被强平的曲线，描述的是任何交易所
都不提供的账户。**不加杠杆时六个策略的两列完全相同**，而这本身就是结论：
**保证金模型在杠杆变成真的之前是看不见的。** 加了杠杆之后，网格最终剩 4.06、
账户被交易所关闭两次，而 margin-free 那一侧为一个它一直持有着的仓位报告 −513.74。
