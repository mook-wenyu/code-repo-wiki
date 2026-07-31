# repo-wiki

AI 驱动的代码仓库 Wiki 自动生成工具。分析源码结构，通过 LLM 生成结构化 Wiki 文档，支持全量/增量更新、多引擎搜索和文件变更监听。

## 架构

```
                   ┌──────────┐
                   │  Source   │
                   │   Files   │
                   └─────┬─────┘
                         ▼
              ┌──────────────────┐
              │  ingest (parser) │  tree-sitter AST 解析
              │  scan + parse    │
              └────────┬─────────┘
                       ▼
              ┌──────────────────┐
              │ analysis (graph) │  petgraph 知识图谱
              │ build + detect   │
              └────────┬─────────┘
                       ▼
              ┌──────────────────┐
              │ generate (LLM)   │  OpenAI/Anthropic
              │ cards + wiki     │
              └────────┬─────────┘
                       ▼
              ┌──────────────────┐     ┌──────────────────┐
              │ output (render)  │────▶│  Wiki (Markdown)  │
              │ markdown / html  │     │  + HTML 导出      │
              └────────┬─────────┘     └──────────────────┘
                       ▼
              ┌──────────────────┐
              │ search (index)   │  text / semantic / hybrid
              │ FTS5 + embedding │
              └──────────────────┘
```

## 核心功能

- **代码解析**：基于 tree-sitter，支持 Rust/Python/JavaScript/TypeScript/Go/C#/Java
- **知识图谱**：petgraph 构建实体间（调用/导入/包含/实现）关系图
- **Wiki 生成**：经 LLM（OpenAI/Anthropic）生成模块知识卡片和文档
- **增量更新**：git diff 或文件事件驱动，只重新生成变更模块
- **搜索**：BM25 全文搜索 + 向量语义搜索 + RRF 混合排序
- **文件监听**：`watch` 子命令实时监听文件变更，自动增量更新
- **HTML 导出**：将 Wiki 导出为静态 HTML 站点
- **OpenCode 插件**：可注册为 OpenCode 插件，在对话中查询代码

## 子命令

| 命令 | 用途 |
|------|------|
| `generate` | 全量生成 Wiki 文档（支持 `-o` 输出覆盖、`--force` 强制重写、`--progress-json` 进度输出）|
| `update` | 增量更新（基于 git diff，支持 `-o` 输出覆盖）|
| `status` | 查看 Wiki 状态 |
| `export` | 导出为 HTML |
| `init` | 初始化配置文件 |
| `watch` | 监听文件变更并自动更新 |
| `search` | 搜索代码实体 |
| `card` | 知识卡片操作（generate/modify/supplement/rewrite，对应 Qoder `/knowledge`）|
| `install-to-opencode` | 注册为 OpenCode 插件 |
| `uninstall-from-opencode` | 移除 OpenCode 插件（需 `--force`）|

## 技术栈

- **语言**：Rust (edition 2024)
- **解析**：tree-sitter（Rust/Python/JS/TS/Go/C#/Java）
- **图结构**：petgraph (StableDiGraph)
- **存储**：rusqlite (SQLite FTS5)
- **LLM**：OpenAI / Anthropic / 兼容 API
- **嵌入**：text-embedding-3-small 等
- **CLI**：clap（derive 模式）
- **文件监听**：notify + debouncer

## 安装与使用

```bash
# 编译安装
cargo build --release

# 初始化配置
repo-wiki init

# 全量生成 Wiki
repo-wiki generate

# 增量更新
repo-wiki update

# 搜索代码实体
repo-wiki search --query "fn_name"

# 监听文件变更
repo-wiki watch

# 知识卡片操作（Qoder /knowledge 对等）
repo-wiki card modify config_plan --instruction "补充错误处理说明"
```

## 生成干预（wiki_plan.yaml）

仓库根放置 `wiki_plan.yaml`（随 Git 提交共享），可干预 LLM 生成方向：

```yaml
version: 1
notes: "请重点描述安全设计"        # 全局引导提示（追加到所有 system prompt）
knowledgecard:
  notes: "卡片请注明编码规约"       # 知识卡片专用提示
scope:
  include: ["src/**"]             # 覆盖扫描范围（优先于 config.toml scope）
  exclude: []
sections:                          # 模块级规划（按模块路径 glob 匹配）
  - module_pattern: "src/config/**"
    template_type: "api-ref"       # architecture / prd / api-ref
    notes: "重点列出接口签名与参数"
documents:                         # 页面白名单（提供时严格只输出列出的页面）
  - title: "src::config"
    goal: "介绍配置系统"
    parent: ""
    hints: ""
```

修改后需手动触发 `generate` 才生效。`notes` 追加到 system prompt 末尾；模块级 `sections` 按模块路径匹配（支持 `src/config/**` 与 `src::config` 两种形态）；`documents` 白名单过滤输出页面集合。

## 限制项

- 单项目最多扫描 10,000 个文件，超限显式报错
- 增量更新仅支持 Git 仓库（非 Git 目录自动回退全量生成）
- 单次变更超过 10,000 行自动回退全量生成

## 人工修改保护

`update`（增量）与 `generate` 生成前会比对磁盘文档与上次生成时记录的 SHA256 指纹：人工修改过的文档自动加入保护集，后续更新不覆盖（保护集记录于 `.repo-wiki/.state/generation_state.json`）。使用 `generate --force` 清空保护集强制重写。

## 配置

```toml
[wiki]
template = "architecture"
language = "zh"

[scope]
include = ["src/**"]
exclude = ["**/test/**", "target/**"]

[llm]
provider = "openai"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"

[embed]
enabled = false  # 启用后开启语义搜索
model = "text-embedding-3-small"

[search]
enabled = true
default_engine = "hybrid"

[incremental]
enabled = true
strategy = "git-diff"
```

## 搜索

repo-wiki 提供三种搜索引擎：

- **text**：SQLite FTS5 BM25 全文搜索，无需额外依赖
- **semantic**：基于向量嵌入的语义搜索（需 `[embed]` 配置且 `enabled = true`）
- **hybrid**：RRF 算法融合全文与语义结果，`k=60`

CLI 使用：`repo-wiki search --query "keyword" --engine hybrid --top-k 10`
