# CLI 命令参考

除 `bench-manifest` 外，所有子命令支持 `--root <路径>` 指定项目根（默认当前目录；`bench` 的 `--root` 为必填）；每个命令 `--help` 有详细用法。

| 命令 | 用途 |
|---|---|
| `generate` | 全量生成 Wiki（分阶段进度提示 + LLM 逐项进度 `N/M` + 完成摘要；`-o` 输出覆盖、`--force` 强制重写、`--progress-json` 机器可读进度） |
| `update` | 增量更新（git diff / 文件指纹；分阶段进度提示 + LLM 逐项进度 + 完成摘要；`--dry-run` 预览不执行） |
| `watch` | 监听文件变更自动更新（内置崩溃自愈循环） |
| `sync` | 以 Git 工作区内容同步指纹库（不触发 LLM） |
| `status` | 查看 Wiki 状态（含语义索引 / LLM 状态行） |
| `lint` | 产物健康检查（断链/过时/引用错位/坏 mermaid）；退出码 0/1/2 供 CI |
| `export` | 导出 HTML（`--skip-generate` 从快照直接导出） |
| `doctor` | 环境诊断（配置/产物/输出/Key/网络/版本六查） |
| `key` | 交互式配置 LLM API key（用户级，`--env` 用环境变量引用） |
| `install` | 一键集成：用户级默认配置 + OpenCode 全局 MCP/插件 + AGENTS.md 引导 + git hooks（`--claude` 加用户级 ~/.claude.json MCP + CLAUDE.md、`--codex` 加 Codex 配置） |
| `uninstall` | 移除全部集成（MCP 条目/插件/引导块/hooks；需 `--force`） |
| `search` | 搜索代码实体（text/semantic/hybrid 三引擎；`--query` 必填，`--engine` 默认 hybrid、`--top-k` 默认 10——均可省略；`--json` 机器可读） |
| `ast-search` | AST 精确符号查找（文件+行号+签名） |
| `card` | 知识卡片操作（generate/modify/supplement/rewrite） |
| `note` | 知识沉淀记录（追加到 `_log.md`） |
| `mcp` | 启动 MCP stdio server（Claude Code/Cline 接入） |
| `bench` / `bench-manifest` | 文档质量评测 / 清单批量跑分 |

## MCP server 工具

`mcp` 子命令启动 MCP stdio server（Claude Code/Cline 接入），暴露五个 `wiki_` 前缀工具（全部只读；产物由 `generate` 先行生成，未生成时工具显式报错提示）：

| 工具 | 用途 |
|---|---|
| `wiki_search` | 按关键词检索代码实体（text/semantic/hybrid 三引擎，hybrid 含调用链补全；结果含 score 与调用关系，尾部附语义降级提示） |
| `wiki_ast_search` | 精确符号定义查找（全量 AST 扫描，返回文件+行号+签名，附扫描耗时提示——成本随仓库规模增长，慎用） |
| `wiki_status` | 报告 Wiki 生成状态（页面/卡片计数、语义索引降级原因、lint 问题清单；未生成时提示先运行 generate） |
| `wiki_read_page` | 读取模块页/架构/概览/API 页面 Markdown 内容（返回文件路径 + 正文） |
| `wiki_read_card` | 读取知识卡片（模块结构化摘要；返回文件路径 + 正文） |

调用示例（Claude Code）：`wiki_status` 确认已生成 → `wiki_search` 定位实体 → `wiki_read_page` 读取对应模块页。

## 评测命令

- `bench --root <repo> [--judge]`：自动评测（Coverage/文档信息/Completeness@K/lint/Update Recall/耗时 + LLM-as-judge 打分）
- `bench --reference <path>`：注入人工参考材料供 judge 对照
- `bench --repodoc`：RepoDocBench 对齐五维聚合报告（Coverage/Doc Info/Completeness@K/TQS/Update Recall，LLM 不可用等降级显式标注）
- `bench --rubrics-only`：仅跑文档质量准则评分
- `bench-manifest`：清单批量跑分（注释/空行/本地路径/URL/带名形式；本地路径无需预克隆，URL 自动克隆）

## 环境变量

| 变量 | 用途 |
|---|---|
| `OPENCODEGO2_API_KEY` | 默认 LLM key（opencode.ai 网关） |
| `BAILIAN_API_KEY` | 默认 embed key（阿里百炼），LLM 降级时也常用 |

用户级配置（`~/.code-repo-wiki/config.toml`，Windows 为 `%USERPROFILE%\.code-repo-wiki\config.toml`）中可写 `api_key_env` 引用任意环境变量名，项目级配置只写变量名不写明文。

## 退出码约定

- `lint`：`0` 干净 / `1` 有问题 / `2` 配置或环境错误（CI 可用）
- 其他命令：`0` 成功 / 非零失败（错误信息含定位上下文）
