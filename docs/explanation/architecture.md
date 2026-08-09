# 架构说明

## 流水线总览

```text
Source Files → ingest (tree-sitter AST 解析) → analysis (知识图谱 + 社区检测)
            → generate (LLM 卡片 + 文档) → output (Markdown/HTML) → Wiki/
            → search (FTS5 + embedding 索引) → text/semantic/hybrid 搜索
```

## 各阶段职责

### ingest（源码解析）

- tree-sitter AST 解析 11 种语言，提取实体（函数/结构体/类/常量）与可见性、签名、行区间。
- 全量遍历 + 四层内置边界自动过滤（`.gitignore`/内置噪音目录清单/支持语言自动识别/二进制与上限）。
- 指纹（内容 SHA256）与实体哈希驱动增量判定。

### analysis（知识图谱 + 模块划分）

- 知识图谱（petgraph）：实体-文件-模块引用边，`detect_communities` 用 File 节点弱连通分量。
- leiden-rs 社区检测自动发现模块边界；v26 起目录超节点聚类（≥24 目录 → 纯目录页稳定，<24 → 实体级 Leiden）。

### generate（生成）

- chunk 切分（实体摘要片段，带引用来源）→ 知识卡片（LLM）→ 模块页 / api.md / architecture.md / overview.md。
- 实体引用清单只含签名与文件路径（不发全文件给 LLM）；签名级片段注入（v31）。
- `wiki.guide` 生成引导：`pages` 白名单过滤 / `priority` 确定性排序 / `notes` 全局注意事项注入。
- 16 路并发 LLM 调用；失败模块记录 `failed_modules` + 调用级重试 3 次退避；404/400 自动回退协议一次。

### output（渲染与导出）

- Markdown 页 + HTML 静态站点导出（`export`）；快照（`.code-repo-wiki/export/`）供 `--skip-generate` 直出。
- `llms.txt` / `llms-full.txt`（Agent 索引，确定性重写）；AGENTS.md 引导块注入。
- 人工修改保护集（SHA256 指纹）——保护页跳过更新，`generate --force` 清空。

### incremental（增量）

- git diff / 内容指纹双来源；实体级变化分类（新增/删除/签名变更/正文修改）驱动语义传播，只重生成受影响模块。
- 单次变更超 1 万行自动回退全量；no-op 空转（git-head + 状态指纹双判据）快速退出。
- v31：failed_modules 增量空 chunk 不重记；watch 冷却窗口（2s 尾沿/5s 强制）。

### search（检索）

- 三引擎：text（FTS5 + BM25）、semantic（sqlite-vec KNN 余弦 + 0.3 阈值）、hybrid（RRF k=60 融合 + 调用链补全）；默认 hybrid（v36 起）。
- 分层策略：FTS5 不足 3 条自动回溯语义；embed 降级全链路可观测（`semantic_degraded` 标记）。
- v33：CJK 2-gram token 列（独立 tokens 列 + `PRAGMA user_version` 表重建迁移）；错误路径一致化（FTS5 语法错误转空结果 + 告警）。

### bench（评测）

- 五维：Coverage / Doc Info / Completeness@K / TQS / Update Recall（RepoDocBench 对齐）。
- rubrics 准则评分（叶子 0/1 加权聚合 + judge 三态 + abstain + tie 升级阈值）。
- 产物缓存：`.state/` 指纹 + 快照，评测不重跑生成。

## 状态与缓存

| 位置 | 内容 |
|---|---|
| `.code-repo-wiki/.state/` | 生成状态（`generation_state.json`）、指纹库、单实例锁（`run.lock`） |
| `.code-repo-wiki/.cache/` | 检索图谱缓存（`call_index.json` + 指纹，v36） |
| `.code-repo-wiki/export/` | 导出快照 |
| `~/.code-repo-wiki/config.toml`（Windows 为 `%USERPROFILE%\.code-repo-wiki\`） | 用户级配置（v41 起，home 点目录惯例；`CODE_REPO_WIKI_HOME` 可重定位） |

## 设计决策速查

- **零配置**：v30 起绝大多数选项硬编码为合理默认（`scope`/`output`/`plan` 段、`embed.enabled`、`incremental.strategy` 等已删除）。
- **字段级配置合并**：用户级 ↔ 项目级（v25，uv/Claude Code/cargo 语义）。
- **确定性**：社区检测 seed 固定、llms.txt 确定性重写、join_all 保序（test_determinism 逐文件 SHA-256）。
- **无防御性代码**：错误显式传播（`?`），不静默兜底；降级路径显式标记可观测。
