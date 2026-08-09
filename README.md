# Code Repo Wiki

自动为代码仓库生成**持续更新**的 Wiki 文档：模块页、API 参考、知识卡片，供人和 AI 助手阅读。

零配置文件 · 单二进制 · 支持 11 种语言 · 增量更新 · 可注册为 OpenCode 插件 / MCP server

---

## 它能做什么（30 秒了解）

输入一个代码仓库，`code-repo-wiki generate` 产出 `.code-repo-wiki/` 目录：

- 分析源码结构（tree-sitter AST + 知识图谱 + 社区检测自动划分模块）
- 让 LLM 为每个模块生成知识卡片与文档页（API 参考含**真实文件与行号**）
- 之后每次 `git commit` 自动增量更新（只重写受影响的模块页）

真实产物示例（本项目自己生成的 `.code-repo-wiki/wiki/zh/api.md`）：

```markdown
## analysis

- `pub fn detect_communities(graph: &KnowledgeGraph) -> Vec<Vec<NodeId>>`（模块）· 文件级社区检测：
  构建 File 节点弱连通分量，每簇一个 Vec<NodeId> · src\analysis\community.rs:54
- `MIN_DIRS_FOR_SUPERNODE`（常量） · src\analysis\community.rs:90
```

## 快速开始

> 前置条件：需要 Rust 工具链（含 `cargo`）；目标仓库可以是**任何语言**的项目（解析器见[限制项](docs/reference/limitations.md)），不要求是 Rust 仓库，也不要求是 git 仓库。

```bash
# 1. 安装（源码构建；发布 crates.io 后可直接 cargo install code-repo-wiki）
cargo install --path . --locked

# 2. 配置 LLM API key（不配也能跑：自动降级为本地模拟内容）
export OPENCODEGO2_API_KEY="sk-..."

# 3. 在目标仓库里生成 Wiki（零参数全自动，产物在 .code-repo-wiki/wiki/zh/）
cd /path/to/your/repo
code-repo-wiki generate
```

**一键全自动（推荐）**：`code-repo-wiki install` 注册 git `post-commit`/`post-merge` hook——之后**每次 commit 后 Wiki 自动增量更新**，无需再手动执行任何命令。常驻实时模式用 `code-repo-wiki watch`（代码保存即更新）。卸载用 `code-repo-wiki uninstall --force`。

> ⚠️ **已知问题（2026-08 期间）**：默认 LLM 端点 `https://opencode.ai/zen/go/v1` 曾出现上游临时拒绝（网关生成端点 400/500，`/models` 正常），表现为 `generate` 报「Wiki 页面生成失败」且 `failed_modules` 全模块失败。**已于 2026-08-10 实测恢复**（真实 `generate` 端到端成功）。若再遇 400/500：先重试一次（上游波动）；持续失败时切换兼容端点（阿里百炼）——见 [docs/reference/config.md](docs/reference/config.md#已知问题)。

## 核心功能

| 功能 | 说明 |
|---|---|
| 代码解析 | tree-sitter：Rust/TypeScript/TSX/Python/Go/JS/JSX/MJS/CJS/C#/Java 共 11 种 |
| 模块划分 | petgraph 知识图谱 + leiden-rs 社区检测，自动发现模块边界 |
| Wiki 生成 | LLM 生成知识卡片 + 模块页 + API 参考，引用真实文件/行号 |
| 增量更新 | 实体级变化分类（新增/删除/签名变更/正文修改）驱动语义传播，只重生成受影响模块；基于内容指纹，**非 Git 仓库同样支持** |
| 搜索 | BM25 全文 + 向量语义 + RRF 混合排序（默认 hybrid）；另有 `ast-search` 精确符号查找 |
| 文件监听 | `watch` 常驻监听，保存即更新（内置崩溃自愈） |
| HTML 导出 | `export` 一键导出静态 HTML 站点 |
| AI Agent 友好 | 自动生成 `llms.txt`/`llms-full.txt`（Agent 索引）、AGENTS.md 引导块；可注册为 OpenCode 插件 / MCP server（`install --claude` / `--codex`） |
| 质量评测 | `bench` RepoDocBench 五维评测 + rubrics 准则评分；`lint` 九类健康检查（断链/过时/引用错位/LLM 编造） |

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
| 会泄露我的代码/密钥吗？ | 只把模块内实体清单/文件路径发给 LLM（不发全文件）；key 只从环境变量或用户级配置读取 |

更多见 [FAQ](docs/reference/faq.md)。

## 贡献

- 搜索：`code-repo-wiki search --query "k" --engine hybrid --top-k 10`
- AI Agent 入口：`llms.txt` + `llms-full.txt`（随生成确定性重写）
- CI：clippy `-D warnings` + `cargo doc` 门禁 + ubuntu/windows 测试矩阵 + actionlint
- 发布流程：见 [docs/how-to/maintenance.md](docs/how-to/maintenance.md)
