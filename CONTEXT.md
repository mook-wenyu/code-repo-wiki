# repo-wiki 领域词汇表

## 核心概念

| 术语 | 定义 |
|------|------|
| **Insight** | 单文件解析结果：AST 实体列表 + 导入列表 |
| **Entity** | AST 中的一个具名代码单元（函数/类/结构体/接口/枚举/变量/属性） |
| **CodeGraph** | petgraph 0.8 `StableDiGraph<CodeNode, CodeEdge>` 知识图谱 |
| **CodeNode** | 图中的节点，代表一个实体、文件或模块 |
| **CodeEdge** | 图中的边，代表 Contains/Imports/Calls/Implements 关系 |
| **ModuleCluster** | 文件级社区检测（leiden-rs CPM）产出的模块划分，cohesion/coupling 为描述性元数据 |
| **Feature** | 实体级特征聚类（跨文件协作的方法组），嵌入相似度与调用结构融合聚类 |
| **Chunk** | 按模块聚类的实体分组（含 entity_sources 记录实体归属文件），传递给 LLM 生成卡片 |
| **KnowledgeCard** | LLM 生成的模块摘要（JSON 格式），包含 summary、key_entities、design_patterns |
| **WikiDocument** | 单个 wiki 页面，包含 LLM 渲染的 Markdown 内容 |
| **Embedding** | 代码/文档文本的向量表示，用于语义搜索和相似度匹配 |
| **IncrementalState** | 持久化的生成状态（commit_hash + 文件指纹） |
| **ImpactSet** | 变更文件通过依赖传播影响到的模块集合 |
| **Frontier** | 增量更新时待处理的变更集合 |
| **SymbolEngine** | 基于 tree-sitter query API 的符号级搜索引擎，支持 AST 模式匹配、精确定位 |
| **TextEngine** | 基于 SQLite FTS5 的全文搜索引擎，支持 BM25 关键字检索 |
| **SemanticEngine** | 基于 embedding + SQLite BLOB 向量存储（内存余弦相似度，>0.3 阈值过滤）的语义搜索引擎 |
| **HybridSearch** | RRF（Reciprocal Rank Fusion）融合 BM25 + 语义搜索的混合检索 |
| **AstQuery** | 封装 tree-sitter Query 对象的 AST 查询器，用于精确符号定位和引用追踪 |
| **CodeAgent** | 可自动回溯的搜索 Agent，多轮组合关键词+语义+AST 查询 |
| **CallGraph** | 基于 petgraph 的调用图，支持调用者/被调用者查询和调用链路追踪 |

## 模块层次

| 层 | 职责 | 输入 | 输出 |
|----|------|------|------|
| **ingest** | 文件扫描 + AST 解析 | 源码文件 | `Vec<FileInsight>` |
| **analysis** | 知识图谱构建 + 模块检测 | `Vec<FileInsight>` | `KnowledgeGraph` |
| **generate** | LLM 生成知识卡片和 wiki 页面 | `KnowledgeGraph` + config + `Vec<FileInsight>` | `Vec<KnowledgeCard>` + `Vec<WikiDocument>` |
| **output** | 渲染为 Markdown/HTML + 导出快照 | `Vec<WikiDocument>` + cards + graph | 文件系统上的 wiki 目录 + `.state/export_snapshot.json` |
| **incremental** | 变更检测 + 影响传播 | git diff / 文件事件 | `IncrementalResult` |
| **search** | 代码搜索智能体（符号+文本+语义三引擎） | 查询字符串 + KnowledgeGraph | `Vec<SearchHit>` |

## 边界规则

- **analysis** 层只读访问 **ingest** 层输出
- **generate** 层依赖 **analysis** 层结果与 **ingest** 层 `FileInsight`（经 lib.rs 管线传入，不直接调用 parser 注册表）
- **incremental** 层可跳过 **analysis** 层的全量重建（仅影响传播需图结构）
- **output** 层是纯渲染，不含业务逻辑（不反向依赖 generate——语言列表由本层 `wiki_languages` 自持）
- **search** 层是默认只读的聚合层，可组合访问 ingestion / analysis / generate 三层的索引和数据

## 单进程契约

同一输出目录并发运行 repo-wiki 不被支持：`.state/` 状态文件、导出快照、搜索缓存均无锁，最后写入者胜。CI、编辑器、插件集成必须串行调用（多个进程写同一 `output.dir` 是未定义行为）。
