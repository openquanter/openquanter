# OpenQuanter 文档

[English](README.md) · [中文](README.zh-CN.md)

项目文档以英文撰写，并提供中文对照版本。每篇文档顶部都有中英文互跳链接。

| 文档 | 内容 |
|---|---|
| [**为什么有 OpenQuanter**](WHY.zh-CN.md) · [English](WHY.md) | **定位、目标人群、前身实盘多年撞到的墙、远景，以及现在到哪了** |
| [快速上手](QUICKSTART.zh-CN.md) · [English](QUICKSTART.md) | 从克隆到跑通回测、三个示例、如何写自己的策略 |
| [版本规则](VERSIONING.zh-CN.md) · [English](VERSIONING.md) | 全工作区一个版本、为什么从 2 开始、每个阶段承诺什么 |
| [需求规格说明](REQUIREMENTS.zh-CN.md) · [English](REQUIREMENTS.md) | 定位、目标用户、功能与非功能需求、验收目标、保真度阶梯、非目标 |
| [路线图](ROADMAP.zh-CN.md) · [English](ROADMAP.md) | M0–M5 与 2.0 各里程碑、启动触发条件与验收门、发布节奏、风险清单 |
| [实施方案](IMPLEMENTATION.zh-CN.md) · [English](IMPLEMENTATION.md) | 架构、设计决策、crate 划分、分阶段任务计划、测试策略、性能预算 |
| [Tick 格式规范](TICK-FORMAT.zh-CN.md) · [English](TICK-FORMAT.md) | 回测读取的磁盘格式：布局、只追加字段规则、完整性与身份的区分。**§4 起是提案中的 v3**；`oq-data` 实现的是 v2 |
| [保证金保真度](MARGIN-FIDELITY.zh-CN.md) · [English](MARGIN-FIDELITY.md) | **没有保证金模型的回测错得有多离谱**、为什么答案是跨窗口尾部而不是均值、以及哪些数字在改变窗口配比后依然成立 |
| [执行契约](EXECUTION.zh-CN.md) · [English](EXECUTION.md) | 与交易所无关的下单契约：三态结果、客户端订单号、以及"答案丢失"的代价 |
| [实盘路径](LIVE-PATH.zh-CN.md) · [English](LIVE-PATH.md) | 先落账再发送、进程被杀后的恢复、以及 supervisor 被允许决定什么 |
| [采集格式规范](CAPTURE-FORMAT.zh-CN.md) · [English](CAPTURE-FORMAT.md) | 行情采集的分帧、控制记录、按天密封、归档校验与容量规划 |
| [变更日志](../CHANGELOG.zh-CN.md) · [English](../CHANGELOG.md) | 变了什么；语义或事件 schema 的改动必须记录在哪里 |

## 阅读顺序

初次了解：先看 [README](../README.zh-CN.md) 的一页式介绍，再读**需求规格说明**
（框架要做什么）、**路线图**（什么时候做）、**实施方案**（怎么做）。

准备贡献代码：从[实施方案](IMPLEMENTATION.zh-CN.md) §5 的任务清单和 §6 的测试
策略入手，并阅读 [CONTRIBUTING.md](../CONTRIBUTING.md) 与 [AGENTS.md](../AGENTS.md)。

## 文档状态

这些文档描述的**不是同一类东西**，混读是误解本项目最容易的方式：

| 文档 | 描述的是 |
|---|---|
| 快速上手、版本规则、变更日志 | **已经存在的东西。** 今天就能跑的命令；数字由 `crates/oq-examples/tests/golden.rs` 钉住 |
| 采集格式规范 | **已实现的东西**，由 `oq-l2feed` 实现 |
| Tick 格式规范 | §1–§3 是 `oq-data` 已实现的 v2；§4 起是**提案中的 v3** |
| 需求规格说明、路线图、实施方案 | **意图。** 评审稿——框架必须做到什么、将怎么建，而不是已交付什么 |

已建/未建的划分，以 [README](../README.zh-CN.md) 的"当前状态"一节为唯一答案。
欢迎以 issue 形式讨论与指正。

翻译约定：英文版为准。两版出现分歧时以英文为权威，中文版的差异按 bug 处理。
