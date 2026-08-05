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
| `update` | 增量更新（基于 git diff，支持 `-o` 输出覆盖、`--dry-run` 预览变更不执行、`--root` 项目根）|
| `sync` | 以 Git 工作区内容同步指纹库（不触发 LLM 生成）|
| `status` | 查看 Wiki 状态 |
| `lint` | 产物健康检查（孤儿页/断链/过时/引用错位/坏 mermaid），供 CI 使用；退出码三态：`0` 通过、`1` 发现问题、`2` 配置加载失败 |
| `export` | 导出为 HTML（支持 `-o` 输出目录、`--skip-generate` 从快照直接导出不重新生成）|
| `doctor` | 环境诊断（配置/产物目录/输出目录/LLM Key/网络/版本自检六查），失败退出码 1 |
| `init` | 初始化配置（缺省在全局配置目录创建；项目根已有 `.repo-wiki/config.toml` 时跳过不覆盖，`--force` 强制；显式 `--config <path>` 始终覆盖）|
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
| `bench` | 自动评测仓库 Wiki 质量（Coverage/Doc Info/lint/Update Recall/Time 五维 + `--judge` TQS 裁判打分 + `--rubrics-only` 跳过 git 回放）|
| `bench-manifest` | 清单批量跑分：每行一个仓库（本地路径或 git URL），输出仓库×维度矩阵（mock 可跑，不触网）|

`generate`/`update`/`sync`/`status`/`lint`/`export`/`doctor`/`init`/`watch`/`search`/`ast-search`/`card`/`note`/`install-to-opencode`/`uninstall-from-opencode`/`install-wiki`/`uninstall-wiki`/`mcp` 支持 `--root` 指定项目根（扫描根/git 定位基准，默认当前目录）；`bench` 的 `--root` 为必填项（目标评测仓库根，语义与其他子命令不同）。

### 评测（bench / bench-manifest）

- `bench --root <仓库> [--judge] [--rubrics-only]`：五维自动评测。Update Recall 会回放 git commit（reset --hard 工作区，有未提交改动会被安全闸拒绝）；`--rubrics-only` 跳过回放只做裁判层（大仓库评测成本控制，与 `--judge` 互斥）。
- `bench-manifest --manifest <清单> [--config <模板>] [--json] [--work-dir <目录>]`：清单批量跑分。清单每行一个仓库（`#` 注释/空行跳过；本地路径直接使用，`https://`/`git@` 开头克隆到 `--work-dir`）。每个仓库按模板配置 mock/真实生成后测 Coverage/Doc Info/lint/Time，产物输出到 `--work-dir/<仓库名>-out/`（不污染原仓库）；单仓库失败在该行标注，不中断整批。Update Recall 回放与 LLM 裁判在本模式跳过（深评用单仓库 `bench`）。

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

### 编译安装与一键启动

外部 AI Coding Agent 进入新仓库的最短路径（3 条命令，其余命令见下表）：

```bash
# 1. 安装到 ~/.cargo/bin（自动加入 PATH；或仅构建：cargo build --release）
cargo install --path . --locked

# 2. 配置 LLM Key（默认 deepseek-v4-flash，配置链：项目 .repo-wiki/config.toml
#    → 全局配置目录 → init 自动创建）
export DEEPSEEK_API_KEY="sk-..."

# 3. 生成 Wiki（首次零参数全自动：无配置自动创建，产物在 .repo-wiki/）
repo-wiki generate
```

常用命令一览（完整用法见各命令 `--help`）：

| 命令 | 作用 |
|---|---|
| `repo-wiki init` | 创建默认配置（缺省全局目录；已存在跳过，`--force` 重写） |
| `repo-wiki generate` | 全量生成 Wiki（模块页/API 参考/知识卡片/llms.txt/AGENTS.md） |
| `repo-wiki update` | 增量更新（无变更时秒级 no-op 跳过；`--dry-run` 预览） |
| `repo-wiki search -q "关键词"` | 搜索代码实体（text/semantic/hybrid 三引擎） |
| `repo-wiki lint` | 产物健康检查（三态退出码：0 通过 / 1 有问题 / 2 配置失败） |
| `repo-wiki doctor` | 环境六查诊断（配置/可写/状态/Key/网络/版本漂移） |
| `repo-wiki watch` | 监听文件变更自动增量更新 |
| `repo-wiki note "记录"` | 追加知识记录（_log.md） |
| `repo-wiki card modify <模块> --instruction "..."` | 修改知识卡片 |

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

## 版本自检（v19 t01）

产物状态记录生成时的工具版本（`generation_state.json` 的 `tool_version` 字段，`llms.txt` 头部亦标注）。`doctor` 会对比状态版本与当前二进制版本：不一致时提示"建议运行一次完整 generate 升级产物"——用于捕获 PATH 中旧版二进制（缺 `doctor`/`--dry-run` 的旧版调用会报 unrecognized subcommand，用户无从判断产物新旧）生成产物后又被新版调用的静默漂移。

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
provider = "openai"        # openai = Responses API（可配 base_url，DeepSeek 归此）
model = "deepseek-v4-flash" # openai-compatible = chat/completions（兼容端点）
api_key_env = "DEEPSEEK_API_KEY"

[embed]
enabled = false  # 启用后开启语义搜索
model = "text-embedding-3-small"

[search]
enabled = true
default_engine = "text"   # 与 schema 默认一致（v19 t02 统一示例）

[incremental]
enabled = true
strategy = "git-diff"
```

### LLM Provider 协议说明（v17 t02 拆分）

`provider` 按协议显式绑定，不是品牌选择：

| provider | 协议 | 适用 |
|----------|------|------|
| `openai` | OpenAI **Responses API**（`POST /responses`） | OpenAI 官方；DeepSeek（`base_url = "https://api.deepseek.com/v1"`，deepseek-v4-flash 已支持 Responses）；其他提供 /responses 的服务 |
| `openai-compatible` | **chat/completions**（`POST /chat/completions`） | 阿里云/自建等 OpenAI 兼容端点（无 /responses）；v17 起原 `custom` 并入此值（旧配置需改 `provider = "openai-compatible"`） |
| `anthropic` | Anthropic Messages API | Claude 系列 |
| `mock` | 本地模拟（不触网，返回占位内容） | 测试/CI/无 Key 演示 |

`openai`（Responses）请求失败且状态码为 404/400（端点不支持信号）时自动回退同 `base_url` 的 chat/completions 重发一次。

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

### Agent 入口文件（llms.txt / llms-full.txt）

面向外部 AI Coding Agent 的机器消费索引，随 `generate`/`update` 写入产物根：

- **`llms.txt`**：站点地图（llmstxt.org 社区规范），列出全部模块页/全局文档/卡片路径——Agent 先读它发现文档位置，再按需打开页面
- **`llms-full.txt`**：模块职责一句话 + 实体清单（签名级）内联索引（llms.txt 的超集，社区惯例格式非官方规范）。单次读取即获得完整骨架，无需逐页打开；32K token 预算内按启发式裁剪（丢常量级条目 → 丢无源码定位条目 → 实体签名截断 → 整模块省略，模块名始终保留）

两者都是确定性重生成产物，不参与人工修改保护。
