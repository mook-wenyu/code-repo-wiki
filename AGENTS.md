# AGENTS.md — AI 代理导航（由 code-repo-wiki 生成，可人工编辑）

<!-- CODE-REPO-WIKI:START -->
本仓库使用 code-repo-wiki 维护可持续进化的项目 Wiki，产物位于 `.code-repo-wiki/`。

## 核心命令速查

| 命令 | 用途 |
|---|---|
| `code-repo-wiki generate` | 全量生成（首次/配置变更后；分阶段进度 + 完成摘要） |
| `code-repo-wiki update` | 增量更新（改完代码后运行；无变更秒回 no-op） |
| `code-repo-wiki search -q "<关键词>"` | 语义搜索实体（text/semantic/hybrid，hybrid 含调用链补全） |
| `code-repo-wiki ast-search <符号>` | 精确符号查找（文件 + 行号 + 签名） |
| `code-repo-wiki lint` | 产物健康检查（孤儿页/断链/过时/引用错位） |
| `code-repo-wiki status` | Wiki 状态报告（是否就绪/语义降级/lint 问题） |
| `code-repo-wiki watch` | 常驻监听，保存即自动更新 |
| `code-repo-wiki note "<记录>"` | 追加知识记录到 `.code-repo-wiki/wiki/zh/_log.md` |
| `code-repo-wiki install` | 注册 git hooks + 插件 + MCP（本块由它维护） |

## MCP 工具（已注册的 Agent 会话可直接调用）

| 工具 | 使用时机 |
|---|---|
| `wiki_search` | 按关键词检索代码实体（定位函数/结构体/类定义或引用，hybrid 含调用链补全） |
| `wiki_ast_search` | 精确符号定义查找（全量 AST 扫描，成本随仓库规模增长，仅需精确定位时用） |
| `wiki_status` | 先确认 Wiki 是否已生成/健康（语义索引降级、lint 问题） |
| `wiki_read_page` | 读取模块页/架构/概览/API 页面正文 |
| `wiki_read_card` | 读取知识卡片（模块结构化摘要） |

## 完成定义

- `update` 输出 no-op（无文件变更，跳过更新）即无增量待生成；
- `lint` 无孤儿页/断链/过时/引用错位问题即产物健康；
- `generate` 输出完成摘要（扫描 N 文件 / M 实体 / K 页文档）即生成成功。

## 渐进式披露（按上下文预算分层）

1. 预算紧张：只读 `.code-repo-wiki/llms.txt` 站点地图定位目标页面；
2. 预算充足：读 `.code-repo-wiki/wiki/zh/overview.md` 与 `.code-repo-wiki/wiki/zh/architecture.md`
   建立全局认知，再按需深入模块页（可用 `.code-repo-wiki/llms-full.txt` 一次获得实体骨架）；
3. 查 API 签名与文件行号：`.code-repo-wiki/wiki/zh/api.md`。
<!-- CODE-REPO-WIKI:END -->

## 产物布局与保护机制（人工维护区，install 幂等替换只动上面的标记块）

- `.code-repo-wiki/wiki/zh/` — 模块页（每模块一份，含职责/实体/依赖/使用示例）
- `.code-repo-wiki/wiki/zh/api.md` — API 参考（按模块分组，真实文件与行号）
- `.code-repo-wiki/cards/zh/` — Knowledge Card（AI 代理的结构化摘要，JSON 元数据 + Markdown）
- `.code-repo-wiki/assets/diagrams/` — Mermaid 调用图/依赖图
- 人工修改产物页面后不会被自动覆盖（保护机制），修改会反向同步到卡片（pending_manual_edits 节）
