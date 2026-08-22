# 快速上手

> [English](QUICKSTART.md) · 目标：从干净的机器开始，30 分钟内跑起一次回测。

## 1. 构建

Rust 2024 edition，最低版本 1.85。不需要起服务，不需要下载数据——示例的行情由
种子自己生成。

Cargo 只会为两个 crate 拉依赖树，其余一个都没有：`oq-l2feed` 带 WebSocket 与 TLS
栈，因为它必须和交易所通信；`oq-examples` 把 `criterion` 作为 dev-dependency，
用于跑基准。引擎本身——类型、journal、内核、撮合、保证金、回测、数据、parity、
统计——是纯 std Rust，这一点由 `scripts/check-composability.sh` 在 CI 里强制。
只要引擎的话，`cargo build -p oq-core` 什么都不拉。

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
