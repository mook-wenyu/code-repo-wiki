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

## 三、已知风险点（由AI诚实自曝）
- 大型仓库（10 模块）真实 LLM 全量生成超时 20min——deepseek 单次调用 ~60s，30+ 次调用，外部 API 性能限制；并行 describe 已缓解但 card/page 串行仍是瓶颈
- 真实 LLM 链路仅在小仓库（1 文件）验证成功（34s）；大型仓库未验证完整产物
- B1-B6 运行时项（插件加载/命令交互/watch 端到端）仍需真实 opencode 会话验证
- 工作树 6 文件改动待提交

## 四、下次最该做的事（AI建议）
1. 提交本轮 4 项修复 + 插件 2 工具（6 文件）
2. 真实 LLM 全量验证需小步拆分（逐模块生成）或接受 API 性能限制
