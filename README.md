# wei-class

微助教签到工具（Rust 重写版）。


[![CI](https://github.com/pjm314159/wei-class/actions/workflows/ci.yml/badge.svg)](https://github.com/pjm314159/wei-class/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

## 项目状态

框架搭建阶段：工程规范与 CI 已就绪，功能实现正在推进，目录结构与接口可能频繁变动。

## 功能规划

- [ ] 身份接入（openid）
- [ ] 签到核心流程
- [ ] 实时通信（WebSocket）
- [ ] Web 服务与前端页面

## 快速开始

环境要求：Rust 1.85+（edition 2024）。

```bash
git clone https://github.com/pjm314159/wei-class.git
cd wei-class
cargo build
cargo run
```

## 目录结构

```
.
├── .github/    # CI 工作流、PR / Issue 模板
├── docs/       # 工程规范文档
├── src/        # 源码（重写中）
└── old/        # 旧版参考实现（不入库，本地保留）
```

## 开发指南

- 分支模型、提交与 PR 规范：[docs/GIT.md](docs/GIT.md)
- Lint 严格档与豁免规则：[docs/LINTER.md](docs/LINTER.md)

提交前本地全绿：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

典型流程：从 `dev` 切出工作分支 → PR 到 `dev`（CI 通过后合并）→ 发版时 `dev` → `main` 并打 tag。

## 反馈

缺陷请提 [Bug 报告](https://github.com/pjm314159/wei-class/issues/new?template=bug_report.yml)，建议请提[功能建议](https://github.com/pjm314159/wei-class/issues/new?template=feature_request.yml)。

## 许可证

本项目基于 [GPL-3.0](LICENSE) 协议发布。
