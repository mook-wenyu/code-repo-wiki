# repo-wiki 代码智能增强计划 v2

## 目的地

将 repo-wiki 从"代码→LLM→文档"单向流水线演进为**持久化搜索优先 + AI Agent 调用**的代码知识平台。开发者通过搜索 Agent 可组合使用 BM25 全文检索、语义向量搜索、AST 感知符号查询和依赖图遍历，索引数据在进程重启后自动恢复。

---

## 设计原则

1. **持久化优先**：所有搜索引擎（TextEngine / SemanticEngine）数据持久化到磁盘，无需每次重建。
2. **零新依赖优先**：当前网络环境不支持下载 sqlite-vec，采用 bincode 序列化实现持久化；SQLite 版本作为可迁移优化。
3. **Agent 优先**：搜索 Agent 自动回溯多引擎，开发者不需要关心底层引擎选择。

---

## 已实现状态

| Phase | 内容 | 文件 | 状态 |
|-------|------|------|------|
| B1 | AST 查询接口 | `src/search/ast.rs` | **已完成** |
| B2 | 调用图增强 | `src/search/callgraph.rs` + `graph.rs` | **已完成** |
| C1 | 全文搜索（bincode 持久化 BM25） | `src/search/text.rs` | **已完成** |
| C2 | 向量搜索（bincode 持久化） | `src/search/semantic.rs` | **已完成** |
| C3 | RRF 混合搜索 | `src/search/hybrid.rs` | **已完成** |
| D1 | AST 感知分块 | `src/search/ast_chunker.rs` | **已完成** |
| D2 | 自动回溯搜索 Agent | `src/search/agent.rs` | **已完成** |
| E2 | D3.js 代码库可视化 | `src/output/map.rs` | **已完成** |
| E2 | Mermaid 路径修复 | `src/output/mermaid.rs` | **已完成** |

---

## 架构总览

```
┌──────────────────────────────────────────┐
│             SearchAgent                   │
│  自动回溯：FTS5 → 语义 → AST 精确定位      │
└────────────┬─────────────────────────────┘
             │ compose
     ┌───────┼───────────────┐
     ▼       ▼               ▼
┌────────┐ ┌────────┐ ┌──────────────┐
│Symbol  │ │Text    │ │Semantic      │
│Engine  │ │Engine  │ │Engine        │
│(AST q) │ │(BM25)  │ │(cosine sim)  │
│(call   │ │(bin    │ │(bin persist) │
│graph)  │ │persist)│ │              │
└───┬────┘ └───┬────┘ └──────┬───────┘
    │          │              │
    └──────────┼──────────────┘
               ▼
      ┌──────────────────┐
      │   output.dir/    │
      │  wiki + .bin 索引│
      └──────────────────┘
```

---

## 待实现

### P1. 搜索索引嵌入到完整 pipeline

**文件**: `src/lib.rs`, `src/analysis/graph.rs`

**当前问题**: 搜索引擎未被集成到 pipeline 主流程中。`lib::run_generation` 完成 build_graph 和 generate 后，不会自动构建搜索索引。

**方案**:
1. `build_graph` 完成后，遍历所有 entity 调用 `text_engine.index()` 和 `semantic_engine.index()`。
2. 搜索索引作为 pipeline 的可选阶段（默认启用，可通过配置关闭）。

**验证**: 全量 pipeline 运行后，搜索索引文件在 `output.dir` 下存在且非空。

---

### P2. 增量索引更新

**文件**: `src/incremental/mod.rs`, `src/lib.rs`

**当前问题**: `apply_incremental` 只影响 KnowledgeGraph 和卡片重新生成，不更新搜索索引。

**方案**:
1. 增量变更时，只对变更文件涉及到的 entity 重新索引（删除旧索引项 + 添加新项）。
2. 从 `ImpactResult` 中提取受影响文件列表，精确重建对应 entity 的索引。

**验证**: 修改一个文件后增量更新，搜索索引中的对应 entity 反映最新内容。

---

### P3. 搜索 CLI 命令

**文件**: `src/cli/main.rs`

**当前问题**: 没有 `search` 子命令，搜索只能通过 Rust API 调用。

**方案**: 添加 `search` 子命令：
```
repo-wiki search --query "auth flow" --top-k 10
```
输出格式：表格化搜索结果（名称 + 种类 + 文件路径 + 相关性评分）。

**验证**: `cargo run -- search -q "auth"` 返回非空结果。

---

### P4. AST 分块索引

**文件**: `src/search/ast_chunker.rs`, `src/search/text.rs`

**当前问题**: TextEngine 索引 entity 层数据，不会按 AST 细粒度分块索引。

**方案**: 新增 `index_ast_chunks` 方法：对每个 entity 的 source_code 调用 `chunk_by_ast`，将每个 AST 块（函数体、类方法等）作为独立文档索引。

**验证**: 索引 AST 分块后，"user login" 等跨函数概念搜索能匹配分块内容。

---

### P5. 一键 `install-to-opencode` CLI 命令

**文件**: `src/cli/main.rs`

**当前问题**: OpenCode 插件不存在。用户无法将 repo-wiki 注册为 opencode 的搜索工具。

**方案**:

1. 不创建 TypeScript MCP 插件（维护成本高 + 无 TypeScript 工具链）。
2. 改为 `install-to-opencode` 命令：生成一个 JSON 配置片段，描述 repo-wiki 作为 opencode 可调用工具。
3. 配置片段包含：
   - `name`: "repo-wiki-search"
   - `command`: `repo-wiki search --query "{input}" --top-k 10`
   - `description`: "在代码仓库中搜索符号、函数和模块"
4. 配置写入到 opencode 的 MCP servers 配置目录。

**验证**: 运行 `install-to-opencode` 后，opencode 可调用 repo-wiki 搜索。

---

### P6. 文件监听 Watch 模式

**文件**: `src/incremental/watch.rs`, `src/cli/main.rs`

**当前问题**: `watch.rs` 已实现文件系统监听，但 CLI 没有 `watch` 命令。

**方案**: `repo-wiki watch` 命令：
1. 启动文件监听（复用已有的 `WatchService`）。
2. 检测到文件变更时，自动执行增量索引更新（P2）。
3. 默认不生成 wiki 文档（仅更新索引）。

**验证**: 在项目目录中修改文件，watch 模式自动触发索引更新。

---

### P7. SQLite FTS5 迁移（网络恢复后）

**文件**: `src/search/text.rs`, `src/search/semantic.rs`, `Cargo.toml`

**当前方案**: bincode 序列化持久化（零新依赖）。

**待迁移方案**: rusqlite + sqlite-vec：
1. `Cargo.toml` 添加 `rusqlite = { version = "0.32", features = ["bundled", "vtab"] }`
2. TextEngine 改为 FTS5 虚拟表（porter unicode61 分词）。
3. SemanticEngine 改为 vec0 虚拟表（1536 维向量）。
4. 对外 API 不变（`open()` / `index()` / `search()` / `clear()`）。

**验证**: FTS5 BM25 搜索结果与 bincode BM25 一致。

---

## 依赖图

```
P1(pipeline集成) ───── 零依赖，可直接开始
│
├─ P2(增量索引) ──── 依赖 P1
├─ P3(CLI search) ─── 依赖 P1
├─ P4(AST分块索引) ── 依赖 D1(已完成)
│
├─ P5(install-opencode) ─ 依赖 P3
├─ P6(watch模式) ──── 依赖 P2 + P3
│
└─ P7(SQLite迁移) ─── 依赖网络恢复
```

**推荐执行顺序**：
```
Day 1: P1(pipeline集成) — 打通全流程
Day 2: P3(CLI search) — 可交互验证
Day 3: P2(增量索引) + P4(AST分块索引) — 并行
Day 4: P6(watch模式) — 自动化
Day 5: P5(install-to-opencode) — 外部集成
Day 7: P7(SQLite迁移) — 网络恢复后执行
```

---

## 技术选型

| 维度 | 当前选择 | 未来升级 |
|------|----------|----------|
| 全文搜索 | **bincode 序列化 BM25** | SQLite FTS5 |
| 向量存储 | **bincode 序列化向量** | sqlite-vec vec0 |
| Embedding API | **OpenAI 兼容 HTTP**（已有） | 不变 |
| AST 查询 | **tree-sitter Query**（已有） | 不变 |
| 调用图 | **petgraph**（已有） | 不变 |
| 持久化 | **bincode**（已有依赖） | rusqlite + sqlite-vec |
| 搜索 Agent | **src/search/**（已完成） | 不变 |
| MCP 集成 | **JSON 配置片段**（无 TypeScript 插件） | 完整 MCP Server(可选) |

---

## 不包含

- 定义跳转/go-to-definition — LSP Server 已覆盖
- GitHub/GitLab 自动同步 — 独立功能
- PDF 导出 — 可独立讨论
- 上游文档索引 — 需依赖爬虫，效用/成本比低
