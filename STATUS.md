# 项目状态简报 （AI自动维护）

## 一、架构健康度
- 当前模块总数：11（config/model/ingest/analysis/generate/output/incremental/search/commands + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：127 测试通过（119 unit + 5 integration + 3 snapshot），0 失败
- 代码量：7,902 行 / 48 .rs 文件

## 二、本次变更影响范围
- 修改的功能：修复 FileInsight + SearchSection 新增字段导致的测试编译错误
- 摸到的文件：`src/analysis/graph.rs`（3处）、`src/analysis/mod.rs`（2处）、`src/generate/card.rs`（1处）、`src/generate/chunk.rs`（1处）、`src/generate/wiki.rs`（1处）、`tests/integration_test.rs`（1处）
- 是否改变了接口/契约：否，仅补全测试代码中的结构体字段

## 三、已知风险点（由AI诚实自曝）

### P0 — 必须修
| # | 问题 | 位置 | 说明 |
|---|------|------|------|
| 1 | commands.rs 死代码 | `src/commands.rs` | 完整 install 逻辑但从未 `mod commands;`，实际 InstallToOpencode 2/10 分 |
| 2 | Java 解析器未注册 | `parser/mod.rs:65-70` | 173 行完整代码搁置，unreachable |
| 3 | 无 README.md | 项目根 | 零入口文档，用户无法了解项目 |
| 4 | FileInsight 不缓存 source | `ingest/parser/mod.rs:13` | 搜索索引构建时重读磁盘（2x I/O） |

### P1 — 应该修
| # | 问题 | 位置 | 严重度 |
|---|------|------|--------|
| 5 | STATUS.md 测试计数 134≠126 | `STATUS.md:6` | 误导性 |
| 6-7 | sqlite-vec + async-openai 未用 | `Cargo.toml` | 增编译时间 |
| 8 | rrf_k=60 硬编码 | `agent.rs:37`, `lib.rs:408` | 应参数化 |
| 9 | impact max_depth=3 硬编码 | `impact.rs:38` | 应参数化 |
| 10 | diff repo_path="." | `diff.rs:28` | 不可定制 |
| 11 | 无性能基准 benches/ | `benches/` 缺失 | 无法量化性能 |
| 12 | 无 doc-tests | 全局 | 零文档测试 |

### P2 — 值得修
| # | 问题 | 位置 |
|---|------|------|
| 14 | SemanticEngine load_all_vectors O(n) | `semantic.rs:77` |
| 15 | module_fingerprints 永远空 | `state.rs:68` |
| 16 | ✅ HTML `<cite>` 在 .md 中 | `crossref.rs:94` — 已改为 Markdown 链接 |
| 17 | 5 个独立 tokio Runtime | lib.rs + semantic.rs |
| 18 | SearchAgent 未集成到 pipeline | `lib.rs:execute_search` |
| 19 | pulldown-cmark Options::all() 过宽松 | `html.rs:195` |
| 20 | crossref contains() 子串误报 | `crossref.rs:55` |
| 21 | module_path[0] 丢子模块信息 | `impact.rs:81` |

### Spec 差距
- Phase 6（OpenCode Plugin）: **0%** — 无 MCP 工具、无 slash 命令、无进度反馈
- Phase 2（Knowledge Graph）: **63%** — 已补 complete_stream SSE / Level 0 实体摘要 / <cite> Markdown 链接 / 循环标注；缺 Leiden 算法、图持久化、call_edges 精确匹配
- 总体实现度：**45/70 = 64%**

## 四、下次最该做的事（AI建议）
1. **Phase 2 剩余**: 实现 Leiden 模块聚类算法 + 图持久化 + call_edges 精确匹配
2. **Phase 0**: 清理 `commands.rs` 死代码（P0.1） + 修正 STATUS.md 计数 + 写 README.md
