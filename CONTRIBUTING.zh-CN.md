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
cargo test
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

是 `cargo test` 而不是 `--workspace`：带 `--workspace` 会一并构建 `oq-py`，
它的测试要链接 CPython 的共享库，在 Python 版本对不上的机器上会失败。绑定有
自己的 CI job 和钉住的解释器——`cargo clippy -p oq-py` 与 `cargo test -p oq-py`
——在本地跑它们需要那个解释器在场。

文档以英文撰写并提供中文对照（`*.zh-CN.md`）。英文为准，双语在同一个 PR 内同步
更新。

## 分支与合并

主干开发：`main` 始终可发布，分支保持短命。

| | |
|---|---|
| **命名** | `feat/`、`fix/`、`docs/`、`chore/` 加简短主题 |
| **寿命** | 一到两天内合入 |
| **合并方式** | squash，一个 PR 在 `main` 上留下一个提交 |
| **合并后** | 分支自动删除 |

分支的代价随存活时间增长，而且不是线性的。一天的分支能干净 rebase；一周的分支要
去和"假设它不存在"的那些改动做调和，而做这件事的人已经忘了它当初为什么那么写。
如果一个改动大到两天内落不完，就拆开落——用能让每一块自身安全的方式护住它。

`main` 受保护：每个改动都经 PR 进入，需要 code owner 批准，CI 必须通过。这对所有
人一视同仁；维护者不受审查约束只是机制上的例外，而不是惯例上的——**惯例才是重要的
那一半**。

一个 PR 只做一件事。一次要同时装下三个无关改动的评审，在三件事上都会漏掉更多问题。

## 记录待办

值得以后做的事情放进 issue，不要留在对话里。只存在于聊天记录里的决定，对下一个人
和你自己都是不可见的，于是它会被重新做一遍——而且做得不一样。

## 从哪里入手

[路线图](docs/ROADMAP.zh-CN.md)说明每个里程碑解锁什么能力，
[实施方案](docs/IMPLEMENTATION.zh-CN.md)把它拆成带完成标准的任务。当前最有用的
贡献是：带复现 seed 的精确 bug 报告、交易所适配器、描述通用失败模式的仿真场景、
以及文档。

社区支持为 best-effort，2.0 之前不承诺 SLA。开新 issue 前请先搜索已有 issue。
