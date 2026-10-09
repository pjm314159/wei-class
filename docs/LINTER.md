# Linter 规范（严格档）

本项目所有代码统一按**严格档**执行 lint 与格式化，由 CI（`.github/workflows/ci.yml`）强制把关：任何一项不通过，PR 无法合入 `main` / `dev`。

## 工具链

| 工具 | 作用 | 配置来源 |
|---|---|---|
| rustfmt | 代码格式化 | 官方默认配置（不自定义） |
| clippy | 静态检查 | `Cargo.toml` 的 `[lints]` 表 |

## 强制命令

本地开发、提交前必须全绿：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

自动修复格式：

```bash
cargo fmt --all
```

## 严格档定义

### 级别语义

| 级别 | 含义 |
|---|---|
| `forbid` | 禁止，且**不允许任何 `#[allow]` 覆盖** |
| `deny` | 禁止，默认不可通过（改动需走评审讨论） |
| `warn` | 警告；CI 中经 `-D warnings` **升级为 error**，效果等同强制 |

### rust 层（`[lints.rust]`）

| Lint | 级别 | 说明 |
|---|---|---|
| `unsafe_code` | `forbid` | 全面禁用 unsafe，不可豁免 |
| `missing_docs` | `warn` | 公共 API 必须有文档注释 |

### clippy 层（`[lints.clippy]`）

| Lint | 级别 | 说明 |
|---|---|---|
| `pedantic` | `warn` | pedantic 全组启用，经 CI `-D warnings` 强制 |
| `unwrap_used` | `deny` | 禁止 `unwrap()`（潜在 panic） |
| `expect_used` | `deny` | 禁止 `expect()`（潜在 panic） |
| `panic` | `deny` | 禁止显式 `panic!` |
| `todo` | `deny` | 禁止 `todo!` 占位符进入主线 |
| `unimplemented` | `deny` | 禁止 `unimplemented!` 占位符 |
| `dbg_macro` | `deny` | 禁止 `dbg!` 调试残留 |
| `print_stdout` | `deny` | 禁止裸打印到 stdout，输出统一走日志设施（后续引入 `tracing`） |
| `print_stderr` | `deny` | 禁止裸打印到 stderr，错误统一走日志设施 |

> 说明：`pedantic` 组内与上述 `deny` 项冲突时，`deny` 优先级更高（`pedantic` 使用 `priority = -1`）。
> 暂不启用 `nursery` 组：其规则不稳定且误报率高，待项目稳定后再评估单项引入。

## 豁免规则

严格档默认**不允许豁免**。确有必要的，必须满足：

1. `#[allow(...)]` 的作用范围**最小化**——只作用于单个条目/语句，禁止模块级或文件级豁免；
2. 紧邻 allow 处必须写注释说明**为什么**不得不豁免，以及期望的移除时机；
3. 不允许在 `Cargo.toml` 中全局调低任何 `deny` 项；
4. `unsafe_code` 为 `forbid`，任何情况下不可豁免。

示例：

```rust
// 解析第三方返回的固定格式，字段恒存在；待上游增加 Err 分支后移除
#[allow(clippy::single_match_else)]
fn parse_fixed(input: &str) -> u8 {
    ...
}
```

## 错误处理约定

- 一切可能失败的操作返回 `Result`，禁止用 panic 表达错误；
- 错误类型后续统一（引入 `thiserror` / `anyhow` 时在本文件补充章节）；
- 需要中断程序时使用受控退出（如 `main` 返回 `Result`），而非 `panic!`。

## 前端门禁

前端（`static/`）不引入构建链，用 **Biome** 单二进制做 lint + 格式，与 cargo fmt/clippy 对等：

```bash
npx --yes @biomejs/biome@2.3.4 check static/   # 本地：发现问题
npx --yes @biomejs/biome@2.3.4 ci static/      # CI：同 check，面向 CI 输出
```

豁免规则与 Rust 侧一致：范围最小化 + 紧邻注释说明理由。当前两处豁免：
`document.cookie` 直接赋值（需清除服务端签发的非 HttpOnly cookie）、`x-cloak` 的 `display: none`。

JS 纯函数单测走 Node 内置 runner（`node --test "static/**/*.test.js"`，需传 glob 而非目录：Node 24 会把目录参数当作待执行文件）；按哑渲染原则用例接近为零，CI 中保留步骤、无用例时跳过。

## CI 强制项

`.github/workflows/ci.yml` 中 check 名为 `test` 的任务依次执行：

1. `cargo fmt --all -- --check`（格式检查）
2. `cargo clippy --all-targets --all-features -- -D warnings`（严格档 lint）
3. `cargo test --verbose`（测试）
4. `npx @biomejs/biome ci static/`（前端 lint + 格式）
5. `node --test static/`（前端纯函数单测，无用例时跳过）

全部通过，分支保护规则（rulesets）才允许合入。
