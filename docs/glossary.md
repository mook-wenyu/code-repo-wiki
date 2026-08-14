# 术语表

本项目核心概念的速查。按字母序（拼音）排列。

## A

- **api.md** —— 按模块分组的 API 参考页（真实文件与行号）。见[架构](explanation/architecture.md)。

## B

- **保护集（protection set）** —— 人工修改过的产物页面的 SHA256 指纹集合。指纹匹配的页面后续更新跳过（保护机制），`generate --force` 清空保护集。反向同步：人工修改会记录到卡片的 `pending_manual_edits`。

## C

- **chunk（片段）** —— 检索的最小单元。知识卡片按实体摘要切分为片段，带引用来源。
- **社区检测（community detection）** —— leiden-rs 在知识图谱上自动划分模块边界；目录超节点聚类（≥24 目录）在 v26 引入，保证大仓库模块页稳定。

## E

- **embed（向量嵌入）** —— 语义搜索的向量化模型。Key 缺失自动降级为纯文本（见[限制项](reference/limitations.md)）。
- **实体（entity）** —— 源码中的函数/结构体/类/常量等符号，带文件路径与行号。实体的增删改是增量更新的驱动力。

## F

- **failed_modules** —— 生成失败的模块记录（带重试）。增量空 chunk 不记 failed_modules（v31 修复）。
- **指纹（fingerprint）** —— 源文件内容 SHA256 摘要；增量更新与 no-op 判定基于内容指纹而非 mtime（非 git 仓库同样可用）。

## G

- **generate（全量生成）** —— 扫描 → 图谱 → 聚类 → chunk → 卡片 → Wiki 页 → 渲染 → 索引 → 状态的完整流水线。
- **generation_state.json** —— 生成状态记录（含 failed_modules、指纹等）。

## H

- **hybrid 搜索** —— BM25 全文 + 向量语义双路召回，RRF（k=60）融合 + 调用链补全。默认引擎（v36 起）。

## K

- **知识卡片（Knowledge Card）** —— AI 代理的结构化摘要（JSON 元数据 + Markdown），位于 `.code-repo-wiki/cards/{lang}/`，分三类：
  - **模块卡**（`card_kind: module`，`cards/{lang}/{module}.md`）——架构文档类，每模块一张，含摘要/关键实体/依赖/设计意图。
  - **Spec 卡**（`card_kind: spec`，`cards/{lang}/project/spec.md`）——代码规约类，项目级一张，列出命名/接口/质量/约束规约（每条带来源锚定）。
  - **技术栈卡**（`card_kind: tech-stack`，`cards/{lang}/project/tech-stack.md`）——项目级一张，依赖清单零 LLM 确定性解析（见[限制项](reference/limitations.md)）。
- **快照（snapshot）** —— 导出/评测用的产物快照（`.code-repo-wiki/export/`），`export --skip-generate` 从快照直接导出。

## L

- **llms.txt / llms-full.txt** —— Agent 索引（站点地图 / 含实体签名的内联索引），随生成确定性重写。
- **lint** —— 产物健康检查：orphan/broken/stale/bad-citation/bad-citation-overlap/bad-vctx/entity-coverage/stale-entity/bad-mermaid 九类。退出码 0/1/2。见[lint 参考](reference/lint.md)。

## M

- **模块（module）** —— 社区检测划分的逻辑单元（非目录映射），每个模块一个文档页。
- **MCP server** —— Model Context Protocol 服务（stdio），供 Claude Code/Cline 等接入；`install --claude` / `--codex` 注册。

## N

- **no-op（空转）** —— 增量更新无事可做时的快速退出（基于 git HEAD + 状态指纹双判据）。

## O

- **overview.md** —— 自底向上合成的项目概览（汇总所有模块）。
- **OUTPUT_DIR** —— 产物根目录常量，恒为 `.code-repo-wiki/`（v37 起；历史版本为 `.repo-wiki/`）。

## R

- **RepoDocBench 五维** —— Coverage / Doc Info / Completeness@K / TQS（真值一致性）/ Update Recall 评测维度；`bench --repodoc` 输出对齐报告。
- **rubrics** —— 文档质量准则评分（叶子 0/1 加权聚合 + judge 三态）。

## S

- **semantic_degraded 标记** —— embed 失败持久化标记；`search`/`status` 显式提示「语义索引已降级」，下次成功生成自动清除。
- **SSE（Server-Sent Events）** —— LLM 流式响应协议；失败自动回退非流式。
- **status_report** —— `status` 命令的产物状态报告（含语义索引/LLM 状态行）。

## U

- **update（增量更新）** —— 基于 git diff / 文件指纹的差异驱动生成，只重写受影响模块页；`--dry-run` 预览。

## W

- **watch** —— 常驻文件监听，保存即更新；崩溃自愈循环（v36 起）+ 平台托管模板见[watch 托管](how-to/watch.md)。
- **Wiki 页** —— 模块页（每模块一份：职责/实体/依赖/使用示例）。

## X

- **项目卡（project card）** —— 项目级单张卡片，存于 `cards/{lang}/project/` 子目录（与模块卡 `cards/{lang}/{module}.md` 隔离，路径明确分工不重叠）；`spec.md`= Spec 规约卡，`tech-stack.md`= 确定性依赖清单卡。
