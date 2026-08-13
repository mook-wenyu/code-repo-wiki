# Code Repo Wiki

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE) [![CI](https://github.com/mook-wenyu/code-repo-wiki/actions/workflows/ci.yml/badge.svg)](https://github.com/mook-wenyu/code-repo-wiki/actions/workflows/ci.yml)

自动为代码仓库生成**持续更新**的 Wiki 文档：模块页、API 参考、知识卡片，供人和 AI 助手阅读。

零配置开箱即用 · 单二进制 · 支持 11 种语言 · 增量更新 · 可注册为 OpenCode 插件 / Claude / Codex MCP

---

## 快速开始

> 前置条件：需要 Rust 工具链（含 `cargo`）。目标仓库可以是**任何语言**的项目（解析器见[限制项](docs/reference/limitations.md)），不要求是 Rust 仓库，也不要求是 git 仓库。

```bash
# 1. 安装（源码构建；发布 crates.io 后可直接 cargo install code-repo-wiki）
cargo install --path . --locked

# 2. 配置 LLM API key（不配也能跑：自动降级为本地模拟内容）
export OPENCODEGO2_API_KEY="sk-..."            # macOS/Linux
# Windows PowerShell: $env:OPENCODEGO2_API_KEY = "sk-..."

# 3. 在目标仓库里生成 Wiki（零参数全自动，产物在 .code-repo-wiki/wiki/zh/）
cd /path/to/your/repo
code-repo-wiki generate
```

**一键全自动（推荐）**：`code-repo-wiki install` 注册 git `post-commit`/`post-merge` hook——之后**每次 commit 后 Wiki 自动增量更新**，无需再手动执行任何命令；同时注册 OpenCode 插件与 MCP（用户级全局，一次安装所有仓库可用），`--claude` / `--codex` 可加注 Claude Code / Codex MCP。常驻实时模式用 `code-repo-wiki watch`（代码保存即更新）。卸载用 `code-repo-wiki uninstall --force`。

**可选配置**：默认零配置即可运行；需要自定义时使用 `config.toml`——用户级（Windows: `%USERPROFILE%\.code-repo-wiki\config.toml`；其他: `~/.code-repo-wiki/config.toml`，可用环境变量 `CODE_REPO_WIKI_HOME` 重定位；v41 起自动从旧目录一次性迁移）与项目级（仓库根 `config.toml`）字段级合并，详见[配置参考](docs/reference/config.md)。语义嵌入走纯远程 API（默认阿里百炼 `qwen3.7-text-embedding`，配置 `[embed]` 段，key 缺失时自动降级为纯文本搜索）

## 常用命令

| 命令 | 说明 |
|---|---|
| `generate` | 全量生成 Wiki（分阶段进度提示 + LLM 逐项进度 + 完成摘要） |
| `update` | 增量更新：无变更秒回，失败模块自动补偿重试，尾部自动 lint 复核 |
| `watch` | 常驻监听，代码保存即自动更新（内置崩溃自愈） |
| `search --query "关键词"` | 代码语义搜索——默认 hybrid 引擎 + top-k 10，**均可省略**；另有 `ast-search` 精确符号查找 |
| `lint` | 九类健康检查（断链/过时/引用错位/LLM 编造） |
| `doctor` / `status` | 环境健康检查 / Wiki 状态报告 |
| `bench` | RepoDocBench 五维评测 + rubrics 准则评分（--reference 可注入人工参考材料） |
| `export` | 一键导出静态 HTML 站点 |
| `sync` | 以 Git 工作区内容同步指纹库（不触发 LLM） |
| `key` | 交互式配置 LLM API key（用户级） |
| `note` | 知识沉淀记录（追加到 `_log.md`） |
| `card` | 知识卡片操作（generate/modify/supplement/rewrite） |
| `mcp` | 启动 MCP stdio server（Claude Code/Cline 接入） |
| `install` / `uninstall` | 一键集成 / 卸载（hook + 插件 + MCP + AGENTS.md） |

全部命令见 [CLI 命令参考](docs/reference/cli.md)。

## 它能做什么（30 秒了解）

输入一个代码仓库，`code-repo-wiki generate` 产出 `.code-repo-wiki/` 目录：

- 分析源码结构（tree-sitter AST + 知识图谱 + 社区检测自动划分模块）
- 让 LLM 为每个模块生成知识卡片与文档页（API 参考含**真实文件与行号**）
- 之后每次 `git commit` 自动增量更新（只重写受影响的模块页）

真实产物示例（本项目自己生成的 `.code-repo-wiki/wiki/zh/api.md`）：

```markdown
## analysis

- `pub fn detect_communities(graph: &KnowledgeGraph) -> Vec<Vec<NodeId>>` (函数, pub) — 文件级社区检测：返回 File 节点的社区划分（每社区一个 `Vec<NodeId>`） — src\analysis\community.rs:54
- `MIN_DIRS_FOR_SUPERNODE` (常量) —  — src\analysis\community.rs:90
```

## 核心功能

| 功能 | 说明 |
|---|---|
| 代码解析 | tree-sitter：Rust/TypeScript/TSX/Python/Go/JS/JSX/MJS/CJS/C#/Java 共 11 种；自动跳过噪音目录（依赖/构建产物/Unity 根级 `Packages`/`Temp`/`Logs`，详见[限制项](docs/reference/limitations.md)） |
| 模块划分 | petgraph 知识图谱 + leiden-rs 社区检测，自动发现模块边界 |
| Wiki 生成 | LLM 生成知识卡片 + 模块页 + API 参考，引用真实文件/行号 |
| 增量更新 | 实体级变化分类（新增/删除/签名变更/正文修改）驱动语义传播，只重生成受影响模块；基于内容指纹，**非 Git 仓库同样支持** |
| 搜索 | BM25 全文 + 向量语义 + RRF 混合排序（默认 hybrid）；另有 `ast-search` 精确符号查找 |
| 文件监听 | `watch` 常驻监听，保存即更新（内置崩溃自愈） |
| HTML 导出 | `export` 一键导出静态 HTML 站点 |
| AI Agent 友好 | 自动生成 `llms.txt`/`llms-full.txt`（Agent 索引）、AGENTS.md 引导块；`install` 注册 OpenCode 插件 / Claude / Codex MCP（用户级全局） |
| 质量评测 | `bench` RepoDocBench 五维评测 + rubrics 准则评分；`lint` 九类健康检查（断链/过时/引用错位/LLM 编造） |

## 面向 AI 助手

本项目把「可被 AI 高效使用」作为一等目标：

- `code-repo-wiki search --query "关键词"` —— 代码语义搜索（BM25 + 向量 + RRF 混合，默认 hybrid；另有 `ast-search` 精确符号查找）
- 每次生成自动重写 `llms.txt` / `llms-full.txt` —— 按 Agent 上下文预算裁剪的仓库索引
- `install` 向仓库根注入 AGENTS.md 引导块 —— AI 代理打开仓库即可按指引维护文档
- 已注册的插件 / MCP 工具可供 OpenCode、Claude Code、Codex 会话直接调用。MCP server（`code-repo-wiki mcp`）暴露五个 `wiki_` 前缀工具（全部只读，产物由 `generate` 先行生成）：
  - `wiki_search` — 按关键词检索代码实体（text/semantic/hybrid 三引擎，hybrid 含调用链补全）；`wiki_ast_search` — 精确符号定义查找（全量 AST 扫描，返回文件+行号+签名，成本随仓库规模增长）
  - `wiki_status` — 报告 Wiki 生成状态与健康度（页面/卡片计数、语义索引降级原因、lint 问题清单）；`wiki_read_page` / `wiki_read_card` — 读取模块页/架构/概览/API 页面与知识卡片正文
  - 调用示例（Claude Code）：`wiki_status` 确认已生成 → `wiki_search` 定位实体 → `wiki_read_page` 读取对应模块页

## 文档

完整文档见 [docs/index.md](docs/index.md)（Diátaxis 组织）：

- [教程](docs/tutorial.md) —— 第一次生成 Wiki 的完整流程
- [CLI 命令参考](docs/reference/cli.md) —— 全部子命令
- [配置参考](docs/reference/config.md) —— 零配置默认 + 全部可配键 + 已知问题
- [架构说明](docs/explanation/architecture.md) —— 流水线各阶段与设计决策
- [FAQ](docs/reference/faq.md) · [限制项](docs/reference/limitations.md) · [lint 检查项](docs/reference/lint.md) · [运维指南](docs/how-to/watch.md) · [术语表](docs/glossary.md)

## 常见问题（精简）

| 问题 | 回答 |
|---|---|
| 没有 API key 能跑吗？ | 能。LLM 降级为本地模拟、语义搜索降级为纯文本，全流程不中断 |
| 手动改过文档会被覆盖吗？ | 不会。人工修改过的页面自动加入保护集（SHA256 指纹） |
| 文档过时了？ | `code-repo-wiki lint` 检查断链/过时/引用错位；commit 后自动增量更新 |
| 会泄露我的代码/密钥吗？ | 只把模块内实体清单/文件路径发给 LLM（不发全文件）；key 从环境变量或 `config.toml` 读取（建议明文 key 只放用户级配置或环境变量，项目级 `config.toml` 若入版本控制则不要写 key） |
| 网关报 400/500？ | 默认 LLM 端点上游偶发波动（2026-08 期间出现过，已实测恢复）；重试一次，持续失败切换阿里百炼端点——见[配置参考-已知问题](docs/reference/config.md) |

更多见 [FAQ](docs/reference/faq.md)。

## 贡献

- **构建**：`cargo build --release`；**测试**：`cargo test`（全量套件）；**静态检查**：`cargo clippy -- -D warnings` + `cargo doc --no-deps`
- **CI**：ubuntu/windows/macos 测试矩阵 + clippy/doc 门禁 + markdownlint/lychee 文档门禁 + actionlint（工作流见 `.github/workflows/ci.yml`）
- **发布流程**：版本号与 CHANGELOG 规范见 [docs/how-to/maintenance.md](docs/how-to/maintenance.md)

## License

Apache License 2.0 — 见 [LICENSE](LICENSE)。
