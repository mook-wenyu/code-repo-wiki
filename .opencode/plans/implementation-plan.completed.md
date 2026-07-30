# repo-wiki 全面实现计划

> 基于业务分析报告 + 代码审计 + 新需求，覆盖 P0/P1/P2/P3 四级的修复和新功能。

## 领域词汇表（计划前锚定）

| 术语 | 定义 |
|------|------|
| **InSight** | 单文件解析结果：AST 实体列表 + 导入列表 |
| **CodeGraph** | petgraph `StableDiGraph<CodeNode, CodeEdge>` 知识图谱 |
| **Chunk** | 按模块聚类的实体分组，传递给 LLM 生成卡片 |
| **KnowledgeCard** | LLM 生成的模块摘要（JSON 格式） |
| **WikiDocument** | 单个 wiki 页面，包含 LLM 渲染的 Markdown |
| **Embedding** | 代码/文档的向量表示，用于语义搜索和相似度匹配 |
| **IncrementalState** | 持久化的生成状态（commit_hash + 文件指纹） |
| **ImpactPropagation** | 变更文件 → 依赖传递 → 受影响模块集 |

---

## 优先级总览

```
Phase 0: 死代码清理        (低风险，先清理再动其他)
Phase 1: 配置增强 + Embed   (新功能，需要支撑其他层)
Phase 2: P0 灾难性修复      (数据损坏，必须立即修)
Phase 3: P1 严重修复        (功能损坏+性能)
Phase 4: 架构改进           (P2/P3 优化)
Phase 5: 测试覆盖           (补齐测试，防止回归)
Phase 6: 领域模型文档        (ADR + CONTEXT.md)
```

---

## Phase 0: 死代码清理

### 0.1 删除 Java 解析器
- **文件**: `Cargo.toml:24` — 删除 `tree-sitter-java = "0.23"`
- **证据**: 该依赖声明但 `src/ingest/parser/` 中没有 `java.rs`，`parser/mod.rs` 中没有 `JavaProcessor` 注册
- **操作**: 单行删除

### 0.2 删除 `TypeScriptProcessor.js_lang` 死字段
- **文件**: `src/ingest/parser/typescript.rs:13,23`
- **问题**: `js_lang` 被设为 `ts_lang.clone()` 且从未使用
- **操作**: 删除该字段

### 0.3 删除所有 parser 的 `Mutex<Parser>` 死代码
- **文件**: `rust.rs:9-14`、`typescript.rs`、`javascript.rs`、`python.rs`、`go.rs`、`csharp.rs`
- **问题**: 每个 parser 的 `parser: Mutex<Parser>` 都有 `#[allow(dead_code)]`，且实际解析从未使用
- **操作**: 删除 Mutex 字段、对应 `#[allow]`、以及构造中的 `Self { parser, language }` 赋值

### 0.4 删除 `crossref.rs:title_map` (dead_code)
- **文件**: `src/output/crossref.rs:19`
- **操作**: 若 `title_map` 仅用于 `#[allow(dead_code)]` 的 self 字段，删除

### 0.5 删除 `edge.rs:is_strong()` (从未调用)
- **文件**: `src/model/edge.rs:55`
- **操作**: 删除方法

### 0.6 删除 `card.rs:124-128` 死分支
- **文件**: `src/generate/card.rs:124-128`
- **问题**: `module_type` 两个分支都返回 `"module"`，逻辑无效
- **操作**: 用常量 `"module"` 替换整个条件

### 0.7 移除 `analysis/mod.rs:73` + `module.rs:313` 同义反复断言
- **文件**: `src/analysis/mod.rs:73`、`src/analysis/module.rs:313`
- **问题**: `assert!(x.is_empty() || !x.is_empty())` 永真式
- **操作**: 替换为有意义的断言（至少 `assert!(!x.is_empty())`）

---

## Phase 1: 配置增强 + Embedding

### 1.1 LLM 配置增加 `api_key` 直传字段

**目标**: 当前只有 `api_key_env`（环境变量名），需增加 `api_key: Option<String>` 直接指定。

**变更文件**: `src/config/schema.rs`

```rust
pub struct LlmSection {
    pub provider: LlmProviderType,
    pub model: String,
    pub base_url: Option<String>,        // 已存在
    pub api_key: Option<String>,         // 新增：直接指定 key，优先级高于 api_key_env
    pub api_key_env: String,             // 已存在：环境变量名
    pub max_concurrent: usize,           // 已存在
    pub max_tokens: Option<u32>,         // 新增：输出最大 token 数
    pub temperature: Option<f32>,        // 新增：生成温度
}
```

**解析优先级**: `api_key` > `env::var(api_key_env)` > 报错

### 1.2 新增 Embedding 配置段

**目标**: 支持向量嵌入模型，为 RAG/语义搜索提供基础配置。

**新增文件**: `src/config/schema.rs` 追加

```rust
/// 嵌入模型提供商类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EmbedProviderType {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "custom")]
    Custom,
}

/// 嵌入模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSection {
    pub enabled: bool,
    pub provider: EmbedProviderType,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: String,
    pub batch_size: usize,           // 批处理大小
    pub dimension: Option<usize>,    // 向量维度（部分模型需要指定）
}

impl Default for EmbedSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: EmbedProviderType::OpenAI,
            model: "text-embedding-3-small".to_string(),
            base_url: None,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            batch_size: 20,
            dimension: None,
        }
    }
}
```

`WikiConfig` 增加 `embed: EmbedSection` 字段。

### 1.3 实现 Embedding 引擎

**目标**: 创建 `src/generate/embed.rs` 实现 OpenAI 兼容的 embedding API 调用。

```rust
// src/generate/embed.rs
//! Embedding 引擎：将代码/文档文本转为向量表示

use crate::config::schema::EmbedSection;

/// Embedding 引擎
pub struct EmbeddingEngine {
    client: reqwest::Client,
    config: EmbedSection,
    call_count: AtomicUsize,
}

impl EmbeddingEngine {
    pub fn new(config: &EmbedSection) -> Result<Self>;
    
    /// 批量将字符串列表转为向量
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    
    /// 单文本转向量
    pub fn embed(&self, text: &str) -> Result<Vec<f32>>;
    
    /// 计算余弦相似度
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32;
    
    /// 调用次数
    pub fn call_count(&self) -> usize;
}
```

**验证**: `cargo test test_embed_basic` — 在 `MockEmbedProvider` 上测试向量形状和余弦相似度。

### 1.4 Embedding 集成到流水线

**目标**: 在 `src/generate/mod.rs` 中新增可选的 embedding 生成步骤。

- 配置 `embed.enabled = true` 时，生成 wiki 后再生成向量索引
- 生成知识卡片级别的向量（卡片内容 embedding）和文件级别的向量
- 输出到 `.repo-wiki/.vectors/` 目录（bincode 存储）

**验证**: 运行 `generate` 后检查 `.repo-wiki/.vectors/` 目录是否存在向量文件。

---

## Phase 2: P0 灾难性修复

### 2.1 `chunk.rs:47-61` — NodeId → file_path 映射

**根因**: `module.node_ids.iter().any(|_| file_path_str.contains(&module.name))` 中的 `|_|` 忽略 node_id。

**修复**:
1. `Module` 结构体增加 `file_to_node: HashMap<String, NodeId>` 预计算映射
2. `chunk_by_module` 中使用 `insight.file_path` 查 `file_to_node` 而非子串包含
3. 移除 `ponytail` 注释

**验证**: 测试：两个模块 `http` 和 `http_server` 的文件能正确分配到各自模块。

### 2.2 `graph.rs:207-228` — 导入边使用全局名称匹配

**根因**: `name_map: HashMap<String, NodeId>` 全局匹配，多模块同名实体产生虚假边。

**修复**:
1. 将 `name_map` 改为 `HashMap<String, Vec<NodeId>>`（同名可能有多个节点）
2. 匹配时根据导入路径的最长前缀消歧：
   - `use std::collections::HashMap` → 优先匹配 `std::collections::HashMap` 路径中的 `HashMap`
   - 如果多个节点有相同的短名称，选择路径前缀匹配度最高的

**验证**: 测试：同名 `Config` 分别在 `config::Config` 和 `web::Config` 中时，导入边正确指向目标模块。

### 2.3 `dependency.rs:69-122` — 环检测错误

**根因**: `visited.insert(current)` 阻止路径重用 → 错过非起始节点的环。

**修复**:
1. 使用 petgraph 内置的 `algo::kosaraju_scc()` 检测强连通分量（SCC）
2. SCC 大小 > 1 的组件即有环
3. 或者使用 Floyd 算法检测路径上的节点重入

**验证**: 测试 `A→B→C→B`（B 重入）和 `A→B→C→A`（标准环）都能被检测到。

### 2.4 `graph.rs:277-292` — `parse_impl_target` 提取错误名称

**根因**: `impl_item` handler 提取 `type` 字段而非 `trait` 字段 → `impl Display for i32` 的名称是 `i32`。

**修复**:
1. 若 `impl_item` 同时有 `trait` 和 `type` 子字段，提取 `trait` 作为实现关系
2. 实体名保留 `type` 字段的实际类型名
3. 创建 `Implements` 边从 `type` 指向 `trait`

**验证**: 测试 `impl Display for i32` 的边方向：`i32 → Display`。

### 2.5 `card.rs:56` — `_max_concurrent` 被忽略

**根因**: `join_all(handles)` 同时驱动所有 future。

**修复**:
1. 使用 `tokio::sync::Semaphore` 限制并发
2. `generate_all_cards` 中初始化 `Semaphore::new(max_concurrent)`，每个卡生成前 `acquire().await`

**验证**: 测试：10 个块、`max_concurrent=3`，并行度不超过 3。

### 2.6 `prompt.rs/chunk.rs` — 源代码不传递给 LLM

**目标**: Chunk 中包含源代码片段，prompt 中传递给 LLM。

**修复**:
1. `EntitySummary` 增加 `source_code: Option<String>`
2. 解析器 `extract()` 时截取实体对应的源代码行（从 AST 的 `start_byte` 到 `end_byte`）
3. `Chunk` 中增加 `source_codes: Vec<(String, String)>`（文件名 + 代码）
4. prompt 模板中新增 `代码片段` 章节

**注意**: 按 YAGNI，只传递文件路径和源代码行范围，LLM 需要源文件时才提供片段。默认最大 200 行/实体，超长截断。

**验证**: 测试：解析后 `EntitySummary.source_code` 不为空、包含函数体的关键行。

### 2.7 `diff.rs:48-57` — 增量更新使用 `HEAD^` 而非 `last_commit_hash`

**根因**: `diff.rs` 硬编码 `HEAD^`，`incremental/mod.rs:88` 加载但未使用 `last_commit_hash`。

**修复**:
1. `run_git_diff_incremental` 接受 `from_commit: &str` 参数
2. 替换 `HEAD^` 为 `from_commit` 或 `format!("{}~1", from_commit)` 的变体
3. 传递 `state.commit_hash` 作为参数

**验证**: 测试：两次提交间增量运行，第一次生成后提交，第二次增量只处理最新提交的变更。

### 2.8 同义反复断言修复（见 Phase 0.7）

---

## Phase 3: P1 严重修复

### 3.1 `main.rs:73` — Generate 的 `output` 参数被忽略

**修复**: 将 `output` 传递给 `render_all` 作为输出目录，覆盖 `config.output.dir`。

### 3.2 `graph.rs:294-332` — 调用边子串匹配产生虚假边

**根因**: `format!("{}(", name)` 子串搜索产生假阳性。

**修复**:
1. 改用 tree-sitter 语法树中的调用表达式
2. 若无法获得 AST 调用信息，使用基于源代码行的精确匹配（仅在同一文件中匹配 `callee_name` + 在同一行的位移内）
3. 排除注释行中的匹配

### 3.3 `chunk.rs:68-72` — dependencies = 所有其他模块

**根因**: `filter(|m| m.name != module.name).map(|m| m.name)` 将"不是自身"等价于"依赖"。

**修复**:
1. 改为分析 `Insight.imports` 数据
2. 从 imports 中解析 `use/import/using` 目标模块
3. 只有当模块 A import 了模块 B 的实体时，B 才出现在 A 的依赖列表中

### 3.4 `html.rs` — Mermaid ID 编码不一致 & CSS 路径

**修复**:
1. 将 `html.rs` 的 Mermaid ID 生成统一为 `mermaid.rs::sanitize_id`
2. CSS 路径改为动态计算（根据文档层级深度）

### 3.5 `python.rs:59` — 函数签名在 `':'` 上分割

**根因**: `split(':')` 会切到类型注解 `name: str`。

**修复**: 改为 `split('(').next()` 提取函数名，签名用 tree-sitter AST 的 `signature` 节点全文。

### 3.6 `python.rs:100-119` — 多行文档字符串忽略

**根因**: 仅处理同行的 `"""doc"""`。

**修复**: 修改 `associate_docstrings` 以支持跨行 `"""..."""` 和 `'''...'''`，以及去掉缩进的 `inspect.cleandoc` 等效逻辑。

### 3.7 `unity.rs:96-101` — 生命周期方法不检查 MonoBehavior 继承

**修复**: 在标记 `unity-lifecycle` 之前检查类的父类/接口列表是否包含 `MonoBehaviour`。

### 3.8 `watch.rs:121` — `config.scope.include[0]` 空列表 panic

**修复**: 使用 `config.scope.include.first().context("scope.include 不能为空")?` 返回有意义错误。

### 3.9 `impact.rs:59+79` — 入边被遍历两次

**修复**: 移除重复的 `edges_directed(current, Incoming)` 遍历，`edges(current)` 已包含所有边。

### 3.10 `module.rs:38` — 硬编码深度 1-3

**修复**: 深度改为可配置（`scope.depth: Option<usize>`），默认 `Some(5)`。

### 3.11 `heat` items from P1 table (functionality broken)

批量修复 `main.rs output`、`prompt.rs docstring truncation`、`html.rs Mermaid ID` 等。

---

## Phase 4: 架构改进

### 4.1 Provider 开放封闭原则重构 (`llm.rs:55-85`)

**当前**: `Provider` 枚举 + `match` 分发，新增 provider 需改 4 处。

**改进**: 提取 `LlmProvider` trait 方法到独立的 `dyn LlmProvider`，用 `Box<dyn LlmProvider>` 替代枚举。

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;
    async fn complete(&self, messages: &[Message]) -> Result<String>;
    fn call_count(&self) -> usize;
}
```

移除 `Provider` 枚举，改为 `Box<dyn LlmProvider>`。

### 4.2 解析器共享 walk 实现

**当前**: 6 个解析器各写一次 DFS 树遍历（约 51 行重复代码）。

**改进**: 在 `parser/mod.rs` 中提供 `TreeWalker` 工具：

```rust
// 统一树遍历
pub fn walk_tree(cursor: &mut TreeCursor, visit: &mut dyn FnMut(&Node)) {
    visit(&cursor.node());
    if cursor.goto_first_child() {
        loop {
            walk_tree(cursor, visit);
            if !cursor.goto_next_sibling() { break; }
        }
        cursor.goto_parent();
    }
}
```

每个解析器的 `extract()` 只需按节点类型分发处理函数。

### 4.3 解析器缓存

**当前**: 每调用 `parse()` 创建新 `Parser`，Mutex 死代码。

**改进**: `Mutex<Parser>` 改为 `ParserPool`，预先创建 N 个 `Parser` 从池中借用，避免重复初始化 tree-sitter 语言。

### 4.4 共享 tokio 运行时

**当前**: 每次 `run_pipeline()` 新建 `Runtime::new()`。

**改进**: `lib.rs` 中创建全局 lazy `TOKIO_RUNTIME`，复用。

### 4.5 并行扫描

**当前**: `ingest/mod.rs` 顺序解析文件。

**改进**: 使用 `rayon::par_iter()` 并行解析，通过 `Mutex<ParserRegistry>` 保护 parser 访问。

---

## Phase 5: 测试覆盖

### 5.1 新增测试清单

| 模块 | 测试场景 | 优先级 |
|------|---------|--------|
| `prompt.rs` | 空实体、单实体、超长实体、多行文档字符串 | P0 |
| `diff.rs` | 真实 git repo 上的添加/修改/删除文件、重命名、多次提交 | P0 |
| `impact.rs` | 深度传播(2/3/4层)、空图、单节点图、环图 | P0 |
| `card.rs` | `max_concurrent` 限制验证、MockProvider E2E、空块 | P1 |
| `mermaid.rs` | `render_entity_graph`、含3模块+5边的图 | P1 |
| `html.rs` | `export_html` 端到端输出验证 | P1 |
| `watch.rs` | 临时目录上的文件创建/修改/删除事件 | P1 |
| `chunk.rs` | `chunk_by_module` 文件→模块映射、依赖正确性 | P1 |
| `graph.rs` | 同名实体导入解析、impl 块边方向 | P1 |
| `scanner.rs` | 无效 glob 模式、深层嵌套目录、unicode 路径 | P2 |
| `python.rs` | 多行文档字符串、装饰器、`async def`、类型注解签名 | P2 |
| `csharp.rs` | 空文件、仅 using 文件、record、partial class | P2 |
| `config/opencode.rs` | 并行测试隔离（已有但可改进） | P2 |
| `config/mod.rs` | 无效 TOML、缺失字段、glob 语法验证 | P2 |

### 5.2 防回归机制

1. 每个 P0/P1 修复先写**失败测试**（证明 bug 存在），再修复
2. `cargo test` 必须全部通过
3. `cargo clippy` 零 warning

---

## Phase 6: 领域文档

### 6.1 `CONTEXT.md`

```
# repo-wiki 领域词汇表

## 核心概念
- **Insight**: 单文件的 AST 分析结果（实体列表+导入列表）
- **CodeGraph**: 基于 petgraph 的知识图谱，节点=代码实体/文件，边=关系
- ...

## 边界
- 分析层只读访问解析层输出
- 生成层只依赖分析层结果（不直接访问解析层）
- ...
```

### 6.2 ADR

| ADR | 决策 |
|-----|------|
| 0001 | 使用 petgraph `StableDiGraph` 而非自定义图 |
| 0002 | 解析器输出统一为 `LanguageProcessor` trait |
| 0003 | 生成层使用 `Box<dyn LlmProvider>` 替代枚举分发 |

---

## 执行顺序图

```
Phase 0 ───→ Phase 1 ───→ Phase 2 ───→ Phase 3 ───→ Phase 4 ───→ Phase 5
(清理)       (配置)       (P0 修复)     (P1 修复)     (架构)       (测试)
   │            │            │             │             │
   └── dead     ├── LLM      ├── chunk     ├── output    ├── Provider trait
       code         config       mapping       fixes         + OCP
       removal   ├── Embed    ├── graph     ├── parser    ├── Shared
                  engine       import         fixes        walk
                             ├── cycle     ├── impact    ├── Global
                                detection     perf         runtime
                             ├── impl fix
                             ├── concurrency
                             ├── source code
                             ├── incremental
```

每个 Phase 内可并行：Phase 0 的 7 个子项互不依赖；Phase 2 的 8 个 P0 修复除 2.8 依赖 Phase 0.7 外全部独立实现。

---

## 验收标准

1. `cargo build` 零警告
2. `cargo test` 全部通过（>=80 个测试）
3. `cargo clippy` 零警告
4. 移除 Java parser 后 Cargo.toml 无 `tree-sitter-java`
5. 配置支持 `[llm] api_key` 和 `[embed]` 段
6. Embedding 引擎能调用 OpenAI 兼容 API 并返回正确形状的向量
7. 8 个 P0 bug 全部修复并有覆盖性测试
