# Repo-Wiki Spec合规与代码质量完善 — 完整实现计划

> 基于 `docs/full-audit-report.md` (18 项审计发现) + 6 个 Wayfinder Tickets

---

## 执行阶段

### Phase 0 — 快速胜利（7项，可并行）

| 优先级 | 问题 | 文件 | 改动 | 估算 |
|--------|------|------|------|------|
| 0.1 | P0.1 删除未使用依赖 | `Cargo.toml:25,37` | 删 `sqlite-vec`, `async-openai` 2 行 | 1 分钟 |
| 0.2 | P1.8 impact 精确路径 | `impact.rs:29` | `fp.contains(cfp)` → `fp == cfp` | 1 分钟 |
| 0.3 | P1.11 crossref 精确路径 | `crossref.rs:55` | `contains` → 精确匹配 | 1 分钟 |
| 0.4 | P1.9 html Options 显式 | `html.rs:195` | `Options::all()` → `ENABLE_TABLES \| ENABLE_FENCED_CODE_BLOCKS` | 1 分钟 |
| 0.5 | P1.5 mermaid 死代码 | `mermaid.rs:66,107` | `render_entity_graph`/`render_cycle_diagram` 加 `#[allow(dead_code)]` | 1 分钟 |
| 0.6 | P2.14 ast_chunker 死代码 | `ast_chunker.rs:1` | 加 `#[allow(dead_code)]` | 1 分钟 |
| 0.7 | P2.6 state 未使用导入 | `state.rs:2` | 删 `use std::io::Read` | 1 分钟 |

**验证**: 每项独立 `cargo check`。全部可同时执行。

---

### Phase 1 — 数据模型与架构（3项，需设计决策）

| 优先级 | 问题 | 方案概要 | 估算 |
|--------|------|----------|------|
| 1.1 | P0.2 tokio Runtime 缓存 | `lib.rs` 顶层 `OnceLock<Arc<Runtime>>` 共享单 RT | ~30 行 |
| 1.2 | P0.4 FileInsight 带 source | FileInsight 加 `source: String`，parse 时填充，索引从字段取 | ~25 行(5 文件) |
| 1.3 | Spec #1 图序列化 | KnowledgeGraph `save/load` 用 bincode(已有依赖)写入 `output.dir/graph/` | ~80 行 |

**依赖**: 无。三项可并行。

---

### Phase 2 — Spec 合规特性（4项，可并行）

| 优先级 | 问题 | 方案概要 | 估算 | 依赖 |
|--------|------|----------|------|------|
| 2.1 | Spec #2 complete_stream | OpenAi SSE 流式读取，Anthropic Message 流式读取 | ~60 行 | 无 |
| 2.2 | Spec #3 Level 0 实体摘要 | pipeline 调用 `entity_summary_prompt` + LLM，结果注入 Card | ~40 行 | 无 |
| 2.3 | Spec #4 `<cite>` 渲染 | crossref.rs TODO 实现，跨文件 Markdown 链接 | ~30 行 | Phase 1.3(需要图) |
| 2.4 | Spec #9 循环依赖标注 | mermaid 高亮 + architecture.md 循环警告段落 | ~20 行 | 无 |

---

### Phase 3 — CLI + 集成（3项，可并行）

| 优先级 | 问题 | 方案概要 | 估算 |
|--------|------|----------|------|
| 3.1 | CLI Install/Uninstall | `install`(创建默认 config + 插件 + git hooks) + `uninstall`(卸载 + 清理) | ~140 行 |
| 3.2 | Spec #8 Java 处理器 | `tree-sitter-java` 依赖 + `JavaProcessor` walk+fallback + 注册 | ~100 行 |
| 3.3 | P1.7 SearchAgent 接入 | `execute_search()` 新增 `--agent` 参数，委托 SearchAgent 智能回溯 | ~15 行 |

---

### Phase 4 — 测试完善（2项）

| 优先级 | 问题 | 方案概要 | 估算 |
|--------|------|----------|------|
| 4.1 | P2.17 全流水线 E2E 测试 | MockProvider 跑 `run_pipeline()` 验证输出目录文件存在 | ~50 行 |
| 4.2 | Spec #7 性能基准 | `benches/bench.rs` — 扫描+解析+建图+搜索 4 个基准 | ~80 行 |

---

## 波及范围矩阵

| Phase | 修改文件数量 | 新增文件 | 新增依赖 | 测试增量 |
|-------|-------------|----------|----------|----------|
| 0 | 6 | 0 | 0 | 0 |
| 1 | 5 | 0 | 0 | 0 |
| 2 | 7 | 0 | 0 | 4-8 |
| 3 | 4 | 1(java.rs) | 1(tree-sitter-java) | 2-4 |
| 4 | 1 | 2 | 0 | 5-10 |

---

## 推荐执行顺序

```
Week 1: Phase 0 (7 quick wins, 7 min) → P0/P1 全部消除
       → Phase 1.1 (Runtime) + Phase 1.2 (FileInsight) [并行]
       
Week 2: Phase 1.3 (图序列化) + Phase 2.1 (complete_stream) [并行]
       → Phase 2.2 (Level 0) + Phase 2.3 (cite) [并行,依赖 1.3]
       
Week 3: Phase 2.4 (循环标注) + Phase 3.1 (CLI) [并行]
       → Phase 3.2 (Java) + Phase 3.3 (SearchAgent) [并行]
       
Week 4: Phase 4.1 (E2E) + Phase 4.2 (bench) [并行]
       → 最终验证 clippy + test + manual check
```

由于 Notes 覆盖允许携带执行，**实际可按单 session 多 Phase 并行子代理执行**，不严格遵循周计划。

---

## 验证策略

| 阶段 | 验证命令 | 验收标准 |
|------|----------|----------|
| 每项 | `cargo check` | 0 errors |
| Phase 0 | `cargo check` | 0 errors, 0 warnings |
| Phase 1 | `cargo clippy -D warnings` | 0 errors |
| Phase 2 | `cargo test` | 全部通过 + 新测试覆盖新行为 |
| Phase 3 | `cargo test` + `cargo check` | 新增测试 + 0 warnings |
| Phase 4 | `cargo test` + `cargo bench`(可选) | E2E 验证 pipeline 输出 + 基准可运行 |

---

## 未包含（Fog of war → Not yet specified）

- GitHub Actions CI/CD: 依赖仓库所有者配置，不在本计划中
- 发布流程: 仓库无 cargo publish 配置，首次发布前单独计划
- 多 Agent 集成: CLI install 支持多 Agent 后，具体 TS plugin 实现需独立 sprint
- 监控遥测: LLM 成本追踪需要外部存储，属额外项目
