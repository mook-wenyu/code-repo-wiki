# OpenCode Repo Wiki 实现计划

## 目的地

构建一个 **Rust CLI 核心引擎 + TypeScript OpenCode 插件** 的混合架构系统，能够：
- 自动分析代码仓库结构，构建知识图谱
- 通过 LLM 生成结构化项目文档（Wiki 页面 + Knowledge Card）
- 基于 Git diff 实现增量更新
- 在 OpenCode 中通过命令/工具无缝使用

## 架构决策

### 核心架构：双层混合

```
┌─────────────────────────────────────────────────────────┐
│  OpenCode Plugin (TypeScript)                            │
│  • 注册 /wiki 命令（generate, update, status, export）    │
│  • 注册 Agent 工具（query_wiki, get_module_info）         │
│  • 调用 Rust CLI 二进制                                   │
│  • 展示进度和结果                                         │
└────────────────────────┬────────────────────────────────┘
                         │ shell exec / HTTP
┌────────────────────────▼────────────────────────────────┐
│  repo-wiki CLI (Rust)                                    │
│  四阶段流水线：                                           │
│  1. Ingestion（扫描 + AST 解析）                          │
│  2. Analysis（知识图谱 + 模块聚类）                        │
│  3. Generation（LLM 分层生成）                            │
│  4. Output（Markdown 渲染 + 交叉引用）                    │
└─────────────────────────────────────────────────────────┘
```

**选择理由**：
- OpenCode 插件只支持 TS/JS → 必须用 TS 写交互层
- Rust 提供高性能 AST 解析和图计算 → 核心引擎用 Rust
- deepwiki-rs (Litho) 验证了此模型可行性（MIT 协议）

### 知识模型：双层产物（参考 Qoder）

| 产物 | 消费者 | 特征 |
|------|--------|------|
| Knowledge Card | AI Agent | 短、密、结构化、可检索 |
| Wiki Page | 人类开发者 | 叙事性、架构图、代码引用 |

### 存储结构

```
<project-root>/
└── .repo-wiki/
    ├── config.toml          ← 用户配置
    ├── state.json           ← 生成状态（commit hash、模块指纹）
    ├── graph/
    │   └── repo-graph.bin   ← 序列化的知识图谱
    ├── cards/               ← Knowledge Cards（给 Agent）
    │   ├── _index.json
    │   ├── module-<name>.md
    │   └── ...
    └── wiki/                ← Wiki Pages（给人）
        ├── _toc.md          ← 目录
        ├── overview.md
        ├── architecture.md
        ├── modules/
        │   ├── <module-name>.md
        │   └── ...
        └── assets/
            └── diagrams/    ← Mermaid 图表
```

## 技术栈

### Rust 核心引擎

```toml
[dependencies]
# AST 解析
tree-sitter = "0.25"
tree-sitter-rust = "0.23"
tree-sitter-python = "0.23"
tree-sitter-javascript = "0.23"
tree-sitter-typescript = "0.23"
tree-sitter-go = "0.23"
tree-sitter-java = "0.23"

# 图结构
petgraph = "0.7"

# Git
git2 = "0.20"

# LLM
async-openai = "0.27"
tokio = { version = "1", features = ["full"] }

# CLI
clap = { version = "4", features = ["derive"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# Markdown
pulldown-cmark = "0.12"

# 并行
rayon = "1.10"

# 错误处理
anyhow = "1"
thiserror = "2"

# 日志
tracing = "0.1"
tracing-subscriber = "0.3"
```

### TypeScript 插件

```json
{
  "dependencies": {
    "@opencode-ai/plugin": "latest"
  }
}
```

## 模块设计（Rust 核心引擎）

```
src/
├── main.rs              ← CLI 入口（clap）
├── lib.rs               ← 库根
├── config/
│   ├── mod.rs           ← 配置加载和验证
│   └── schema.rs        ← 配置数据模型
├── ingest/
│   ├── mod.rs           ← 扫描编排
│   ├── scanner.rs       ← 文件系统遍历 + gitignore 过滤
│   └── parser/
│       ├── mod.rs       ← LanguageProcessor trait + 注册表
│       ├── rust.rs      ← Rust 语言处理器
│       ├── python.rs    ← Python 语言处理器
│       ├── typescript.rs← TS/JS 语言处理器
│       └── go.rs        ← Go 语言处理器
├── analysis/
│   ├── mod.rs           ← 分析编排
│   ├── graph.rs         ← 知识图谱构建（petgraph）
│   ├── module.rs        ← 模块边界检测（社区检测算法）
│   └── dependency.rs    ← 依赖关系提取
├── generate/
│   ├── mod.rs           ← 生成编排
│   ├── llm.rs           ← LLM Provider trait + 实现
│   ├── chunk.rs         ← AST 感知分块
│   ├── prompt.rs        ← Prompt 模板管理
│   ├── card.rs          ← Knowledge Card 生成器
│   └── wiki.rs          ← Wiki Page 生成器
├── output/
│   ├── mod.rs           ← 输出编排
│   ├── markdown.rs      ← Markdown 渲染
│   ├── mermaid.rs       ← Mermaid 图表生成
│   └── crossref.rs      ← 交叉引用链接
├── incremental/
│   ├── mod.rs           ← 增量更新编排
│   ├── diff.rs          ← Git diff 分析
│   ├── impact.rs        ← 语义影响传播
│   └── state.rs         ← 状态持久化
└── model/
    ├── mod.rs           ← 领域模型
    ├── node.rs          ← 代码实体（CodeNode）
    ├── edge.rs          ← 关系类型（CodeEdge）
    └── document.rs      ← 文档模型
```

## 实现阶段

### Phase 1: 基础骨架（CLI + 配置 + 扫描）

**目标**：能扫描仓库、解析文件、输出结构信息

1. **CLI 框架**：clap derive 模式，子命令 `generate`、`update`、`status`、`export`
2. **配置系统**：`config.toml` 加载，支持 include/exclude 规则
3. **文件扫描器**：遍历目录树，尊重 .gitignore，按扩展名分类
4. **LanguageProcessor trait**：
   ```rust
   pub trait LanguageProcessor: Send + Sync {
       fn name(&self) -> &'static str;
       fn extensions(&self) -> &[&str];
       fn parse(&self, source: &str, path: &Path) -> Result<FileInsight>;
   }
   ```
5. **Rust 语言处理器**（首个实现）：提取 struct、fn、trait、impl、use 语句

### Phase 2: 知识图谱构建

**目标**：从 AST 信息构建有向图，检测模块边界

1. **CodeNode / CodeEdge 模型**：
   - 节点：Project → Module → File → Struct/Function/Trait
   - 边：Contains, Calls, Imports, Implements, DependsOn
2. **图构建**：从 FileInsight 集合构建 `DiGraph<CodeNode, CodeEdge>`
3. **模块检测**：基于导入关系 + 目录结构的社区检测（简化 Leiden）
4. **图序列化**：bincode 持久化到 `.repo-wiki/graph/`

### Phase 3: LLM 生成引擎

**目标**：基于知识图谱，分层生成 Knowledge Card 和 Wiki Page

1. **LLM Provider trait**：
   ```rust
   #[async_trait]
   pub trait LlmProvider: Send + Sync {
       async fn complete(&self, messages: &[Message]) -> Result<String>;
       async fn complete_stream(&self, messages: &[Message]) -> Result<Pin<Box<dyn Stream<Item = Result<String>>>>>;
   }
   ```
2. **AST 感知分块**：以函数/结构体为边界，附带签名 + 文档 + 依赖上下文
3. **Prompt 模板**：
   - 模块摘要 Prompt（输入：模块内所有实体签名 + 依赖）
   - 架构概览 Prompt（输入：模块列表 + 模块间关系）
   - Knowledge Card Prompt（输入：模块分析 → 输出结构化 YAML/JSON）
4. **分层生成**（Bottom-Up）：
   - Level 0: 每个文件/实体的摘要
   - Level 1: 每个模块的 Knowledge Card + Wiki Page
   - Level 2: 架构概览 + 全局 Wiki 目录
5. **并行调度**：tokio 并发 LLM 调用，rayon 并行 AST 解析

### Phase 4: 输出渲染

**目标**：生成最终 Markdown 文件集

1. **Markdown 渲染器**：模板 + 动态内容组装
2. **Mermaid 图表**：从 petgraph 自动生成模块依赖图、调用链图
3. **交叉引用**：Wiki 页面间的 `[链接](../modules/xxx.md)` + 代码文件引用 `<cite>`
4. **目录生成**：`_toc.md` 自动从页面树生成

### Phase 5: 增量更新

**目标**：基于 Git diff 精准更新受影响的文档

1. **状态记录**：每次生成后记录 commit hash + 每模块文件指纹
2. **变更检测**：`git2` diff 两个 commit，分类 新增/修改/删除
3. **影响传播**：在知识图谱上双向遍历，找到所有受影响模块
4. **选择性重生成**：仅重新生成受影响模块的 Card + Wiki Page
5. **一致性验证**：检查交叉引用完整性

### Phase 6: OpenCode 插件

**目标**：在 OpenCode 中无缝使用 repo-wiki

1. **插件结构**（`.opencode/plugins/repo-wiki.ts`）：
   ```typescript
   import type { Plugin } from "@opencode-ai/plugin"
   export const RepoWikiPlugin: Plugin = async ({ project, client, $, worktree }) => {
     return {
       // 注册自定义工具
       tools: [{
         name: "wiki_query",
         description: "查询项目 Wiki 知识",
         args: { query: { type: "string" } },
         execute: async (args) => { /* 调用 Rust CLI */ }
       }]
     }
   }
   ```
2. **命令注册**：`/wiki generate`、`/wiki update`、`/wiki status`
3. **Agent 工具**：`wiki_query`（语义搜索 Wiki）、`module_info`（获取模块信息）
4. **进度反馈**：通过 OpenCode 事件系统展示生成进度

### Phase 7: 测试与质量保证

1. **单元测试**：每个模块独立测试（parser、graph、chunk、prompt）
2. **集成测试**：对 fixture 仓库执行完整流水线
3. **快照测试**：LLM 输出格式验证（mock provider）
4. **性能基准**：对 1000+ 文件仓库的解析时间 < 30s

## 配置模型 (config.toml)

```toml
[wiki]
template = "architecture"    # architecture | product_requirement
language = "zh"              # 输出语言

[scope]
include = ["src/**", "lib/**"]
exclude = ["**/test/**", "**/vendor/**", "target/**"]

[llm]
provider = "openai"          # openai | anthropic | custom
model = "gpt-4o"
base_url = ""                # 自定义端点
api_key_env = "OPENAI_API_KEY"
max_concurrent = 4           # 并发 LLM 调用数

[output]
dir = ".repo-wiki"
format = "markdown"          # markdown | html

[incremental]
enabled = true
strategy = "git-diff"        # git-diff | file-watch
```

## 关键设计原则

1. **知识图谱一次构建，全生命周期复用**（RepoDoc 核心洞察）
2. **AST 感知分块 > 固定大小分块**（保证语义完整性）
3. **Bottom-Up 分层生成**：实体 → 模块 → 架构（避免上下文洪泛）
4. **语义影响传播**：增量更新只触及真正受影响的文档
5. **插件化语言支持**：`LanguageProcessor` trait 可扩展新语言
6. **Provider 无关**：`LlmProvider` trait 支持任意 OpenAI 兼容 API

## 边界条件

| 场景 | 处理策略 |
|------|---------|
| 空仓库（无源文件） | 报错退出，明确提示 |
| 超大仓库（>10000 文件） | scope 过滤 + 分批处理 + 进度报告 |
| LLM API 失败 | 指数退避重试（3次），失败则跳过并报告 |
| 二进制文件 | 扫描时按扩展名过滤，不进入解析 |
| 循环依赖 | petgraph 检测环，在文档中标注 |
| 无 Git 历史 | 禁用增量更新，仅支持全量生成 |
| 非 UTF-8 文件 | 跳过并记录警告 |

## 参考资源

| 资源 | 价值 |
|------|------|
| [deepwiki-rs (Litho)](https://github.com/sopaco/deepwiki-rs) | Rust 四阶段流水线参考实现 |
| [RepoDoc (arXiv:2604.26523)](https://arxiv.org/abs/2604.26523) | 知识图谱 + 增量更新算法 |
| [codebase-memory-mcp](https://github.com/mcp-codebase) | Tree-sitter + Leiden 模块检测 |
| [OpenCode Plugin Docs](https://opencode.ai/docs) | 插件 API 参考 |
| Qoder Repo Wiki | 双层知识模型设计参考 |

## Out of Scope

- Web UI 文档浏览界面（可用 litho-book 等现有工具）
- RAG 问答系统（本计划只生成文档，不做检索增强问答）
- 多仓库联合分析
- 实时协作编辑 Wiki
- 支持超过 6 种初始语言（后续通过 LanguageProcessor 扩展）
