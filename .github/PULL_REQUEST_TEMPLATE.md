<!--
标题格式：<type>(<scope>): <subject>，遵循 docs/GIT.md 的 Conventional Commits
-->

## 改了什么

<!-- 简述改动内容与影响范围 -->

## 为什么

<!-- 动机与背景；关联 issue 用 Closes #123 / Refs #123 -->

## 怎么验证的

<!-- 验证方式：新增/修改了哪些测试，如何复现验证 -->

## 自查清单

- [ ] 提交信息符合 Conventional Commits（见 docs/GIT.md）
- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 无告警
- [ ] `cargo test` 全绿；新功能携带测试（TDD）
- [ ] 无敏感信息、无调试残留（`dbg!`、`println!` 等）
