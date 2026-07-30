# repo-wiki 全面审计报告

> 基于 5 个并行子代理的全面分析（Install/Uninstall、Spec Gap、Code Audit、架构深度、Web Research）
>
> 审计日期：2026-07-31

---

## 一、执行摘要

| 维度 | 评分 | 趋势 |
|------|------|------|
| 总体实现度（vs Spec） | 43/70 = **61%** | — |
| Install/Uninstall 成熟度 | **2/10** | 🔴 紧急 |
| 架构健康度 | ⭐⭐⭐⭐⭐ | ✅ 优秀 |
| 代码质量 | ⭐⭐⭐⭐ | ✅ 良好 |
| 测试覆盖 | ⭐⭐⭐ | ⚠️ 中等（126 测试，无 bench） |
| 死代码 | ⭐⭐ | 🔴 多块死代码未清理 |
| 文档 | ⭐⭐ | ⚠️ 无 README，STATUS.md 计数错误 |

**最紧急行动**：

1. 挂载 `commands.rs`（Install/Uninstall 从 2/10 → 8/10）
2. 注册 Java 解析器（173 行死代码变活代码）
3. 重写 STATUS.md（测试计数 134→126）
4. 清理 `sqlite-vec` + `async-openai` 未用依赖
5. 写 README.md

---

## 二、Install/Uninstall 深度分析（Agent 1）

> **评分：2/10**

### 根因：`commands.rs` 是死代码

`src/commands.rs`（74 行）包含完整的 install/uninstall 逻辑（创建默认 config.toml、安装 git hooks、卸载确认），但**从未被任何模块声明**：

```rust
// 在 main.rs 或 lib.rs 中 —— 不存在
mod commands;    // ← 这一行缺失
```

`main.rs` 的 InstallToOpencode 直接调 `OpenCodeConfig::install_plugin()`，只做了 opencode.json 的 plugins 注册。

### 差距矩阵

| 功能 | 当前 | 差距 |
|------|------|------|
| 写入 opencode.json plugins | ✅ | — |
| 创建默认 .repo-wiki/config.toml | ❌ | 需补 ~20 行 |
| 安装 git post-commit hook | ❌ | 需补 ~15 行 |
| 安装 git post-merge hook | ❌ | 需补 ~10 行 |
| 环境验证（git 可用、git 仓库） | ❌ | 需补 ~15 行 |
| 卸载时移除 git hooks | ❌ | 需补 ~15 行 |
| 卸载确认（--force） | ❌ | 需补 ~10 行 |
| 卸载时可选清理数据 | ❌ | 需补 ~15 行 |
| 错误回滚/原子性 | ❌ | 可暂时忽略 |

### 修复方案

**方案 A（推荐，~50 行）：** 在 `main.rs` 加 `mod commands;`，然后将 InstallToOpencode/UninstallFromOpencode 逻辑改为调 `commands::install("opencode")` 和 `commands::uninstall(force_flag)`。

**方案 B（~80 行）：** 将 install_plugin()/uninstall_plugin() 扩展为包含全部功能，删除 commands.rs。

---

## 三、Spec 差距分析（Agent 2）

### 按 Phase 评分

| Phase | 评分 | 实现度 | 关键缺失 |
|-------|------|--------|----------|
| Phase 1: CLI+Config+Scanner+Parser | 7/10 | **70%** | 缺 rayon 并行解析 |
| Phase 2: Knowledge Graph | 6/10 | **60%** | 缺 Leiden 算法、图持久化、call_edges 精确匹配 |
| Phase 3: LLM Generation | 8/10 | **80%** | 缺 rayon，其余完整 |
| Phase 4: Output | 7/10 | **70%** | 缺独立类图/调用图渲染 |
| Phase 5: Incremental | 8/10 | **80%** | 缺一致性验证 |
| Phase 6: OpenCode Plugin | 0/10 | **0%** | 无 MCP 工具、无 Slash 命令、无进度反馈 |
| Phase 7: Testing | 7/10 | **70%** | 缺性能基准 |
| **总体** | **43/70** | **61%** | — |

### 最严重缺失（Top 5）

1. **Phase 6（0%）** — 项目自称 OpenCode 插件但未暴露任何 MCP tool（wiki_search、wiki_generate 等）或 slash 命令
2. **Phase 2 — Leiden algorithm** — 当前用目录前缀 + 内聚度阈值做模块聚类，不是 Leiden/Louvain
3. **Phase 2 — 图持久化** — KnowledgeGraph 完全在内存重建，无 save/load
4. **Phase 7 — 性能基准** — 无 criterion/devian bench，无法量化性能影响
5. **边界条件 — 超大仓库** — 无分批处理，全部在内存中

### 超额交付（Spec 未要求但已实现）

- ✅ 完整三引擎搜索（FTS5 + Semantic + RRF Hybrid）
- ✅ SearchAgent 自动回溯搜索
- ✅ CallGraph 调用者/被调用者查询
- ✅ AstQuery 符号精确定位
- ✅ AstChunker AST 感知代码分块
- ✅ 文件系统监听（notify-debouncer-full）
- ✅ Embedding 引擎
- ✅ HTML 导出（pulldown-cmark + Mermaid CDN）
- ✅ 交叉引用断链检测
- ✅ Mock LLM Provider
- ✅ OpenCode 插件管理 CLI

---

## 四、代码审计（Agent 3 + Agent 4）

> 7,902 行 / 48 .rs 文件 / 126 测试全通过

### 架构健康度

```
src/
├── config/    578 行  ✅ 加载 + schema
├── model/     253 行  ✅ CodeNode/CodeEdge/KnowledgeCard
├── ingest/    139 行  ✅ 扫描 + 解析入口
│   └── parser/ 1423 行 ✅ 7 种语言处理器（含死 Java）
├── analysis/  914 行  ✅ 图构建 + 模块检测
├── generate/  1862 行 ✅ LLM + chunk + card + wiki + embed
├── output/    1154 行 ✅ Markdown/HTML/Mermaid/CrossRef
├── incremental/ 1006 行 ✅ diff + impact + state + watch
├── search/    1333 行 ✅ 9 文件全面搜索引擎
├── lib.rs      412 行 ✅ Pipeline 编排
├── main.rs     177 行 ✅ CLI（9 子命令）
└── commands.rs  74 行 ❌ 死代码（未声明 mod）
```

### 搜索模块覆盖度

| 文件 | 行数 | pipeline 使用 | 说明 |
|------|------|:---:|------|
| `store.rs` | 280 | ✅ | SQLite FTS5 + 向量 BLOB 存储层 |
| `text.rs` | 136 | ✅ | TextEngine，委托 SearchStore |
| `semantic.rs` | 135 | ✅ | 但 load_all_vectors 全量内存加载 |
| `hybrid.rs` | 89 | ✅ | RRF 合并，k=60 硬编码 |
| `ast.rs` | 169 | ⚠️ 半活 | agent.rs 引用但 agent 死代码 |
| `ast_chunker.rs` | 146 | ❌ 死 | 仅测试覆盖 |
| `callgraph.rs` | 90 | ❌ 死 | 仅测试覆盖 |
| `agent.rs` | 124 | ❌ 死 | SearchAgent 未集成到 pipeline |
| `mod.rs` | 8 | ✅ | — |

### 问题清单

#### P0 — 必须修（4 项）

| # | 问题 | 位置 | 说明 |
|---|------|------|------|
| 1 | commands.rs 死代码 | `src/commands.rs` | 完整 install 逻辑从未挂载 |
| 2 | Java 解析器未注册 | `parser/mod.rs:65-70` | 173 行完整代码不可用 |
| 3 | 无 README.md | 项目根 | 零入口文档 |
| 4 | FileInsight 不缓存 source | `ingest/parser/mod.rs:13` | 搜索索引构建时重读磁盘 |

#### P1 — 应该修（9 项）

| # | 问题 | 位置 | 说明 |
|---|------|------|------|
| 5 | STATUS.md 测试计数 | `STATUS.md:20` | 134→126 |
| 6 | sqlite-vec 未用 | `Cargo.toml` | 手工 BLOB 编解码替代了它 |
| 7 | async-openai 未用 | `Cargo.toml` | HTTP POST 直接调用代替了 SDK |
| 8 | rrf_k 硬编码 60 | `agent.rs:37`, `lib.rs:408` | 应入 SearchSection 配置 |
| 9 | impact max_depth 硬编码 3 | `impact.rs:38` | 应入 IncrementalSection 配置 |
| 10 | diff repo_path 硬编码 "." | `diff.rs:28` | `Repository::open(".")` 不可定制 |
| 11 | 无性能基准 | `benches/` 缺失 | criterion bench 完全缺失 |
| 12 | 无文档测试 | 全局 | 零 doc-test |
| 13 | uninlined_format_args | `main.rs:130` | clippy pedantic 警告 |

#### P2 — 值得修（8 项）

| # | 问题 | 位置 | 说明 |
|---|------|------|------|
| 14 | SemanticEngine 全量加载 | `semantic.rs:77` | load_all_vectors O(n) |
| 15 | module_fingerprints 永远空 | `state.rs:68` | 声明了 never populated |
| 16 | HTML <cite> 在 .md 中 | `crossref.rs:94` | 纯 Markdown 输出含 HTML |
| 17 | 5 个独立 tokio Runtime | 全局 | lib.rs:59, lib.rs:123 + semantic.rs x3 |
| 18 | SearchAgent 未集成 | `lib.rs` | execute_search bypass agent |
| 19 | pulldown-cmark Options::all() | `html.rs:195` | 过宽松 |
| 20 | crossref substring 匹配 | `crossref.rs:55` | contains() 误报 |
| 21 | module_path[0] 降级 | `impact.rs:81` | 损失子模块信息 |

---

## 五、统一行动计划

### 4 个 Phase，按优先级排列

#### Phase 0 — 快速胜利（6 项，总计 ~50 行，0 风险）

| 项 | 文件 | 描述 | 行数 |
|----|------|------|------|
| P0.1 | `parser/mod.rs:65` | 加 `reg.register(Box::new(java::...))` | 1 |
| P0.2 | `Cargo.toml` | 删 sqlite-vec + async-openai | 2 |
| P0.3 | `STATUS.md` | 134→126 | 1 |
| P0.4 | `main.rs:130` | fix uninlined_format_args | 1 |
| P0.5 | `parser/mod.rs:13` | FileInsight 加 source: String | 2+17 |
| P0.6 | 根目录 | 写 README.md | ~30 |

#### Phase 1 — 架构修整（5 项，~200 行，低风险）

| 项 | 文件 | 描述 | 行数 |
|----|------|------|------|
| P1.1 | `main.rs` + `commands.rs` | 挂载 commands.rs | 3 |
| P1.2 | `lib.rs` | OnceLock<Arc<Runtime>> 缓存 | 12 |
| P1.3 | `lib.rs` | execute_search → SearchAgent | 20 |
| P1.4 | `config/schema.rs` | rrf_k, max_depth 参数化 | 10 |
| P1.5 | `model/mod.rs` | KnowledgeGraph::save/load | 30 |

#### Phase 2 — Spec 合规（4 项，~400 行，中风险）

| 项 | 文件 | 描述 | 行数 |
|----|------|------|------|
| P2.1 | `llm.rs` | complete_stream SSE 实现 | 80 |
| P2.2 | `generate/mod.rs` | Level 0 实体摘要 | 60 |
| P2.3 | `crossref.rs` | <cite> 渲染 | 20 |
| P2.4 | `mermaid.rs` | 循环标注 | 30 |

#### Phase 3 — 测试 + 文档（3 项，~200 行，低风险）

| 项 | 文件 | 描述 | 行数 |
|----|------|------|------|
| P3.1 | `benches/` | criterion benchmark | 100 |
| P3.2 | `tests/` | E2E 测试 + 快照增强 | 80 |
| P3.3 | `docs/` | doc-test（关键模块） | ~50 |

### 执行顺序

```
Week 1: Phase 0（快速胜利，50 行改）
         └─ P0.6 (README) 可并行
Week 2: Phase 1（架构修整，200 行改）
         └─ P1.1 (commands 挂载) 1 行，立刻见效
         └─ P1.2-P1.5 可并行
Week 3: Phase 2（Spec 合规，400 行改）
         └─ P2.1 (complete_stream) 依赖 P1.2 (Runtime)
         └─ P2.2-P2.4 可并行
Week 4: Phase 3（测试 + 文档，200 行改）
         └─ 可并行
```

---

## 六、验证标准

每个 Phase 完成后必须通过：

1. `cargo check` — 0 errors
2. `cargo clippy -D warnings` — 0 warnings
3. `cargo test` — 所有测试通过（含新增测试）
4. STATUS.md 更新

### 防回归

- 搜索测试（21 个）：确保 FTS5/Semantic/Hybrid/AST/SearchAgent 行为不退化
- 增量测试（17 个）：确保 diff/impact/state/watch 不退化
- 生成测试（15 个）：确保 LLM/chunk/card/wiki/embed 不退化
- 输出测试（20 个）：确保 html/md/mermaid/crossref 不退化

---

## 七、结论

repo-wiki 有一个**非常清晰的架构基础**（7 层无循环依赖、完整搜索引擎套件、增量更新、Mock 测试模式），但当前的**主要债务是死代码和不完整性**：

- `commands.rs` 和 Java parser 证明了"写了但没挂载"的遗留问题
- Phase 6（0%）意味着 OpenCode 插件基本是占位符
- 无 README 意味着新用户无法了解项目

建议优先 Phase 0，用最小成本消除最痛的点。
