# OpenQuanter

**基于 Rust 的确定性、AI-native 量化交易框架 —— 从 CTA 到高频。**

[English](README.md) · [中文](README.zh-CN.md)

> ⚠️ 早期开发阶段。1.0 之前 API 不保证稳定。本项目不构成投资建议，使用风险自负。

## OpenQuanter 是什么？

OpenQuanter 是一个围绕**确定性事件核**构建的开源交易框架：回测与实盘共用同一个
引擎，二者只在事件生产者上有差别。它优先面向加密永续合约市场，并提供一条保真度
阶梯——从用于快速研究的 tick 回放，一直到订单簿级别的仿真。

2.x 是在 Rust 核之上的**全新架构重写**，而不是对旧版本的增量移植。

### 设计支柱

以下描述的是项目**正在建设的目标架构**。今天实际有什么见下方[当前状态](#当前状态)——
如果你在判断现在能不能用，请先看那一节。

- **确定性事件核** —— 由定序、落 journal 的事件流驱动的纯状态机（LMAX/Aeron
  谱系）。任何一次运行都可以由 `(journal, seed)` 完整重放；崩溃恢复、审计流、
  仿真测试三者共用同一套机制。
- **带保证金的回测** —— 分级维持保证金、强平价路径、资金费尖峰场景都是一等
  公民。多数开源回测器永远不会爆仓，真实交易所会。
- **保真度阶梯** —— L0 tick 回放用于快速参数扫描 → 排队位置与延迟建模 →
  L2 订单簿重建。每次回测都会输出保真度报告（参与率、延迟假设、保证金峰值）。
- **双层策略** —— 延迟敏感策略用 Rust trait；研究迭代用 Python（PyO3）。
  单一类型系统，不维护两套运行时。
- **AI 一等公民** —— 进程内 ONNX / 编译树推理、向量化 gym 式训练环境、
  面向 LLM 研究的沙盒接口。
- **内建过拟合统计** —— 参数扫描默认输出 Deflated Sharpe Ratio 与
  Probability of Backtest Overfitting。

## 文档

| 文档 | 内容 |
|---|---|
| [需求规格说明](docs/REQUIREMENTS.zh-CN.md) | 框架必须做到什么，以及如何验收 |
| [路线图](docs/ROADMAP.zh-CN.md) | 里程碑、启动触发条件、验收门、通往 1.0 的路径 |
| [实施方案](docs/IMPLEMENTATION.zh-CN.md) | 架构、设计决策、crate 划分、任务计划 |

完整索引见 [docs/](docs/README.zh-CN.md)。

## 当前状态

Pre-alpha，并且把话说清楚。**今天已建成并有测试覆盖的**：

- **确定性内核** —— 定序 journal、逐位精确的重放（有测试断言，含一条强平路径）、
  撕裂尾恢复、"先落盘再 apply"由故障注入测试强制。
- **L0 撮合** —— tick 回放，含跳空成交、价格改善、价格时间优先。已冻结为回归锚点。
- **保证金与成本** —— 分级维持保证金、强平价由推导得出而非照抄、资金费与尖峰注入、
  规则表双时间存储，以及 maker/taker 手续费（maker 费率可为负——返佣是存在的）。
- **回测宿主** —— 含保证金偏差报告：同一策略跑两遍，量化无保证金那一臂虚报了多少。
- **数据平面** —— 双时戳、无泄漏的 as-of join、双时间参考数据。
- **采集** —— 原样落盘的交易所记录 + 本地时戳、按 UTC 日密封、带内容哈希的
  manifest。已对真实交易所验证。
- **统计** —— Deflated Sharpe Ratio、回测过拟合概率、试验登记。
- **Parity** —— 逐笔 diff 与差异归因，基线由代码、数据、配置三者共同标识。

**已设计但尚未建成**：保真度 L1 与 L2 档（目前只有 L0）、Python 策略层、
参数扫描器、实盘交易，以及上面「AI 一等公民」下的全部内容。设计支柱那一节讲的是
方向，这一节讲的是现状。

从[快速上手](docs/QUICKSTART.zh-CN.md)开始——三个示例、无需下载数据、几分钟内
就能跑起一次回测。各里程碑解锁什么能力、由什么触发，见[路线图](docs/ROADMAP.zh-CN.md)。

## 构建

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## 许可证

Apache-2.0，见 [LICENSE](LICENSE)。

## 参与贡献

见 [CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md)。提交贡献即表示你接受项目的贡献
条款：每个 commit 需要 DCO sign-off，实质性贡献需同意
[CLA](CLA.zh-CN.md)。社区支持为 best-effort，
1.0 之前不承诺 SLA。
