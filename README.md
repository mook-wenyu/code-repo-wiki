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
              │ analysis (graph) │  petgraph 知识图谱 + leiden 社区检测
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
- **知识图谱**：petgraph 构建实体间（调用/导入/包含/实现）关系图，leiden-rs 社区检测划分模块
- **Wiki 生成**：经 LLM（OpenAI/Anthropic）生成模块知识卡片和文档
- **增量更新**：git diff 或文件事件驱动，实体级变化分类（新增/删除/签名变更/正文修改）驱动语义传播，只重新生成受影响模块
- **搜索**：BM25 全文搜索 + 向量语义搜索 + RRF 混合排序
- **文件监听**：`watch` 子命令实时监听文件变更，自动增量更新
- **HTML 导出**：将 Wiki 导出为静态 HTML 站点
- **OpenCode 插件**：可注册为 OpenCode 插件，在对话中查询代码

## 子命令

| 命令 | 用途 |
|------|------|
| `generate` | 全量生成 Wiki 文档（支持 `-o` 输出覆盖、`--force` 强制重写、`--progress-json` 进度输出、`--root` 项目根）|
| `update` | 增量更新（基于 git diff，支持 `-o` 输出覆盖、`--root` 项目根）|
| `sync` | 以 Git 工作区内容同步指纹库（不触发 LLM 生成）|
| `status` | 查看 Wiki 状态 |
| `lint` | 产物健康检查（孤儿页/断链/过时），供 CI 使用；发现问题退出码非 0 |
| `export` | 导出为 HTML（支持 `-o` 输出目录、`--skip-generate` 从快照直接导出不重新生成）|
| `init` | 初始化配置文件 |
| `watch` | 监听文件变更并自动更新 |
| `search` | 搜索代码实体 |
| `ast-search` | AST 精确符号查找（文件+行号+签名，不依赖搜索索引）|
| `card` | 知识卡片操作（generate/modify/supplement/rewrite，对应 Qoder `/knowledge`）|
| `note` | 知识沉淀记录（追加到 `_log.md`）|
| `install-to-opencode` | 注册为 OpenCode 插件 |
| `uninstall-from-opencode` | 移除 OpenCode 插件（需 `--force`）|
| `install-wiki` | 在项目根 AGENTS.md 注入 Wiki 使用引导块（`<!-- REPO-WIKI:START/END -->`，`--also-claude` 双写 CLAUDE.md）|
| `uninstall-wiki` | 移除 AGENTS.md 中的 Wiki 引导块（未安装时提示并退出 0）|
| `mcp` | 启动 MCP (Model Context Protocol) stdio server（Claude Code/Cline 等客户端接入）|
| `bench` | 自动评测仓库 Wiki 质量（Coverage/Doc Info/lint/Update Recall/Time 五维 + `--judge` TQS 裁判打分）|

`generate`/`update`/`sync`/`status`/`lint`/`export`/`init`/`watch`/`search`/`ast-search`/`card`/`install-to-opencode`/`uninstall-from-opencode`/`install-wiki`/`uninstall-wiki`/`mcp` 支持 `--root` 指定项目根（扫描根/git 定位基准，默认当前目录）。

## 技术栈

- **语言**：Rust (edition 2024)
- **解析**：tree-sitter（Rust/Python/JS/TS/Go/C#/Java）
- **图结构**：petgraph 0.8 (StableDiGraph) + leiden-rs（社区检测）
- **存储**：rusqlite (SQLite FTS5 全文索引 + sqlite-vec vec0 向量表)
- **LLM**：OpenAI / Anthropic / 兼容 API（含超时重试、流式解析）
- **嵌入**：text-embedding-3-small 等（embedding 注入特征聚类）
- **CLI**：clap（derive 模式）
- **文件监听**：notify + debouncer

## 安装与使用

### 前置依赖（Linux / macOS）

repo-wiki 在 **Windows 上为单二进制免系统依赖**（TLS 用系统 schannel）；
Linux/macOS 上构建与运行需要 OpenSSL 与 zlib 开发库（`reqwest` 的
`native-tls` 与 `git2` 的 `libgit2-sys` 动态链接它们，`libssh2` 的 SSH
传输已内嵌 vendored，不额外要求）：

- Debian/Ubuntu：`sudo apt install libssl-dev zlib1g-dev`
- RHEL/Fedora：`sudo dnf install openssl-devel zlib-devel`
- macOS：`brew install openssl`（Homebrew 的 openssl 不在默认搜索路径时，
  设置环境变量 `OPENSSL_ROOT_DIR` 指向其安装前缀再构建）

### 编译安装

```bash
# 安装到 ~/.cargo/bin（推荐，自动加入 PATH）
cargo install --path . --locked

# 或仅构建（二进制位于 target/release/repo-wiki）
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
- 同一输出目录并发运行 repo-wiki 不被支持：状态/快照/缓存文件无锁，最后写入者胜（CI/编辑器/插件集成请串行调用）

## 人工修改保护

`update`（增量）与 `generate` 生成前会比对磁盘文档与上次生成时记录的 SHA256 指纹：人工修改过的文档自动加入保护集，后续更新不覆盖（保护集记录于 `.repo-wiki/.state/generation_state.json`）。使用 `generate --force` 清空保护集强制重写。

## 配置

### 配置加载链（全局/项目级）

不带 `--config` 时按以下链解析配置文件（v13 E 组）：

1. **项目级**：`{root}/.repo-wiki/config.toml`（root 为当前目录或 `--root` 指定）
2. **全局（用户级）**：Windows `%APPDATA%\repo-wiki\config.toml`，其他平台 `~/repo-wiki/config.toml`
3. **创建**：两者都不存在时自动创建全局目录与默认配置（引导式，无需先手动 init）

显式 `--config <path>` 指定时原样使用（缺失则报错，不走创建链）。

```toml
[wiki]
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

## 多语言输出

配置 `[wiki] expand_languages` 可为扩展语言**独立生成** Wiki 页面（LLM 每种语言各生成一次，非翻译）：

```toml
[wiki]
language = "zh"
expand_languages = ["en"]  # 增加英文独立生成
```

- **独立生成而非翻译**：每种语言由 LLM 单独生成，LLM 调用成本与耗时随语言数量线性增长，扩展语言多意味着生成时间成倍增加
- **卡片仅主语言生成一次**：KnowledgeCard 是给 Agent 读取的结构化数据，跨语言共享，只按主语言生成（存放于 `cards/{主语言}/`）
- **输出结构**：`wiki/{lang}/{module}.md`、`wiki/{lang}/api.md` 每种语言各自生成；`overview.md` 仅写入主语言目录
- **默认关闭**：`expand_languages` 默认为空数组，行为与单语言完全一致

## 搜索

repo-wiki 提供三种搜索引擎：

- **text**：SQLite FTS5 BM25 全文搜索，无需额外依赖
- **semantic**：基于向量嵌入的语义搜索（需 `[embed]` 配置且 `enabled = true`）
- **hybrid**：RRF 算法融合全文与语义结果，`k=60`

CLI 使用：`repo-wiki search --query "keyword" --engine hybrid --top-k 10`
