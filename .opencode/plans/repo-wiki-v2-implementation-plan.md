# repo-wiki v2 完整实现计划

> 基于全面代码审计（覆盖 src/ 下全部 42+ Rust 源文件 + Cargo.toml + OpenCode 插件 + 配置）设计。
> 已验证：代码库当前状态比审计报告描述的更先进——SQLite FTS5 已实现并集成，search/store.rs 已声明在 mod.rs。
> 2026-07-31

---

## 一、未完成项全景表

| 编号 | 模块 | 当前状态 | 风险等级 | 预估规模 | 依赖 |
|------|------|---------|---------|---------|------|
| **F01** | `generate/mod.rs:89-90` | cards/chunks 索引对齐断裂 — 跳过失败卡片后后续 chunks 与错位的 cards 配对 | **P0** | 1 文件 ~3 行修改 | 无 |
| **F02** | `main.rs:88` | Generate `--output` 参数 `output: _` 静默丢弃 | **P0** | 1 文件 ~5 行修改 + lib.rs 签名变更 | F04 |
| **F03** | `impact.rs:29` | `file_paths.iter().any(\|cfp\| fp.contains(cfp.as_str()))` 子串匹配 → `src/parser` 匹配 `src/parser_utils` | **P0** | 1 行修改 | 无 |
| **F04** | `lib.rs:104-117` | 增量更新无变更时返回空 `Vec::new()` → 清空输出目录 | **P0** | 1 文件 ~10 行修改 | 无 |
| **F05** | `.opencode/plugins/repo-wiki.ts:17` | `execSync` 无 `timeout` 参数 → LLM hang 时阻塞 OpenCode 进程 | **P0** | 1 行修改 | 无 |
| **F06** | `search::agent::SearchAgent` | 已实现 155 行 4 测试，但未接入任何 runtime 调用路径 | **P1** | lib.rs ~5 行集成调用 | 无 |
| **F07** | `search::ast::AstQuery` | 已实现 201 行 6 测试，未接入 pipeline | **P1** | lib.rs ~3 行集成 | 无 |
| **F08** | `search::callgraph::CallGraph` | 已实现 103 行 2 测试，未接入 pipeline | **P1** | lib.rs ~3 行集成 | F07 |
| **F09** | `generate/mod.rs:56-129 vs 135-209` | `run_generation` 和 `run_generation_filtered` 约 80% 代码重复 | **P2** | ~80 行提取为共享函数 | 无 |
| **F10** | `generate/mod.rs:64-72` | 当前 `chunks.len()` 单独输出，但 cards/chunks 长度可能不同 | **P1** | 延续 F01 修复 | F01 |
| **F11** | `incremental/diff.rs:99-105` | `renamed` 文件被解析但从未被 `changed_files` 消费 | **P1** | 2 行修改 | 无 |
| **F12** | `graph.rs:190-195` | `name_map` 已修复为 `HashMap<String, Vec<NodeId>>`, 但 `collect_node_names` 仍被调用 3 次 | **P2** | 缓存 + 重构 | 无 |
| **F13** | `embed.rs:67-103` | Embedding API 无重试逻辑；`filter_map` 静默丢弃非 `f64` → 产生更短向量 | **P1** | 2 处修改 | 无 |
| **F14** | `llm.rs:161` | 退避算法无抖动 → 并行请求同一瞬间重试（雷鸣羊群） | **P1** | 2 行添加 `rand` | 无 |
| **F15** | `llm.rs:174-183` | 4xx 与 5xx 同样重试 → 400 Bad Request 消耗所有重试 | **P1** | ~5 行添加 `if status == 429/5xx` | 无 |
| **F16** | `card.rs:99-113` | LLM 响应缺失字段静默变为空字符串；生成失败静默跳过无计数 | **P1** | ~5 行添加 `tracing::warn` | 无 |
| **F17** | `state.rs:41` | `save()` 不是原子操作 → 写入时崩溃产生损坏文件 | **P2** | 先写临时文件再 `rename` | 无 |
| **F18** | `html.rs:183` | CSS 路径硬编码 `../style.css`, 索引页需要 `./style.css` | **P1** | 1 行修改 | 无 |
| **F19** | `mermaid.rs:140-142` | `module_name` 只返回路径第一段 → 所有模块显示为 "src" | **P1** | 1 行修改 | 无 |
| **F20** | `python.rs:59` | 签名在 `':'` 处截断 → `def greet(name: str)` 返回 `def greet(name` | **P2** | ~3 行修改 | 无 |
| **F21** | `prompt.rs` | 源代码未传递给 LLM → 生成的文档缺少详细描述 | **P1** | 与 Entity.source_code 集成 | F06 |
| **F22** | `lib.rs:54,120` | 每次 `run_pipeline` 创建新 `tokio::runtime::Runtime` | **P2** | 改为 `lazy_static` 或 `OnceCell` | 无 |
| **F23** | `parser/mod.rs:64-69` | 6 个 `.unwrap()` 在 tree-sitter 初始化失败时 panic | **P2** | 改为 `?` 传播 | 无 |
| **F24** | `scanner.rs:21-22` | 无效 glob 模式被静默忽略 | **P2** | ~5 行添加 `glob::Pattern::new` 验证 | 无 |
| **F25** | `diff.rs:28` | `git2::Repository::open` 同步阻塞 | **P2** | `spawn_blocking` 包装 | 无 |
| **F26** | `watch.rs:144` | Channel 断开错误被 `.ok()` 吞掉 | **P2** | 2 行修改 | 无 |
| **F27** | `config/schema.rs` | `temperature` 无范围校验 (0.0~2.0) | **P3** | ~5 行添加校验 | 无 |
| **F28** | `markdown.rs:122,140` | 模块路径未 sanitize → 路径遍历风险 | **P3** | `Path::components` 过滤 | 无 |
| **F29** | `config/opencode.rs:128` | Windows config_dir 用 HOME/USERPROFILE 拼接 → 路径错误 | **P3** | 改为 `dirs::config_dir()` | 无 |
| **F30** | `llm.rs:110` | `reqwest::Client` 无超时设置 → 无限等待 | **P1** | ~3 行 `.timeout()` 设置 | 无 |
| **F31** | `core` | 无 CI 配置、无 lint 检查 | **P3** | 添加 `.github/workflows/ci.yml` | 无 |
| **F32** | `test` | 覆盖率约 10%，关键路径（Parser fallback、diff、watch、impact、generate）无测试 | **P2** | 每个文件至少 1 个集成测试 | 全部 |

---

## 二、分阶段实施计划

### Phase 0 — P0 灾难修复（5 项，零依赖，可并行）

| 步骤 | 文件 | 操作 | 验证 |
|------|------|------|------|
| 0.1 | `impact.rs:29` | `fp.contains(cfp)` → `fp == cfp` | 测试 `test_propagate_impact` 仍然通过 |
| 0.2 | `generate/mod.rs:89-90` | 改为用 `HashMap<module_path, KnowledgeCard>` 替代 Vec 索引对齐 | `for chunk in chunks { if let Some(card) = card_map.get(&chunk.module_path) }` |
| 0.3 | `main.rs:88` + `lib.rs:32` | `output: output` 传递到 `run_pipeline`；若 `Some` 覆盖 `config.output.dir` | CLI 测试 `--output /tmp/wiki` 生效 |
| 0.4 | `lib.rs:104-117` | 增量更新无变更时执行全量 `output::render_all` 且返回当前已有输出 | 增量流水线测试 |
| 0.5 | `.opencode/plugins/repo-wiki.ts:17` | 添加 `timeout: 300_000` | 插件功能测试 |

### Phase 1 — 搜索模块接入 Pipeline（3 项，依赖 F06）

| 步骤 | 文件 | 操作 | 验证 |
|------|------|------|------|
| 1.1 | `lib.rs` + `search::agent::SearchAgent` | 在 `execute_search` hybrid 路径后增加 Agent 组合搜索 | `execute_search --engine agent` 正常工作 |
| 1.2 | `lib.rs` build_graph 后 | 调用 `search::ast::AstQuery::index_graph(graph)` 缓存 AST 查询 | `search::search("fn foo")` 支持 AST 精确定位 |
| 1.3 | `lib.rs` + `search::callgraph::CallGraph` | build_graph 后自动构建 `CallGraph` 实例；暴露 `trace_call` 和 `caller_of` | `trace_call("add")` 返回完整链路 |

### Phase 2 — P1 严重修复（10 项）

| 步骤 | 优先级 | 文件 | 修改 |
|------|--------|------|------|
| 2.1 | P1 | `diff.rs:99-105` | renamed 文件追加到 `changed_files` |
| 2.2 | P1 | `llm.rs:174-183` | 4xx 除 429 外不重试 |
| 2.3 | P1 | `llm.rs:161` | 退避添加 `rand::thread_rng().gen_range(0..1000)` 抖动 |
| 2.4 | P1 | `llm.rs:110` | `reqwest::Client::builder().timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(10))` |
| 2.5 | P1 | `embed.rs:67-103` | 添加重试逻辑 + `filter_map` 改为记录 warn |
| 2.6 | P1 | `card.rs:99-113` | LLM 解析失败时 `tracing::warn!` + 统计计数 |
| 2.7 | P1 | `html.rs:183` | 根据页面层级动态计算 CSS 相对路径 |
| 2.8 | P1 | `mermaid.rs:140-142` | `module_name` 返回完整 `::` 分隔路径 |
| 2.9 | P1 | `prompt.rs` | 传入 `source_code` 到 prompt 模板（需先确认 Chunk.source_codes 存在） |
| 2.10 | P1 | `generate/mod.rs` | 消除 `run_generation` 和 `run_generation_filtered` 代码重复 |

### Phase 3 — P2 架构债务（8 项）

| 步骤 | 文件 | 修改 |
|------|------|------|
| 3.1 | `state.rs:41` | `save()` 改为原子写入（先写 `.tmp` 再 `rename`） |
| 3.2 | `lib.rs:54,120` | `tokio::runtime::Runtime` 改为 `OnceCell<Runtime>` 复用 |
| 3.3 | `parser/mod.rs:64-69` | 6 个 `.unwrap()` → `context("初始化失败")?` |
| 3.4 | `scanner.rs:21-22` | glob 模式加载时 `Pattern::new(&pattern).context(...)?` |
| 3.5 | `diff.rs:28` | `spawn_blocking(|| git2::Repository::open(...))` |
| 3.6 | `watch.rs:144` | channel 断开时 `tracing::error!` 替代 `.ok()` |
| 3.7 | `graph.rs` | `collect_node_names` 结果缓存，消除 3 次重复调用 |
| 3.8 | `chunk.rs:68-72` | 依赖计算从 O(n²) 改为基于图边一次遍历 |

### Phase 4 — P3 改进 + 测试（6 项）

| 步骤 | 文件 | 修改 |
|------|------|------|
| 4.1 | `config/schema.rs` | `temperature` 加 `0.0..=2.0` 校验；`search.default_engine` 验证 |
| 4.2 | `markdown.rs:122,140` | `Path::components().filter(|c| c != Component::ParentDir)` 过滤 |
| 4.3 | `config/opencode.rs:128` | 改为 `dirs::config_dir().unwrap_or_else(...)` |
| 4.4 | `tests/` | 新增集成测试：parser fallback、diff 重命名、impact BFS、watch mock |
| 4.5 | CI | `.github/workflows/ci.yml`：`cargo check` + `cargo test` + `cargo clippy` |
| 4.6 | `llm.rs:286` 等 | Anthropic base_url 可配置 + `max_tokens` 默认值 |



### Phase X — 取消准备（Qoder 兼容层）

| 步骤 | 文件 | 修改 |
|------|------|------|
| X.1 | `Cargo.toml` | 如果 `tree-sitter-c-sharp` 不可用，加注释说明可删除或条件编译 |
| X.2 | `src/ingest/parser/csharp.rs` | 可考虑删除（Qoder 生态不要求 C#）或标记为 `cfg(feature = "csharp")` |
| X.3 | `config/schema.rs` | 增加 `#[serde(deny_unknown_fields)]` 防止配置拼写错误 |

---

## 三、依赖关系图

```
Phase 0（零依赖，可并行）
  ├─ 0.1 impact.rs:29           ← 单行修复
  ├─ 0.2 generate/mod.rs:89-90  ← HashMap 索引对齐
  ├─ 0.3 main.rs:88 + lib.rs    ← --output 传递
  ├─ 0.4 lib.rs:104-117         ← 增量保留旧输出
  └─ 0.5 plugin repo-wiki.ts    ← timeout 参数

Phase 1（依赖 Phase 0 的搜索索引正确）
  ├─ 1.1 SearchAgent 集成      ← 无依赖
  ├─ 1.2 AstQuery 集成         ← 依赖 graph build
  └─ 1.3 CallGraph 集成        ← 依赖 1.2

Phase 2（依赖 Phase 0 + 1）
  ├─ 2.1-2.10 P1 修复          ← 大部分独立，可并行

Phase 3（依赖 Phase 0, 1, 2）
  ├─ 3.1-3.8 P2 债务           ← 多为独立修复

Phase 4（依赖全部前置）
  ├─ 4.1-4.6 P3 + 测试 + CI   ← 终端阶段
```

---

## 四、边界条件与错误处理策略

| 边界 | 当前问题 | 修复策略 |
|------|---------|---------|
| **空仓库** | `graph.graph.node_count() == 0` 未处理 → `impact.rs:13` 返回空 vector | ✅ 已处理 |
| **无 Git 历史** | `diff.rs:52-54` 无 `last_commit_hash` 使用 `HEAD^` 失败 | `git2::Repository::open` 失败时回退全量扫描 |
| **Embed API Key 缺失** | `embed.rs:37` `std::env::var("OPENAI_API_KEY")` panic | `api_key` 从配置 `LlmSection` 读取，缺失时 `tracing::warn` |
| **SQLite 文件锁** | `store.rs:37` `busy_timeout(5s)` | WAL 模式允许多读单写，5s 超时后返回错误 |
| **零匹配搜索** | `execute_search` 返回空 list | CLI 输出"未找到匹配结果" |
| **增量无变更** | `lib.rs:104-117` 返回空 list | 应执行全量 `output::render_all` 并返回当前 `documents` |
| **Parser 初始化失败** | 6 个 `.unwrap()` → panic | 改为 `?` 传播 + `tracing::error` |
| **无效 glob 模式** | 静默跳过 | 配置加载时 `glob::Pattern::new` 验证 |
| **大文件读入内存** | `state.rs:80` 整个文件读入 | 改为流式 `Read` |
| **路径遍历** | `markdown.rs:122,140` 未 sanitize | `Path::components` 过滤 `..` |
| **CLI 退出码** | 所有命令返回 `Ok(())` | 失败时返回非零退出码 |

---

## 五、验证标准

```
必须通过：
  cargo check     → 0 errors, 0 warnings
  cargo test      → 全部通过（当前 111 tests → 目标 ≥130）
  cargo clippy    → 无警告（新增后添加）

CLI 功能验证：
  repo-wiki generate --config .repo-wiki/config.toml --output /tmp/wiki
    → 输出目录应为 /tmp/wiki（非默认路径）

  repo-wiki update
    → 无变更时不应清空输出目录

  repo-wiki search --query "parse" --json
    → 返回结构化 JSON 结果

边界测试：
  空仓库 → 流水线正常完成，输出空 wiki
  无 Git 历史 → 增量更新退化为全量
  Embed API Key 缺失 → 搜索降至 text-only
  SQLite 文件损坏 → 自动重建索引
```
