# Ticket: CLI完整化

## Question

**问题 1 — Install/Uninstall 不足**:
- 现状: 只有 `install-to-opencode` / `uninstall-from-opencode`（写入 `.opencode/config.json` + 插件文件）
- 缺失: 创建默认 `.repo-wiki/config.toml`、安装 git hooks、卸载确认、多 Agent 支持
- 方案: 添加 `install` 和 `uninstall` 命令。`install` 做 3 件事：①创建默认配置 ②注册插件 ③安装 git post-commit hook（自动触发 wiki 更新）

**问题 2 — Java 语言缺失**（Spec #8）
- 现状: 6 语言处理器中没有 Java。ParserRegistry 注册了 Rust/JS/TS/Python/Go/C#
- 方案: 添加 `tree-sitter-java` 依赖，实现 `JavaProcessor`（walk 匹配 class/interface/enum/ method/import/package 等节点），注册到 ParserRegistry
- 文件: `Cargo.toml`(新依赖) + `src/ingest/parser/java.rs`(新文件~100行) + `parser/mod.rs`(注册)

**估算**: 安装 ~140 行，Java ~100 行

## Tickets this blocks

无

## Assets

`docs/full-audit-report.md:8-38` — Install/Uninstall 分析
`docs/full-audit-report.md:55` — Java 处理器缺失
