# 地图：repo-wiki v2 — 从审计到完整实现

## 目的地

将 repo-wiki 从存在 12 个 P0 bug 的状态演进为可生产的代码知识系统：P0/P1 bugs 清零、搜索模块全接入 pipeline、架构债务消除。

验收标准：
- `cargo test` 全部通过（当前 111，目标 ≥130）
- `cargo clippy` 零警告
- `repo-wiki generate --output /tmp/wiki` 使用自定义输出路径
- `repo-wiki update` 无变更时不清空输出
- `repo-wiki search --query "parse" --json` 返回结构化结果
- 搜索索引为 SQLite FTS5，非 bincode
- OpenCode 插件 `execSync` 有 `timeout`

## Notes

- rusqlite bundled 已确认可用，Cargo.toml 已有
- SQLite FTS5 已实现（store.rs + text.rs），非"未来迁移"
- SemanticEngine 已接入 pipeline（lib.rs:213-227）
- 审计发现 12 P0（其中 7 个已修复/非问题，5 个仍存在）
- AstQuery/SearchAgent/CallGraph 已实现但未接入 pipeline

## 决策记录

- [P0-1 搜索索引 bincode→SQLite FTS5](tickets/P1-search-pipeline-integration.md) — store.rs + text.rs 完成，Cargo.toml 有 rusqlite bundled。已完成。
- [P0-2 name_map 覆盖修复](tickets/P2-incremental-index-update.md) — graph.rs:190 `HashMap<String, Vec<NodeId>>`，不覆盖。已完成。
- [P0-3 impact.rs module_path panic 修复](tickets/P3-CLI-search-command.md) — 两处 `!neighbor_node.module_path.is_empty()` 保护。已完成。
- [P0-4 chunk.rs NodeId→file_path 映射修复](tickets/P3-CLI-search-command.md) — `build_node_to_file_map` 函数。已完成。
- [P0-5 Custom provider 修复](tickets/P5-install-to-opencode.md) — generate/mod.rs:44-46 有单独分支。已完成。

## 未修复的 P0（当期 Ticket）

1. **P0-A cards/chunks 索引对齐** — generate/mod.rs:89-90 Vec 索引错位。1 文件修改。
2. **P0-B --output 参数忽略** — main.rs:88 `output: _`。2 文件修改。
3. **P0-C impact.rs 子串匹配** — impact.rs:29 `fp.contains(cfp)`。1 行修改。
4. **P0-D 增量更新清空输出** — lib.rs:104-117 `Vec::new()`。1 文件修改。
5. **P0-E OpenCode 插件无 timeout** — repo-wiki.ts:17。1 行修改。

## 未接入 Pipeline（当期 Ticket）

6. **P1 search::agent::SearchAgent** — 已实现 155 行但未调用
7. **P1 search::ast::AstQuery** — 已实现 201 行但未调用
8. **P1 search::callgraph::CallGraph** — 已实现 103 行但未调用

## 不包含

- 定义跳转/go-to-definition — LSP Server 已覆盖
- GitHub/GitLab 自动同步 — 独立功能
- PDF 导出 — 非知识系统必需
- 上游文档索引 — 效用/成本比低
- CI 配置 — 暂不涉及仓库基础设施
