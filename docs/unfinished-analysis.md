# OpenCode Repo Wiki — 未完成项综合分析报告

> 基于代码深度检索（49 .rs 文件，7,902 行，127 测试）和计划文件对比。
> 报告日期：2026-07-31

---

## 结论先行

**该计划（v4）已经实现约 90%。** 主要功能骨架已完成：

| 计划项 | 状态 | 证据 |
|--------|------|------|
| Cargo.toml 加 rusqlite | ✅ | `line 12: rusqlite bundled` |
| config/schema.rs SearchSection | ✅ | `line 172-197: rrf_k, default_engine` |
| search/text.rs FTS5 | ✅ | 6 测试通过，SQLite FTS5 完整实现 |
| search/semantic.rs 增强 | ✅ | 向量 SQLite BLOB 存储 |
| lib.rs pipeline 搜索集成 | ✅ | Phase 5 + 增量的 `update_search_index_incremental` |
| main.rs CLI search | ✅ | JSON + 表格输出，3 引擎类型 |
| incremental 增量索引同步 | ✅ | `update_search_index_incremental` 含 delete+reindex |
| commands.rs 安装卸载 | ✅ | 已通过 `mod commands;` 挂载 |

**真正的未完成项是 14 项质量/工程问题**（3 P0 + 6 P1 + 5 P2），详见下文。

---

## P0 — 必须修（已造成实际的 bug 或缺失）

### 1. `build_source_map` 重复读盘

**位置**: `src/lib.rs:322-327`
**当前**：
```rust
fn build_source_map(file_insights: &[FileInsight]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for insight in file_insights {
        if let Ok(source) = std::fs::read_to_string(&insight.path) {  // 重读磁盘！
            map.insert(insight.path.to_string_lossy().to_string(), source);
        }
    }
    map
}
```
**根因**: 每个文件在 `ingest/mod.rs:27` 已被 `read_to_string` 读过，`FileInsight` 的 `source: String` 字段已存在，但 `build_source_map` 不取用。
**修复**: 改为 `map.insert(insight.path.to_string_lossy().to_string(), insight.source.clone());`
**影响**: 全量扫描时每个文件读两次磁盘。1000 文件 = 2000 次 I/O。

### 2. `semantic.rs` 三个独立 tokio Runtime

**位置**: `src/search/semantic.rs:37, 62, 82`
**当前**：`index()`、`index_batch()`、`search()` 各自创建 `Runtime::new()`
**根因**: 不知或未使用 `lib.rs:36` 的 `get_global_runtime()`
**修复**: 在 `SemanticEngine` 中缓存对外部的 `&'static Arc<Runtime>` 引用
**影响**: 语义搜索每次查询/索引都部署一个新线程池

### 3. 无 README.md

**位置**: 项目根
**状态**: `Test-Path README.md` → False
**影响**: 零项目入口文档，用户无法了解用途

---

## P1 — 应该修（质量/完整性不足）

### 4. SearchAgent 未接入 pipeline

**位置**: `src/lib.rs:363-419`（`execute_search`）
**当前**: 直接调用 `TextEngine.search()` / `SemanticEngine.search()`，然后手动 RRF 合并
**已在但未用**: `src/search/agent.rs` 的 `SearchAgent` 实现了三层自动回溯（FTS5 → 语义 → AST）
**修复**: `execute_search` 改用 `SearchAgent` 实例，若 config 的搜索引擎不是 hybrid 则直接转发
**影响**: CLI `search` 命令没有自动回溯能力

### 5. 无性能基准

**位置**: `benches/` 不存在
**状态**: 无 `criterion` 或 `devian` 依赖，无任何 benchmark
**影响**: 无法量化功能改动的性能影响

### 6. `wiki_generate` 插件参数错误

**位置**: `.opencode/plugins/repo-wiki.ts:128`
**当前**：
```typescript
await execa("repo-wiki", ["generate", "-o", ".repo-wiki", output], ...)
```
**问题**: `output` 是 args 中的 output 字段（可能为空字符串），被当成第四个参数传入
**同时 `-o` 和 `output` 参数并存**：CLI 的 `Generate` 命令的 `--output` 字段因为有 `_` 前缀而实际被忽略（`main.rs:91`）
**影响**: 插件调用 generate 时可能因空参数产生意外行为

### 7. `module_info` 调用全量 generate

**位置**: `.opencode/plugins/repo-wiki.ts:141`
**当前**: `module_info` 每次执行都运行 `repo-wiki generate`
**问题**: 全量生成耗时数分钟，`module_info` 本质是只读查询
**修复**: 改为搜索本地卡文件或读缓存

### 8. 影响传播的 module_path[0] 降级

**位置**: `src/incremental/impact.rs:49,80`
**当前**: `affected.insert(neighbor_node.module_path[0].clone())`
**问题**: 只用 module_path 的第一段（顶层模块），多层嵌套的子模块信息丢失
**例**: `module_path: vec!["core", "io", "tcp"]` → 只返回 `"core"`，丢失 `"core::io::tcp"` 级别
**影响**: 增量更新无法准确重建深层模块的文档

### 9. `commands.rs` post-merge hook 安装/卸载不一致

**位置**: `src/commands.rs:25-35`（install 只装 post-commit）, `line 59`（uninstall 同时删 post-commit + post-merge）
**当前**: install 只创建 post-commit，但 uninstall 试图删除 post-commit + post-merge
**修复**: install 也添加 post-merge，或 uninstall 只删除 post-commit

---

## P2 — 值得修（工程质量）

### 10. SemanticEngine 全量向量加载 O(n)

**位置**: `src/search/semantic.rs:77`
**当前**: `load_all_vectors()` 加载 SQLite 全部向量到内存，执行全量余弦相似度
**影响**: 10K+ 实体时内存 + CPU 双瓶颈（每次搜索都是 O(n) 扫描）

### 11. 无文档测试

**位置**: 全局
**状态**: 零 doc-tests，公共 API 无文档级别测试覆盖

### 12. SearchAgent `rrf_k` 硬编码 60

**位置**: `src/search/agent.rs:19-20`
**当前**: `SearchAgent::new()` 写死 `rrf_k: 60.0`，虽有 `set_rrf_k()` 方法但从未调用
**影响**: config 中的 `search.rrf_k` 配置对 SearchAgent 无效

### 13. OpenCode 插件无进度反馈

**位置**: `.opencode/plugins/repo-wiki.ts:126-131`
**当前**: `wiki_generate` 调 `sendProgress` 但只发 stage:scanning(0) → stage:complete(100)
**问题**: 中间状态完全空白，长生成操作无实际进度
**修复**: 在 CLI 端输出结构化 JSON progress 事件

### 14. `search/text.rs` 中 `query.is_empty()` 短路

**位置**: `src/search/text.rs:40-44`
**当前**:
```rust
if query.is_empty() {
    return Ok(Vec::new());
}
```
**问题**: 空查询静默返回空结果而非报错，调用方可能误以为索引为空

---

## 小结

| 严重度 | 数量 | 最该先修的 |
|--------|:----:|-----------|
| P0 | 3 | `build_source_map` 重复读盘（约 15 行改） |
| P1 | 6 | SearchAgent 接入（约 20 行改） |
| P2 | 5 | SemanticEngine rrf_k 硬编码（约 5 行改） |

**代码质量较好**：无循环依赖，127 测试全通过。未完成项全部是"已写了但没挂载/用错方式"的类型，不是设计缺陷。
