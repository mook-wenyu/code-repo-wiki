# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 226 通过 0 失败（176 lib + 50 集成，14 套件）
- 代码量：约 10,500 行 / 56 .rs 文件（新增 src/output/lint.rs）

## 二、本次变更影响范围
- 修改的功能：wiki-quality-gaps 计划（.scratch/wiki-quality-gaps/）7 项全部实现——模块职责描述、概览自底向上合成、wiki lint、AGENTS.md 引导、wikilink 导航、知识沉淀通道
- **01 模块职责描述**：wiki.rs describe_modules/describe_module（逐模块 LLM 一行职责，src 兜底跳过）；prompt.rs module_description_prompt（≤30 字约束）；architecture/overview 接入 enriched 快照
- **02 概览自底向上合成**：overview_prompt 注入卡片摘要（CodeWiki 合成：父概览基于子文档卡片摘要）
- **03 wiki lint**：src/output/lint.rs（孤儿页=无入链/断链=链接目标不存在/过时=页面 mtime<源文件 mtime）；CLI `repo-wiki lint`（exit 码供 CI）
- **04 AGENTS.md 引导**：generate_agents_md 写 output_dir.parent()/AGENTS.md（已存在跳过，人工版不覆盖）；render_all 末尾幂等调用
- **05 wikilink 导航**：TOC 与 html index 按模块分组（Karpathy index 优先导航），render_toc_line 抽离
- **06 知识沉淀通道**：形态定案=Karpathy log.md 追加日志（note 命令→_log.md），不扩 KnowledgeCard
- 前置缺陷修复（上轮）：跨文件调用边（Calls 2→6125）、模块检测阈值移除（10 模块）、概览断链、api.md/模块图空、CLI 调用链补全
- 摸到的文件：src/generate/{wiki,prompt}.rs、src/output/{mod,markdown,html,lint(新)}.rs、src/main.rs、src/lib.rs、src/analysis/{graph,module}.rs、tests/{test_cli,test_e2e,test_overview}.rs、.scratch/wiki-quality-gaps/
- 是否改变了接口/契约：新增 CLI 子命令 lint；overview_prompt 签名变化（modules+cards 参数）；TOC/HTML 结构变化（分组）

## 三、已知风险点（由AI诚实自曝）
- 10 模块真实 LLM 生成超时 10min（max_concurrent=1 串行 30+ 调用）——describe 批量可并行化（未做）
- output.dir 嵌套语义缺陷：dir="wiki" 时产物 wiki/wiki/zh（render_all 在 output.dir 下再建 wiki/{lang}）；default-config 与 schema 默认 .repo-wiki 不一致（待决策）
- lint 过时检查依赖"相关文件"行格式（`- \`path\``），手工编写的页面可能提取不到源文件（降级为不检查）
- AGENTS.md 生成位置 = output_dir.parent()（仓库根），多语言/多输出目录项目可能生成位置不合预期
- B1-B6 运行时项（插件加载/命令交互/watch 端到端）仍需真实会话验证
- 工作树 8 文件改动+1 新增待提交（含 default-config.toml deepseek 配置、opencode-swarm.json 删除等环境改动）

## 四、下次最该做的事（AI建议）
1. 提交本轮 wiki-quality-gaps 7 项实现（9 文件）
2. 评估 10 模块 describe 并行化（性能）与 output.dir 嵌套语义（default-config 对齐 schema）
