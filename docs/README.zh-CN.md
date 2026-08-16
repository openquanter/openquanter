# OpenQuanter 文档

[English](README.md) · [中文](README.zh-CN.md)

项目文档以英文撰写，并提供中文对照版本。每篇文档顶部都有中英文互跳链接。

| 文档 | 内容 |
|---|---|
| [快速上手](QUICKSTART.zh-CN.md) · [English](QUICKSTART.md) | 从克隆到跑通回测、三个示例、如何写自己的策略 |
| [版本规则](VERSIONING.zh-CN.md) · [English](VERSIONING.md) | 全工作区一个版本、为什么从 2 开始、每个阶段承诺什么 |
| [需求规格说明](REQUIREMENTS.zh-CN.md) · [English](REQUIREMENTS.md) | 定位、目标用户、功能与非功能需求、验收目标、保真度阶梯、非目标 |
| [路线图](ROADMAP.zh-CN.md) · [English](ROADMAP.md) | M0–M5 与 1.0 各里程碑、启动触发条件与验收门、发布节奏、风险清单 |
| [实施方案](IMPLEMENTATION.zh-CN.md) · [English](IMPLEMENTATION.md) | 架构、设计决策、crate 划分、分阶段任务计划、测试策略、性能预算 |
| [Tick 格式规范](TICK-FORMAT.zh-CN.md) · [English](TICK-FORMAT.md) | 回测读取的磁盘格式：布局、只追加字段规则、完整性与身份的区分 |
| [采集格式规范](CAPTURE-FORMAT.zh-CN.md) · [English](CAPTURE-FORMAT.md) | 行情采集的分帧、控制记录、按天密封、归档校验与容量规划 |

## 阅读顺序

初次了解：先看 [README](../README.zh-CN.md) 的一页式介绍，再读**需求规格说明**
（框架要做什么）、**路线图**（什么时候做）、**实施方案**（怎么做）。

准备贡献代码：从[实施方案](IMPLEMENTATION.zh-CN.md) §5 的任务清单和 §6 的测试
策略入手，并阅读 [CONTRIBUTING.md](../CONTRIBUTING.md) 与 [AGENTS.md](../AGENTS.md)。

## 文档状态

三篇文档均为**评审稿**，描述的是设计意图而非已交付功能——当前仓库仍是 pre-alpha
阶段的骨架。欢迎以 issue 形式讨论与指正。

翻译约定：英文版为准。两版出现分歧时以英文为权威，中文版的差异按 bug 处理。
