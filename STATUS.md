# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 231 通过 0 失败（181 lib + 50 集成，14 套件）
- 代码量：约 10,700 行 / 56 .rs 文件

## 二、本次变更影响范围
- 修改的功能：深度分析遗留未完成项修复——lint 过时检查修复、note 命令实现、output.dir 嵌套语义、describe 并行化、插件补 note/lint 工具
- **lint 过时检查修复（最重大）**：CLI 传空 source_roots 导致过时检查静默跳过 → main.rs Lint 分支从 scope.include 派生源码根（"src/**"→"src"）；lint 只检查 wiki 页不检查卡片（卡片才含"相关文件"段）→ 过时检查遍历 cards 目录；resolve_source_path 双路径修复（先试 cwd 相对再试 root.join）；stale 单测隔离 cwd 竞态（独立 fixture）
- **note 命令实现**：commands.rs append_note（追加 `## YYYY-MM-DD` 节，节内序号递增，Karpathy log 模式）+ main.rs Commands::Note；2 单测（序号递增/空内容拒绝）
- **output.dir 嵌套语义**：default-config.toml dir="wiki" 与 schema 默认 ".repo-wiki" 不一致（install 模板导致 wiki/wiki/zh 嵌套）→ 对齐为 ".repo-wiki" + 注释说明
- **describe 并行化**：wiki.rs describe_modules 串行 await → futures::future::join_all 并行（10+ 模块真实 LLM 超时根因之一）
- **插件补 2 工具**：wiki_note（带 text 参数）、wiki_lint（产物健康检查）；tsc 零错误
- **html css 链接 bug 修复**：wrap_html 硬编码 `../style.css`，index.html（产物根）与 style.css 同目录却引用 `../style.css`（样式失效）→ 改为 css_href 参数（index 用 `style.css`，wiki/cards 子目录页用 `../style.css`）；新增 test_wrap_html_css_href_follows_depth；真实验证 index.html href="style.css"
- **AST 精确搜索 CLI 暴露**：库层 AstQuery/SearchAgent::search_ast 完整但 CLI 未接（search 命令仅 text/semantic/hybrid 索引引擎）→ 新增 `execute_ast_search`（lib.rs，扫描源文件逐文件 AST 定位定义，返回文件+行号+签名，不依赖索引）+ main.rs Commands::AstSearch（文本/JSON 输出）+ 插件 ast_search 工具；真实验证函数/结构体定位、未找到提示、JSON；集成测试 test_ast_search_finds_definition
- 摸到的文件：src/commands.rs、src/main.rs、src/output/{lint,html}.rs、src/generate/wiki.rs、src/lib.rs、default-config.toml、.opencode/plugins/repo-wiki.ts
- 是否改变了接口/契约：新增 CLI 子命令 note；lint 现在执行过时检查（行为变化）；default-config dir 变化
- 分析报告（只读评估，不改代码）：深度分析报告落盘 .scratch/analysis-report.md——对照 RepoDoc/CodeWiki/RepoSummary/HGEN 等 2024-2026 论文与工程，确认架构方向正确（知识图谱+影响传播路线），两大短板为模块聚类（目录启发式 vs 社区检测）与影响传播（文件路径 vs AST 实体级分类），另列 P0 三项收尾（提交 6 文件、card/page 并行、LLM 重试）
- 深度演进实现计划（规划，未改代码）：.opencode/plans/1785542400000-deep-evolution.md——基于两并行子代理全源码证据化检索（发现 llm.rs 已有重试、卡片已有并发、RRF 非标准、default-config.toml 死键等 12 项关键事实）+ 网络查证（leiden-rs 0.8.1 支持 petgraph adapter、petgraph 0.8 兼容）。计划含 T0-T4 共 14 任务、D1-D10 决策点、F1-F4 雾区、依赖图与整体验收标准；核心：Leiden 社区检测聚类 + AST 实体级变化分类与语义影响传播 + 生成并发化 + 评测基准。等用户拍板决策点后开工
- 决策拍板（2026-08-01，用户经 question 工具确认）：D1=leiden-rs；D2/D4/D5/D8/D9/D10 采纳推荐；**D3=实体级双层聚类**（新增 T1.2b 特征聚类任务：embedding 注入 + 纯结构降级，analysis 层经 DIP 接 Embedder trait）；D6=实体集合对比；D7=语义传播；存储=本地 markdown；范围=T0→T4 全量。计划已修订（T1.2b、T3.3 特征追溯联动、决策记录小节），等待用户确认后开工

## 三、已知风险点（由AI诚实自曝）
- 大型仓库（10 模块）真实 LLM 全量生成超时 20min——deepseek 单次调用 ~60s，30+ 次调用，外部 API 性能限制；并行 describe 已缓解但 card/page 串行仍是瓶颈
- 真实 LLM 链路仅在小仓库（1 文件）验证成功（34s）；大型仓库未验证完整产物
- B1-B6 运行时项（插件加载/命令交互/watch 端到端）仍需真实 opencode 会话验证
- 工作树 6 文件改动待提交

## 四、下次最该做的事（AI建议）
1. 提交本轮 4 项修复 + 插件 2 工具（6 文件）
2. 真实 LLM 全量验证需小步拆分（逐模块生成）或接受 API 性能限制
