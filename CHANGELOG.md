# Changelog

本文件记录 repo-wiki 的显著变更，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

## [Unreleased]

### Added
- LLM Provider 协议拆分（v17 t02）：provider = openai（Responses API，base_url 可配，DeepSeek 归此）/ openai-compatible（chat/completions，custom 并入）；Responses 端点不支持（404/400）自动回退 chat/completions；旧配置 provider = custom 需改 openai-compatible
- 生产就绪 P0-P2（v16）：README 安装段与 Linux/macOS 前置依赖声明、插件 PATH 绝对路径注入（install 时绑定 current_exe，不再依赖 PATH）、CHANGELOG 建档
- 评测闭环（v14 C 组）：Rubric 层级完整性维度 7（CodeWikiBench 协议：docs_tree → 3 次独立生成 + 1 次合并 → 叶子 0/1 判定 → 加权聚合 S±σ+coverage）；TQS 补机会校正 Cohen's κ、位置偏差 |P(A胜)−0.5|、低置信模块清单
- 语义 lint（v14 D 组）：LLM 跨页矛盾检查（变更驱动、单次调用合并多页、失败只告警）
- llms.txt 导出（v14 E 组）：Agent 站点地图（确定性生成，render_all 同步写）
- watch Ctrl-C 优雅退出（v14 F 组）：stop_flag + 500ms 轮询，等当前增量生成完成后退出
- 引用机械校验（v14 B 组）：区间重叠两级校验（文件级 + 实体区间覆盖），lint 新检查 bad-citation-overlap
- 全局配置链（v13 E 组）：默认配置搜索链 项目级 → 全局（Windows %APPDATA%/repo-wiki，其他 ~/repo-wiki）→ 创建全局
- 符号漂移检查（v13 D 组）：api.md 清单实体与当前源码 AST 对比（stale-entity）
- LLM 生产路径统一流式（v13 A 组）：complete 默认实现委托 complete_stream，移除客户端总超时（SSE 60s 空闲超时保护）

### Changed
- 解析失败统计（v13 B 组）：ScanOutput{insights, files_failed}，AnalysisStats 新增 files_failed
- Mermaid 依赖图/调用图确定性排序（v13 A 组）
- 首次 update 无 git 基线时回退全量生成（v13 A 组）
- lint 检查数 3 类 → 7 类（孤儿/断链/过时/引用/实体覆盖/Mermaid/符号漂移）

### Fixed
- 增量语义索引入口静默失败（v14 A 组）：EmbeddingEngine/数据库打开失败显式告警
- 状态文件读取/保存失败静默吞错（v13 A/B 组）
- 文档指纹读取失败保守计入保护集（v13 A 组）
- test_cli mock LLM server 未按 SSE 格式响应导致卡片内容为空（v13 A4 连带回归）

## [0.1.0] - 2026-08-02

初始版本：AI 驱动的代码仓库 Wiki 自动生成工具（全量/增量生成、多引擎搜索、
文件监听、HTML 导出、OpenCode 插件、MCP server、评测基准）。
