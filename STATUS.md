# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无（output::wiki_languages 委托 generate::collect_languages，依赖方向 output → generate，无环）
- 测试覆盖率：171 测试通过（140 unit + 5 integration + 1 progress + 3 snapshot + 4 multilang + 11 plan + 7 protected），0 失败
- 代码量：约 9,200 行 / 55 .rs 文件
- clippy --all-targets -- -D warnings：0 警告；cargo build --release：通过

## 二、本次变更影响范围
- 修改的功能：Qoder Repo Wiki 功能对等计划（.opencode/plans/1785403231502-calm-moon.md）全部 6 个 Phase 实现完成
- P0：Generate/Update `-o/--output` 生效（load_config_with_output 统一覆盖）；uninstall-from-opencode 新增 `--force`
- P1：wiki_plan.yaml 对齐官方语义（version/knowledgecard/scope/hints）+ ResolvedPlan + resolve_plan（坏 yaml 显式报错）；五模板 notes/sections 注入（glob 双形态匹配 src/config/** 与 src::config）；ApiRef 模板 + 文档白名单过滤 + render_api_reference → api.md；KnowledgeCard 新增 related_files/coding_spec/tech_stack/architecture
- P2：人工修改保护（protected_docs + detect_manually_modified + 指纹路径与写盘一致修复 + generate --force 清空保护集）；非 Git 仓库显式报错/回退全量；限制项（10,000 文件 scan_with_limit / 10,000 行 diff stats）；scope_override 消费
- P3：CLI card 子命令（generate/modify/supplement/rewrite，原子写回）；插件 /knowledge 实装；--progress-json 进度事件（run_pipeline_with_progress，8 阶段）；修复 chunk_by_file Windows 绝对路径非法 module_path 存量 bug
- P4：多语言独立生成（WikiDocument.language + collect_languages + render_all 按 doc.language 分组）；render_module_call_graph → call-graph.mermaid；DB Schema 文档（generate/schema.rs 正则切块 + LLM 解析）；基准扩展（3 个新 bench + 修复旧 API）
- P5：README 同步（wiki_plan.yaml 示例/限制项/人工保护/card 命令）；增量路径 run_generation_filtered 补多语言循环；record_doc_fingerprints 统一 wiki_file_name（schema 文档恢复指纹保护）
- 摸到的文件：src/{main,lib,commands,config/{plan,mod},model/document,ingest/scanner,generate/{mod,prompt,card,chunk,wiki,schema},output/{mod,markdown,mermaid,html,crossref},incremental/{state,diff,mod},search/{agent,semantic}}.rs、benches/bench_search.rs、tests/{test_plan,test_protected_files,test_multilang,progress_test}.rs、.opencode/plugins/repo-wiki.ts、README.md
- 是否改变了接口/契约：是 — run_pipeline/run_incremental_pipeline 签名扩展（output/force/progress 回调）；render_all 加 protected 参数；WikiDocument.language、DocumentKind::{ApiReference,DatabaseSchema}、KnowledgeCard 4 新字段；新增 pub ProgressEvent/run_pipeline_with_progress/run_card_command/resolve_plan/collect_languages/collect_sql_files 等

## 三、已知风险点（由AI诚实自曝）
- 增量路径仍不生成架构概览/schema 文档（原行为，全量 generate 才产出）
- api.md / overview.md / _toc.md 由 render_all 无条件重写，不进 doc_fingerprints（无人工修改保护，既有设计）
- render_table_of_contents 链接无语言前缀，多语言下指向主语言目录外（既有行为）
- collect_sql_files 复用 scope.include（默认 src/**、lib/**），migrations/ 等目录需用户调整 scope
- extract_create_table_blocks 对字符串字面量内的分号不识别（仅定位不解析，复杂 SQL 可能提前截断）
- card 单模块编辑写回后不重建搜索索引/状态指纹（后续增量可能按旧指纹判定人工修改）
- tests/progress_test.rs 曾在并发 session 间被覆盖，现已稳定（本地 mock LLM server 0.2s）

## 四、下次最该做的事（AI建议）
1. 增量路径补生成架构概览/schema 文档（与全量一致）
2. render_table_of_contents 链接加语言前缀，修复多语言 TOC 指向
3. MCP Agent 工具定义（Phase 6 插件侧剩余项）+ Leiden 模块聚类算法（Phase 2 剩余项）
