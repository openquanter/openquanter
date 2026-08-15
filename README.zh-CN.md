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

Pre-alpha。当前仓库只包含初始 crate 骨架。里程碑进度通过本仓库的 issues 和
milestones 跟踪；每个里程碑解锁什么能力见[路线图](docs/ROADMAP.zh-CN.md)。

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

见 [CONTRIBUTING.md](CONTRIBUTING.md)。提交贡献即表示你接受项目的贡献条款
（需要 DCO sign-off；实质性贡献需签署 CLA）。社区支持为 best-effort，
1.0 之前不承诺 SLA。
