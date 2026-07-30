# repo-wiki → Qoder Repo Wiki 功能对等升级实现计划

> 现状：49 .rs 文件，7,902 行，127 测试全部通过，7 层架构无循环依赖。
> 目标：覆盖 wiki_plan.yaml、/knowledge 交互、多语言输出、人工修改保护、P0/P1/P2 修复。

---

## 执行顺序总图

```
Phase 0 (P0 Bug Fixes)
  └→ Phase 1 (P1 Bug Fixes) ← 可在 Phase 0 后立即并行
       └→ Phase 2 (RRF_k / Config 修) ← 可在 Phase 1 后立即并行
            └→ Phase 3 (Part A: wiki_plan.yaml)
                 └→ Phase 4 (Part C: 多语言)
                      └→ Phase 5 (Part D: 保护)
                           └→ Phase 6 (Part B: Plugin /knowledge)
```

每个 Phase 完成后 `cargo test` 全绿才进入下一 Phase。

---

## Phase 0 — P0 Bug 修复（3 项）

### P0.1: `build_source_map` 重复读盘

**文件**: `src/lib.rs:320-328`
**当前问题**: `build_source_map()` 遍历 FileInsight 后再次 `fs::read_to_string` 读盘。
**修复方案**: FileInsight 已携带 `source: String`，直接使用。
**变更**:
```
src/lib.rs:208       build_source_map(file_insights)  → 直接传 &file_insights
src/lib.rs:282       同上
src/lib.rs:320-328   删掉整个函数，内联为：
                     fn build_source_map(insights: &[FileInsight]) -> HashMap<String, String> {
                         insights.iter().map(|i| (i.path.to_string_lossy().to_string(), i.source.clone())).collect()
                     }
```
**验证**: 搜索索引构建的单元测试 + `cargo test`

### P0.2: `semantic.rs` 3 个独立 tokio Runtime

**文件**: `src/search/semantic.rs:37,62,82`
**当前问题**: `index()`、`index_batch()`、`search()` 各 `Runtime::new()` 一次，浪费资源。
**修复方案**: 在 `SemanticEngine` 中持有 `Arc<Runtime>`，从 `lib.rs:36` 的全局 Runtime 传入。
**变更**:
```
src/search/semantic.rs:19-21   struct SemanticEngine 增加 rt: Arc<Runtime>
src/search/semantic.rs:25-28   open(path, embedder)  →  open(path, embedder, rt: Arc<Runtime>)
src/search/semantic.rs:37      删除 Runtime::new()，使用 self.rt.block_on
src/search/semantic.rs:62      同上
src/search/semantic.rs:82      同上
src/lib.rs:240                 SemanticEngine::open(&semantic_path, embedder)
                               → 传 get_global_runtime().clone()
src/lib.rs:306                 同上
src/lib.rs:389                 同上
src/lib.rs:407                 同上
```
**验证**: `cargo test` + `cargo clippy`

### P0.3: 创建 README.md

**文件**: `README.md`（新建）
**内容要点**: 项目简介、架构图（as text）、快速开始、CLI 子命令一览、配置说明、搜索功能
**验证**: 纯文档，无测试

---

## Phase 1 — P1 Bug 修复（6 项）

### P1.1: SearchAgent 未接入 execute_search

**文件**: `src/lib.rs:358-418`, `src/search/agent.rs`
**当前问题**: `execute_search()` 直接拼装 TextEngine + SemanticEngine，跳过 SearchAgent 的分层回溯逻辑。
**修复方案**: `execute_search()` 对 Hybrid 类型直接使用 SearchAgent。
**变更**:
```
src/lib.rs:374-418   在 execute_search 中增加：
                     SearchEngineType::Hybrid => {
                         let agent = SearchAgent::new(text_engine, semantic_engine, config.search.rrf_k);
                         Ok(agent.search(query, top_k, true))
                     }
src/search/agent.rs:19-21   SearchAgent::new 增加 rrf_k 参数
src/search/agent.rs:24      self.search 无需改，auto_backtrack 原本就支持
```
**验证**: 编写 `#[test]` 验证 SearchAgent（Mock）分层回溯路径

### P1.2: 插件 wiki_generate 参数错误

**文件**: `.opencode/plugins/repo-wiki.ts:128`
**当前问题**: `execa("repo-wiki", ["generate", "-o", ".repo-wiki", output])` — `output` 又传给 positional arg。
**修复方案**:
```
// 改为：
const args = ["generate", "--config", ".repo-wiki/config.toml"];
if (output) args.push("-o", output);
```
**验证**: 手动测试插件

### P1.3: module_info 不应全量 generate

**文件**: `.opencode/plugins/repo-wiki.ts:141`
**当前问题**: `module_info` 每次都 `await runCli(["generate"])`。
**修复方案**: 删掉该行，改从 `readExistingCards()` 匹配后若未找到则返回未找到。
**变更**:
```
// 删除第 141 行 await runCli(["generate"]);
// 改为只读现有卡片
const cards = await readExistingCards();
```
**验证**: 手动测试插件

### P1.4: impact.rs module_path[0] 降级

**文件**: `src/incremental/impact.rs:49,80`
**当前问题**: `module_path[0]` 只取第一级路径（如 `"src"` 而非 `"src::config::schema"`）。
**修复方案**: 用完整 `module_path.join("::")` 替代 `module_path[0]`。
**变更**:
```
src/incremental/impact.rs:49   affected.insert(start_node.module_path[0].clone());
                              → affected.insert(start_node.module_path.join("::"));
src/incremental/impact.rs:80   affected.insert(neighbor_node.module_path[0].clone());
                              → affected.insert(neighbor_node.module_path.join("::"));
```
**验证**: 更新 `test_propagate_impact` 的 assert 预期值

### P1.5: post-merge hook 不一致

**文件**: `src/commands.rs:24-35`
**当前问题**: 安装只写 `post-commit`，卸载却检查 `post-merge` 并删除。
**修复方案**: 增加 `post-merge` hook 安装。
**变更**:
```
src/commands.rs:28    在 post-commit 后追加 post-merge 安装（逻辑相同）
```
**验证**: `#[test]` 验证 hook 文件存在

### P1.6: 无性能基准

**文件**: `benches/bench_main.rs`（新建）
**内容**: 用 `criterion` 对核心流水线 benchmark（可选），或简单 time 脚本。
**方案**: 在 `benches/` 下创建 benchmark：
- `benches/search_bench.rs`: TextEngine search 性能基准
- `benches/chunk_bench.rs`: chunk_by_module 性能基准
**验证**: `cargo bench` 或 skip（不影响生产）

---

## Phase 2 — P2 Bug 修复（4 项）

### P2.1: rrf_k 硬编码 → 配置化

**文件**: `src/search/agent.rs:20`
**当前问题**: `rrf_k: 60.0` 硬编码，但 `config.search.rrf_k` 已存在。
**修复方案**: 构造函数接收 `rrf_k` 参数。
**变更**:
```
src/search/agent.rs:19-21   SearchAgent::new(text, semantic, rrf_k: f64)
src/search/agent.rs:20      self.rrf_k = rrf_k
src/search/agent.rs:31      rrf_k 在 rrf_merge 调用时传入
```
**验证**: 单元测试覆盖自定义 rrf_k

### P2.2: SemanticEngine O(n) 扫描

**文件**: `src/search/semantic.rs:76-96`
**当前问题**: `search()` 每次都 `load_all_vectors()`（全表扫描到内存），大项目不可扩展。
**修复方案**: 增加 SQLite 缓存层：保持当前 O(n) 实现作为 fallback，但加入 `ponytail:` 注释标记升级路径。
```
// ponytail: O(n) 扫描，备选方案：sqlite-vec 插件或 HNSW 近似索引，当向量数 > 10K 时启动
```
**变更**: 只加注释，不做架构级重构（YAGNI: 当前向量数远低于 10K）。
**验证**: 现有测试通过

### P2.3: 进度反馈

**文件**: `src/generate/mod.rs:56-143`, `src/lib.rs:42-98`
**当前问题**: 生成过程中无进度反馈，插件 `sendProgress` 只用了两次（0% 和 100%）。
**修复方案**: 在 `run_generation` 各阶段插入 `tracing::info!`（已有）+ 向插件回传进度。
**变更**:
```
src/generate/mod.rs:64     tracing::info!("分块完成")    ← 已有
src/generate/mod.rs:93     tracing::info!("卡片生成")     ← 改进为 30% → 60% → 90% 进度日志
src/generate/mod.rs:113    tracing::info!("Wiki 页面")    ← 已有
```
插件端不变（CLI 模式无法回传细粒度进度，tracing log 足够）。

### P2.4: 空查询短路

**文件**: `src/lib.rs:363`（execute_search 入口）
**当前问题**: `query: ""` 时仍会打开索引执行空搜索。
**修复方案**: 在 `execute_search` 入口加空查询判断。
**变更**:
```
src/lib.rs:363    在最前加：
                  if query.trim().is_empty() {
                      return Ok(Vec::new());
                  }
```
**验证**: 单元测试 `test_empty_query_returns_empty`

---

## Phase 3 — Part A: wiki_plan.yaml 前置干预系统

### A.1 新增 Plan 数据结构

**文件**: `src/config/plan.rs`（新建）
**内容**:
```rust
/// wiki_plan.yaml 的前置干预配置
///
/// 用户通过此文件控制 LLM 生成的内容方向，包括：
/// - 模板选择（architecture / prd）
/// - 按文档维度注入 notes 指令
/// - 覆盖全局作用域
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiPlan {
    /// 全局 Notes（注入到所有 prompt 的 system message）
    pub notes: Option<String>,
    /// 按 section 的详细规划
    pub sections: Vec<PlanSection>,
    /// 要生成的文档列表（白名单）
    pub documents: Vec<PlanDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSection {
    /// 匹配的模块路径（支持 glob: "src/config/**"）
    pub module_pattern: String,
    /// 该模块使用的模板
    pub template_type: PlanTemplateType,
    /// 对该模块 LLM 的额外指导
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanTemplateType {
    #[serde(rename = "architecture")]
    Architecture,
    #[serde(rename = "prd")]
    Prd,
    #[serde(rename = "api-ref")]
    ApiRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDocument {
    pub title: String,
    pub goal: String,
    pub parent: Option<String>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
}
```

### A.2 WikiConfig 增加 plan 字段

**文件**: `src/config/schema.rs:17`
**变更**:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiConfig {
    // ... 现有字段
    #[serde(default)]
    pub plan: PlanConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanConfig {
    pub enabled: bool,
    /// wiki_plan.yaml 路径（相对于 output.dir）
    pub path: String,
}
impl Default for PlanConfig {
    fn default() -> Self { Self { enabled: false, path: "wiki_plan.yaml".into() } }
}
```

### A.3 加载和应用 Plan

**文件**: `src/config/plan.rs` 增加 `load_plan` + `apply_plan_to_config`
**文件**: `src/config/mod.rs` 增加 `load_config_with_plan`
**变更**:
```
src/config/mod.rs:9-18    load_config 后增加 plan 加载逻辑：
                          1. 如果 config.plan.enabled，读取 {output.dir}/{plan.path}
                          2. 解析 wiki_plan.yaml
                          3. merge 到 config（plan.notes 覆盖默认 prompt）
src/generate/mod.rs:56-143  在 run_generation 中注入 plan
                          1. 生成 prompt 前检查 config.plan.enabled
                          2. 全局 plan.notes 追加到 system prompt
                          3. section notes 通过匹配 module_pattern 注入
```

### A.4 测试方案

```
# 文件
tests/test_plan.rs（新建）

# 测试用例
1. test_plan_default_disabled: plan.enabled=false 时不影响生成
2. test_plan_load_yaml: 从 yaml 文件正确解析 WikiPlan
3. test_plan_notes_injected: plan.notes 出现在生成的 system prompt 中
4. test_plan_section_pattern_match: section.module_pattern glob 匹配生效
5. test_plan_document_filter: white-list 只生成匹配的文档
```

---

## Phase 4 — Part C: 多语言输出

### C.1 配置扩展

**文件**: `src/config/schema.rs:23-36`
**变更**:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSection {
    pub template: WikiTemplate,
    /// 默认输出语言
    pub language: String,
    /// 扩展语言列表（同时输出多种语言）
    #[serde(default)]
    pub expand_languages: Vec<String>,
}
```
`expand_languages` 默认空；启用时产生 `wiki/zh/`, `wiki/en/` 等子目录。

### C.2 Output 层改造

**文件**: `src/output/mod.rs:22-99`
**变更**:
```
src/output/mod.rs:28-55   render_all 判断 expand_languages：
                          如果 expand_languages 为空 → 单语言输出（现有路径不变）
                          如果 expand_languages 非空 → 对每种语言：
                              wiki/{lang}/、cards/{lang}/ 各一套
                              prompt 传入对应 language 参数
```

**文件**: `src/output/markdown.rs:136-163`
**变更**:
```
write_document 增加 language 参数：
  output_dir/wiki/{lang}/{module}.md
  output_dir/cards/{lang}/{card}.md
```

### C.3 Generate 层支持多语言生成

**文件**: `src/generate/mod.rs:56-143`
**变更**:
```
run_generation 内部：当 expand_languages 非空时
  for lang in [config.wiki.language] + config.wiki.expand_languages:
      以 lang 为参数调用 prompt::knowledge_card_prompt(chunk, lang)
      → 生成多份 cards / documents
```
注意：让 LLM 为每种语言生成独立内容，而非简单翻译。

### C.4 测试方案

```
tests/test_multilang.rs（新建）
1. test_single_lang_output: expand_languages 为空，目录结构不变
2. test_multi_lang_output: ["en"] 时 wiki/zh/ + wiki/en/ 同时存在
3. test_card_lang_separation: cards/zh/ 和 cards/en/ 分离
```

---

## Phase 5 — Part D: 人工修改保护

### D.1 GenerationState 增加文档指纹

**文件**: `src/incremental/state.rs:12-21`
**变更**:
```rust
pub struct GenerationState {
    // 现有字段
    pub last_commit_hash: Option<String>,
    pub file_fingerprints: HashMap<String, String>,
    pub module_fingerprints: HashMap<String, String>,
    pub generated_at: String,
    // 新增
    /// 已生成文档的路径 → SHA256（用于人工修改检测）
    pub doc_fingerprints: HashMap<String, String>,
}
```

### D.2 保存生成后的文档指纹

**文件**: `src/incremental/state.rs` 增加 `record_doc_fingerprints(docs: &[WikiDocument], output_dir: &Path)`

**文件**: `src/lib.rs:78-85` 在 GenerationState 保存前调用 `record_doc_fingerprints`

### D.3 增量更新前检查保护

**文件**: `src/incremental/mod.rs:90-106`
**变更**:
```
在 run_git_diff_incremental 中：
1. 加载上一次的 GenerationState
2. 对比当前 .md 文件的 SHA256 与 state.doc_fingerprints
3. 不匹配 → 人工修改 → 从 changed_files 中排除该文档 → tracing::warn
```

### D.4 测试方案

```
tests/test_protected_files.rs（新建）
1. test_custom_edit_protected: 手动修改 .md 后增量更新不覆盖
2. test_unprotected_overwritten: 未修改过的文件正常更新
3. test_protected_warning_logged: 跳过保护文件时发出 warn
```

---

## Phase 6 — Part B: /knowledge 命令交互

### B.1 新增 4 个 Slash 命令

**文件**: `.opencode/plugins/repo-wiki.ts:149-186`
**变更**:
```typescript
commands: [
    // 现有 /wiki 命令组
    { name: "wiki", subcommands: [...] },
    // 新增 /knowledge 命令组
    {
        name: "knowledge",
        description: "知识卡片操作（生成/修改/补充/重写）",
        subcommands: [
            {
                name: "generate",
                description: "生成新的知识卡片",
                execute: async (args: string[]) => {
                    // 读取当前文件 + 卡片 → 调用 LLM → 写入 cards/
                    // 接受本地文件引用作为参考
                }
            },
            {
                name: "modify",
                description: "修改已有卡片",
                args: { card_id: "string", instruction: "string" },
                execute: async (args) => { /* 读现有卡片 → LLM modify → 写回 */ }
            },
            {
                name: "supplement",
                description: "补充已有卡片内容",
                args: { card_id: "string", content: "string" },
                execute: async (args) => { /* 读现有卡片 → LLM append → 写回 */ }
            },
            {
                name: "rewrite",
                description: "重写整张卡片",
                args: { card_id: "string", instruction: "string" },
                execute: async (args) => { /* 读现有卡片 + instruction → LLM regenerate → 写回 */ }
            },
        ]
    }
]
```

### B.2 实现细节

每个 `/knowledge` 子命令的工作流：
1. **generate**: 读取当前项目文件 + 现有 cards/ 索引 → 调 LLM 生成新 card → 写回 `cards/{lang}/{name}.md`
2. **modify**: 读取 `cards/{id}.md` 的 YAML frontmatter 和内容 → 拼接 LLM prompt（"基于 instruction 修改此卡片"） → JSON diff 输出 → 写回
3. **supplement**: 与 modify 类似，prompt 为 "补充而非替换"
4. **rewrite**: 全量重写卡片

所有命令共享 `readExistingCards()` 和一个 `callLlmForCard(messages)` 辅助函数。

### B.3 LLM 调用方式

四种模式都用插件直接调 LLM（而非走 repo-wiki CLI）：
- generate: 用 `context7` 或本地 prompt 作为 system message
- modify/supplement/rewrite: 将现有卡片序列化为 JSON，作为 user message 的关键部分

### B.4 测试方案

```
tests/test_knowledge_commands.ts（插件端手动验证）
1. /knowledge generate src/lib.rs → 生成卡片写入 cards/ 
2. /knowledge modify crate::config "增加环境变量说明" → 修改内容
3. /knowledge supplement crate::config "错误处理" → 追加内容
4. /knowledge rewrite crate::config "按新架构重写" → 全量替换
```

---

## 文件影响总表

| 文件 | Phase | 变更类型 | 行数估计 |
|------|-------|----------|----------|
| `src/lib.rs` | 0, 1, 2 | 修改 | ~40 |
| `src/search/semantic.rs` | 0, 2 | 修改 | ~20 |
| `src/search/agent.rs` | 1, 2 | 修改 | ~15 |
| `src/incremental/impact.rs` | 1 | 修改 | 2 |
| `src/incremental/mod.rs` | 1, 5 | 修改 | ~20 |
| `src/incremental/state.rs` | 5 | 修改 | ~15 |
| `src/config/schema.rs` | 3, 4 | 修改 | ~30 |
| `src/config/mod.rs` | 3 | 修改 | ~20 |
| `src/config/plan.rs` | 3 | **新建** | ~120 |
| `src/generate/mod.rs` | 3, 4 | 修改 | ~25 |
| `src/generate/prompt.rs` | 3 | 修改 | ~10 |
| `src/output/mod.rs` | 4 | 修改 | ~30 |
| `src/output/markdown.rs` | 4 | 修改 | ~15 |
| `src/commands.rs` | 1 | 修改 | ~10 |
| `.opencode/plugins/repo-wiki.ts` | 1, 6 | 修改 | ~150 |
| `README.md` | 0 | **新建** | ~80 |
| `tests/test_plan.rs` | 3 | **新建** | ~80 |
| `tests/test_multilang.rs` | 4 | **新建** | ~60 |
| `tests/test_protected_files.rs` | 5 | **新建** | ~50 |
| `PLAN.md` | — | **本文件** | — |

---

## 风险与边界

| 风险 | 缓解 |
|------|------|
| wiki_plan.yaml 解析失败不应阻止生成 | plan.enabled=false 时跳过，enabled=true 时 parse error 抛异常 |
| `expand_languages` 会放大 LLM 调用数 | 文档中注明按需启用，默认空 |
| 人工修改保护依赖 SHA256，用户重命名文件丢失保护 | output.dir 内文件路径为 key，重命名 = 新路径 = 无保护，合理 |
| `/knowledge` 命令的 LLM 调用失败 | 插件端 try/catch + 错误提示，不破坏已有卡片 |

---

## 验证方法

每个 Phase 后：
```
cargo test                    # 全测试通过
cargo clippy --all-targets    # 无 warning
cargo build                   # 编译成功
```

最终验收：
```
# Phase 0-2: 系统稳定
repo-wiki init
repo-wiki generate
repo-wiki search -q "struct" --json    # 搜索正常

# Phase 3: plan 生效
echo '{"notes": "请重点描述安全設計", "sections": []}' > .repo-wiki/wiki_plan.yaml
repo-wiki generate                       # output 含有安全設計指导

# Phase 4: 多语言
config.toml 中设置 expand_languages=["en"]
repo-wiki generate                       # wiki/zh/ + wiki/en/

# Phase 5: 保护
手动修改 wiki/zh/crate_config.md
repo-wiki update                         # 该文件不被覆盖

# Phase 6: 插件
opencode 中执行 /knowledge generate src/lib.rs   # cards/ 下生成新文件
opencode 中执行 /knowledge modify crate::config "补充说明"  # 卡片被修改
```
