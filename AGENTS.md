# AGENTS.md — AI 代理导航（由 repo-wiki 生成，可人工编辑）

本仓库使用 repo-wiki 维护可持续进化的项目 Wiki，产物位于 `wiki/`。

## 产物布局

- `wiki/wiki/{lang}/` — 模块页（每模块一份，含职责/实体/依赖/使用示例）
- `wiki/wiki/{lang}/api.md` — API 参考（按模块分组）
- `wiki/wiki/{lang}/architecture.md` — 架构概览
- `wiki/wiki/{lang}/overview.md` — 项目概览（自底向上合成）
- `wiki/cards/{lang}/` — Knowledge Card（AI 代理的结构化摘要，JSON 元数据+Markdown）
- `wiki/assets/diagrams/` — Mermaid 调用图/依赖图

## AI 代理使用指引

1. 先读 `wiki/wiki/{lang}/overview.md` 与 `architecture.md` 建立全局认知，
   再按需深入模块页。
2. 查找实体（函数/结构体/类）用 `repo-wiki search -q "<关键词>"`（支持
   text/semantic/hybrid 三引擎，hybrid 含调用链补全）。
3. 修改代码后运行 `repo-wiki update` 增量更新；`repo-wiki sync` 以 Git 内容
   合入；`repo-wiki lint` 检查产物健康（孤儿页/断链/过时）。
4. 人工修改产物页面后不会被自动覆盖（保护机制），修改会反向同步到卡片
   （pending_manual_edits 节）。
5. 知识沉淀：`repo-wiki note "<记录>"` 追加到 `wiki/wiki/{lang}/_log.md`。

<!-- REPO-WIKI:START -->
本仓库使用 repo-wiki 维护可持续进化的项目 Wiki，产物位于 `.repo-wiki/`。

## AI 代理使用指引

1. 先读 `.repo-wiki/llms.txt` 定位目标页面（站点地图），再读
   `.repo-wiki/wiki/zh/overview.md` 与 `.repo-wiki/wiki/zh/architecture.md`
   建立全局认知，按需深入模块页；上下文预算充足时用 `.repo-wiki/llms-full.txt`
   一次获得完整实体骨架。
2. 查找实体（函数/结构体/类）用 `repo-wiki search -q "<关键词>"`（支持
   text/semantic/hybrid 三引擎，hybrid 含调用链补全）。
3. 修改代码后运行 `repo-wiki update` 增量更新；`repo-wiki lint` 检查产物健康。
4. 知识沉淀：`repo-wiki note "<记录>"` 追加到 `.repo-wiki/wiki/zh/_log.md`。
<!-- REPO-WIKI:END -->
