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

1. 登录：输入 openid **或包含 openid 参数的 URL**（自动提取，32 位十六进制校验）→ 成功进入监听视图；失败时显示对应原因
2. 收码：教师开启二维码签到后，页面显示二维码与"点击直接跳转"链接
3. 自动跳转：勾选"检测成功后自动跳转"后，检测到签到即跳转（微信网页中可用）
4. 关闭：签到关闭时提示"本轮签到已关闭"，二维码清空
5. 改间隔：修改轮询间隔即时上报；低于服务端下限的值被上调并提示
6. 失效：openid 失效时收到 `sessionExpired` → 自动清除 cookie 并回到登录态
7. 其他：`/usage` 使用说明页、右上角深浅色主题切换、favicon

前端门禁：`npx @biomejs/biome ci static/`（依赖已本地化到 `static/vendor/`，无 CDN）。

## 部署

提供两种方式：

- **本地部署（免 nginx，推荐个人使用）**：下载预编译二进制直接运行，浏览器访问 `http://127.0.0.1:8080`
- **Docker Compose + nginx（推荐公网部署）**：`docker compose up -d`，见 [docs/DEPLOY.md](docs/DEPLOY.md)

### 本地部署

1. 到 [Releases](https://github.com/pjm314159/wei-class/releases) 下载对应平台压缩包并解压
   （Windows：`wei-class-x86_64-windows.zip`；Linux：`wei-class-x86_64-linux.tar.gz`；
   也可自行 `cargo build --release`）
2. （可选）将 `.env.example` 复制为 `.env` 按需修改
3. 运行 `wei-class`（Windows 直接双击或命令行执行；Linux `./wei-class`）
4. 浏览器访问 <http://127.0.0.1:8080>

说明：

- 默认仅监听 `127.0.0.1:8080`；需要局域网访问时改 `.env` 中的 `LISTEN_ADDR=0.0.0.0:8080`
- 前端页面与依赖已内置在二进制中，无需额外文件；日志按日写入 `LOG_DIR`（默认 `logs/`）
- 停止服务：终端 `Ctrl+C`（Windows 关闭窗口即可）
- 升级：下载新版本替换二进制即可（零持久化，无需迁移数据）

公网部署的完整前置检查、构建与冒烟步骤见 [docs/DEPLOY.md](docs/DEPLOY.md)。

## 反馈

缺陷请提 [Bug 报告](https://github.com/pjm314159/wei-class/issues/new?template=bug_report.yml)，建议请提[功能建议](https://github.com/pjm314159/wei-class/issues/new?template=feature_request.yml)。

## 许可证

本项目基于 [GPL-3.0](LICENSE) 协议发布。
