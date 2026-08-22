# 参与 OpenQuanter 贡献

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

感谢关注！项目处于早期开发阶段，当前最有价值的帮助是：试用它、提交精确的 issue、
参与设计讨论。

## 基本规则

- **许可证**：Apache-2.0。所有贡献均以该许可证接受。
- **Sign-off**：每个 commit 必须带 DCO sign-off（`git commit -s`），声明你有权
  提交该代码。CI 会在每个 PR 上检查；本地用同一套检查：

  ```bash
  scripts/check-dco.sh              # 检查 origin/main..HEAD
  git rebase --signoff origin/main  # 为整个分支补签
  ```

- **CLA**：实质性贡献会被要求同意[贡献者许可协议](CLA.md)
  （[中文参考译文](CLA.zh-CN.md)）。它是许可而非著作权转让——著作权仍归你。
- **确定性不可侵犯**：事件核内部的改动不得引入读时钟、随机数、I/O 或线程。
  CI 强制属性测试不变量；golden 基线只有经维护者确认才能变更。
- **不提交专有内容**：不要提交交易所凭证、采集的行情数据，或含实盘参数的策略。

## 开发

Rust 2024 edition；最低支持版本 1.85。

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

文档以英文撰写并提供中文对照（`*.zh-CN.md`）。英文为准，双语在同一个 PR 内同步
更新。

## 从哪里入手

[路线图](docs/ROADMAP.zh-CN.md)说明每个里程碑解锁什么能力，
[实施方案](docs/IMPLEMENTATION.zh-CN.md)把它拆成带完成标准的任务。当前最有用的
贡献是：带复现 seed 的精确 bug 报告、交易所适配器、描述通用失败模式的仿真场景、
以及文档。

社区支持为 best-effort，2.0 之前不承诺 SLA。开新 issue 前请先搜索已有 issue。
