# Git 工作流规范

## 分支模型

| 分支 | 用途 | 保护规则（rulesets 已强制） |
|---|---|---|
| `main` | 稳定发布线，始终可构建可发布 | 必须通过 CI；**只接受其他分支的 PR**；禁止 force push |
| `dev` | 开发主线，功能在此集成 | 合入 PR 必须通过 CI；禁止 force push（允许直接 push） |
| 工作分支 | 功能/修复开发，生命周期短，合入后删除 | 无 |

## 分支命名

从 `dev` 切出，格式 `<type>/<短描述>`，短描述用小写 kebab-case：

```
feature/websocket-server
fix/login-timeout
docs/api-reference
refactor/db-layer
chore/upgrade-deps
test/clippy-fixes
```

`type` 与提交类型一致（见下文），常用：`feature`、`fix`、`docs`、`refactor`、`test`、`chore`。

## 工作流

```
工作分支 ──PR──> dev ──PR──> main ──tag──> 发布
```

1. `git switch dev && git switch -c feature/xxx` 切出工作分支；
2. 开发并提交（遵守提交规范），保持小步提交；
3. 向 `dev` 发起 PR，CI（rustfmt + clippy + test）全绿后合并，推荐 **squash merge** 保持 dev 历史线性；
4. 发版时向 `main` 发起 `dev → main` 的 PR，CI 全绿后合并，并在 `main` 上打 tag；
5. 已合入的工作分支及时删除，长期分支只有 `main` 和 `dev`。

## 提交信息（Conventional Commits 1.0.0）

格式：

```
<type>(<scope>): <subject>

[body]

[footer]
```

- **type（必填，英文小写）**：

  | type | 含义 |
  |---|---|
  | `feat` | 新功能 |
  | `fix` | 缺陷修复 |
  | `docs` | 文档变更 |
  | `style` | 不影响代码含义的调整（格式化等） |
  | `refactor` | 重构（既非新增也非修复） |
  | `perf` | 性能优化 |
  | `test` | 测试相关 |
  | `build` | 构建系统/依赖变更 |
  | `ci` | CI 配置变更 |
  | `chore` | 其他杂项 |
  | `revert` | 回滚提交 |

- **scope（可选）**：影响范围，如 `feat(api):`、`fix(auth):`；
- **subject**：祈使语气、结尾不加句号、一行不超过 72 字符，中英文均可；
- **body（可选）**：说明动机、思路与影响面；
- **footer（可选）**：`BREAKING CHANGE: <说明>` 或 `Closes #123` / `Refs #123`。

示例：

```
feat(signin): 支持多班级并行签到

fix(socket): 断线后未重连导致状态丢失

docs: 补充 linter 严格档与 git 工作流规范

refactor(core)!: 移除同步阻塞接口

BREAKING CHANGE: `run()` 现在返回 `Result<(), Error>`
```

## 提交纪律

1. **一次提交只做一件事**：功能、重构、格式化不混在一起；
2. 提交前本地必须全绿：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`；
3. 不提交生成物与本地环境：`/target`、`/old` 已由 `.gitignore` 排除，新增忽略项先入 `.gitignore`；
4. 不提交敏感信息（密钥、token、cookie），误提交立即视为泄露并轮换。

## PR 规范

- 标题遵循 Conventional Commits（与 squash 后的提交信息一致）；
- 描述包含三要素：**改了什么、为什么、怎么验证的**；
- 新功能必须携带测试（TDD）：先写失败测试，再实现，最后全绿；
- 重构不得改变行为，必须有既有测试覆盖佐证；
- 合并前自查一遍完整 diff；单人项目无强制审批，但 CI 必须绿。

## 版本与发布

- 版本号遵循 [SemVer 2.0.0](https://semver.org/)，与 `Cargo.toml` 的 `version` 保持一致；
- tag 仅打在 `main`：`git tag -a vX.Y.Z -m "release: vX.Y.Z" && git push origin vX.Y.Z`；
- 破坏性变更升级主版本号，新功能升次版本号，修复升修订号；
- **变更日志全自动**（git-cliff，配置见 `cliff.toml`）：
  - 推送 tag 后，`release.yml` 自动构建各平台二进制创建 GitHub Release，
    说明取自 git-cliff 按本次 tag 生成的变更段落；
  - 合并到 `main` / 推送 tag 后，`changelog.yml` 自动更新 `CHANGELOG.md`
    并创建 PR，审阅合并即可，无需手写变更日志。

## 禁止事项（rulesets 强制，勿尝试绕过）

- 对 `main` / `dev` 执行 force push；
- 绕过 PR 直接向 `main` 推送；
- 在 CI 未通过的情况下请求合并。
