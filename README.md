# wei-class

微助教签到工具（Rust 重写版）。


[![CI](https://github.com/pjm314159/wei-class/actions/workflows/ci.yml/badge.svg)](https://github.com/pjm314159/wei-class/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

## 项目状态

核心链路已贯通（配置 → faye 单例 → 微助教 API → axum 服务 → 签到轮询 → 前端页面），部署与 CI 增量推进中，接口仍可能微调。

## 功能规划

- [x] 身份接入（openid）
- [x] 签到核心流程
- [x] 实时通信（WebSocket）
- [x] Web 服务与前端页面

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
├── static/     # 前端单文件静态页（Alpine.js，零构建）
├── Dockerfile  # cargo-chef 多阶段 → musl 静态二进制 → distroless
├── docker-compose.yml / nginx.conf  # 部署编排与反向代理
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

## 前端冒烟清单

启动服务（`cargo run`）后访问 `http://127.0.0.1:8080/`，依次确认：

1. 登录：填入 openid → 成功进入主界面；失败时显示对应原因（无效 / 上游不可用 / 格式不合法）
2. 收码：教师开启二维码签到后，页面显示二维码与可跳转链接
3. 换轮：下一轮推送到达时二维码与"更新于"时间同步刷新
4. 关闭：签到关闭时提示"本轮签到已关闭"，二维码清空
5. 改间隔：修改间隔即时上报；低于服务端下限的值被上调并提示
6. 失效：openid 失效时收到 `sessionExpired` → 自动清除 cookie 并回到登录态

前端门禁：`npx @biomejs/biome check static/`（Alpine.js 与二维码库经 CDN 引入，内网部署需自建静态资源）。

## 部署

`docker compose up -d` 即可（app 仅内网 + nginx 443 终止 TLS，证书 volume 挂载）。
完整前置检查、构建与冒烟步骤见 [docs/DEPLOY.md](docs/DEPLOY.md)。

## 反馈

缺陷请提 [Bug 报告](https://github.com/pjm314159/wei-class/issues/new?template=bug_report.yml)，建议请提[功能建议](https://github.com/pjm314159/wei-class/issues/new?template=feature_request.yml)。

## 许可证

本项目基于 [GPL-3.0](LICENSE) 协议发布。
