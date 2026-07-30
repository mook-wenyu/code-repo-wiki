# 项目状态简报 （AI自动维护）

## 一、架构健康度
- 当前模块总数：12（config/model/ingest/analysis/generate/output/incremental/search/commands + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：139 测试通过（119 unit + 5 integration + 3 snapshot + 5 plan + 4 multilang + 3 protected），0 失败
- 代码量：约 8,200 行 / 53 .rs 文件

## 二、本次变更影响范围
- 修改的功能：全 7 Phase 实现完成 — P0-P2 修复 + Phase 3(plan) + Phase 4(multi-lang) + Phase 5(doc protection) + Phase 6(/knowledge)
- 摸到的文件：`Cargo.toml`、`src/lib.rs`（+doc_fingerprints）、`src/config/schema.rs`（+PlanConfig+expand_languages）、`src/config/plan.rs`（新文件）、`src/config/mod.rs`（+plan加载）、`src/output/mod.rs`（+多语言目录）、`src/output/markdown.rs`（+language参数）、`src/generate/mod.rs`（+plan注入+语言参数）、`src/generate/card.rs`（+language字段）、`src/incremental/state.rs`（+doc_fingerprints）、`src/incremental/mod.rs`（+保护检查）、`src/incremental/impact.rs`（module_path修复）、`src/search/agent.rs`（+rrf_k参数）、`src/search/semantic.rs`（+Arc<Runtime>）、`.opencode/plugins/repo-wiki.ts`（参数修复+/knowledge命令）、`tests/`（3 个新测试文件）
- 是否改变了接口/契约：是 — `write_document` 新增 `language` 参数，`CardGenerator::new` 新增 `language` 参数，`SemanticEngine::open` 新增 `rt` 参数，`SearchAgent::new` 新增 `rrf_k` 参数

## 三、已知风险点（由AI诚实自曝）
### P1 — 应该修
| # | 问题 | 位置 | 严重度 |
|---|------|------|--------|
| 1 | Java 解析器未注册 | `parser/mod.rs` | 173 行死代码 |
| 2 | 无 README.md | 项目根 | 零入口文档 |
| 3 | 无性能基准 benches/ | benches/ 缺失 | 无法量化性能 |
| 4 | 无 doc-tests | 全局 | 零文档测试 |

### P2 — 值得修
| # | 问题 | 位置 |
|---|------|------|
| 5 | SemanticEngine load_all_vectors O(n) | `semantic.rs` |
| 6 | module_fingerprints 永远空 | `state.rs` |
| 7 | diff repo_path="." 硬编码 | `diff.rs` |

### Spec 差距
- Phase 6（OpenCode Plugin）: **60%** — 已补 /knowledge 命令（generate/modify/supplement/rewrite）；缺 MCP Agent 工具定义
- Phase 2（Knowledge Graph）: **70%** — 已补 complete_stream SSE / Level 0 实体摘要 / <cite> Markdown 链接 / 循环标注 / 图持久化 / call_edges 精确匹配；缺 Leiden 模块聚类算法
- 总体实现度：**56/70 = 80%**

## 四、下次最该做的事（AI建议）
1. 写 README.md（补充入口文档）
2. 注册 Java 解析器到 `parser/mod.rs`
3. 添加 benches/ 目录（criterion 基准测试）
