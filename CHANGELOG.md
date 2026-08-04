# Changelog

本文件记录 repo-wiki 的显著变更，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

## [Unreleased]

### Added
- llms-full.txt 完整内容索引（v19 t05）：内联全部实体摘要，32K token 预算内四档裁剪（完整 → 去常量 → 去无定位 → 精简），头部含生成版本行
- 版本自检（v19 t01）：doctor 新增「版本」检查（状态记录 vs 当前二进制版本漂移提示）；llms.txt / llms-full.txt 头部注入生成版本
- update no-op 快速判定（v19 t06）：git HEAD + 工作树状态 + 产物存在三重判据，无变更时秒级跳过（含 watch 免费空转）
- CI 模板（v19 t02）：.github/workflows/ci.yml（build + test + clippy -D warnings）

### Changed
- 社区命名稳定重排序（v19 t07）：社区按大小降序编号（Graphify 模式），新增小社区不扰动既有大社区编号
- lint 实体噪声过滤（v19 t03）：单字符/纯数字实体名（LLM 编造噪声）两侧同口径忽略；graph 新增 mod 实体类型（NodeKind::Module）
- 文档与配置模板统一（v19 t02）：init 模板 embed 段统一 text-embedding-3-small + OPENAI_API_KEY（消除三方失实）；README 补齐 init 全局链/lint 三态/dry-run/doctor/子命令表

### Fixed
- 测试产物泄漏隐患根治（v19 t04）：测试配置统一绝对路径（helper 强制临时目录）

## [0.2.0] - 2026-08-04

### Added
- LLM Provider 协议拆分（v17 t02）：provider = openai（Responses API，base_url 可配，DeepSeek 归此）/ openai-compatible（chat/completions，custom 并入）；Responses 端点不支持（404/400）自动回退 chat/completions；旧配置 provider = custom 需改 openai-compatible
- doctor 诊断命令（v17 t04）：六查（配置可解析/产物目录可写/输出目录状态/LLM Key 引导/网络探活/版本漂移），mock provider 跳过网络查
- update --dry-run 预览（v17 t03）：列出变更文件与受影响模块，零副作用
- init 缺省链保护（v17 t01）：默认配置链已存在时跳过不覆盖，--force 强制重写，显式路径保持覆盖
- mock 占位页页脚（v17 t03）：mock provider 生成的页面注入「非真实内容」标记
- lint 三态退出码（v17 t03）：通过 0 / 检出问题 1 / 配置失败 2（CI 可直接消费）
- 生产就绪 P0-P2（v16）：README 安装段与 Linux/macOS 前置依赖声明、插件 PATH 绝对路径注入（install 时绑定 current_exe，不再依赖 PATH）
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
