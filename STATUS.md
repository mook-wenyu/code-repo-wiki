# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 202 通过 0 失败（159 lib + 43 集成/文档）
- 代码量：约 9,700 行 / 55 .rs 文件（ast_chunker.rs 已删除）

## 二、本次变更影响范围
- 修改的功能：Qoder 对等计划全部实现——搜索层收敛、overview 独立生成、api.md 主语言、三更新路径、插件补全、端到端测试
- Phase 0.1/0.2：删除零消费的 ast_chunker.rs；CallGraph 接入 SearchAgent（call_index 预计算表，SearchHit 新增 callers/callees 字段，lib.rs 无需配合）
- Phase 0.3/0.4：DocumentKind 新增 ProjectOverview 变体 + overview_prompt 独立 LLM 生成（全量/增量共用 generate_global_documents），output 删除占位拼接，overview 走 write_document（指纹/保护/多语言自然覆盖）；api.md 只写主语言
- Phase 1.1：WatchEvent{paths,kind} 显式事件类型，run_incremental_pipeline 新增 change_kind 参数，Deleted 直入 cleanup_deleted_outputs
- Phase 1.2：新 CLI 子命令 sync（commands.rs sync_from_git：指纹不存在→记录、不匹配→更新、受保护→跳过，不触发 LLM）
- Phase 1.3：KnowledgeCard.pending_manual_edits 字段 + lib.rs inject_manual_edits（定位卡片记录路径+摘要）+ prompt 注入"人工修改待同步"节 + markdown/html 渲染
- Phase 2：插件 /knowledge 4 子命令支持 --reference 转发（extractReferences helper，不存在文件 CLI 显式报错）；/wiki 新增 sync 子命令
- Phase 3.1：tests/test_e2e.rs 端到端测试（generate 产物 → 增量只重写受影响页 → Deleted 清理）；config 新增 LlmProviderType::Mock（serde "mock"）供无网络测试/CI 使用
- 摸到的文件：src/search/{ast_chunker(删),mod,agent,hybrid,callgraph}.rs、src/model/document.rs、src/generate/{wiki,mod,card,prompt}.rs、src/output/{mod,markdown,html}.rs、src/incremental/{watch,mod,state}.rs、src/lib.rs、src/commands.rs、src/main.rs、src/config/schema.rs、.opencode/plugins/repo-wiki.ts、tests/{test_e2e(新),test_overview(新),test_git_sync(新),test_multilang,test_protected_files,test_cli}.rs
- 是否改变了接口/契约：run_incremental_pipeline 新增 watch_paths/change_kind 参数；SearchHit 新增 callers/callees（serde default 向后兼容）；KnowledgeCard 新增 pending_manual_edits；LlmProviderType 新增 mock 变体；卡片 markdown 可选节

## 三、已知风险点（由AI诚实自曝）
- 全量/增量管道路径（generate_all_cards）的 LLM 输入注入未闭环：CardGenerator 不访问输出目录，管道路径记录注入靠 lib.rs inject_manual_edits 生成后完成；单卡重生成路径（CLI card）闭环完整
- extract_pending_manual_edits 纯文本节解析依赖渲染层固定格式
- inject_manual_edits 的 stem 匹配：模块名含下划线时（src::foo_bar vs src::foo::bar 均成 src_foo_bar）可能串卡片，属现有命名规则固有限制
- 工作树有大量未提交改动（多轮会话累积），提交前需协调

## 四、下次最该做的事（AI建议）
1. 若需管道路径的 LLM 输入闭环：给 CardGenerator 注入输出目录，生成前从旧卡片恢复 pending 记录（需改 generate/mod.rs 与 lib.rs）
2. 多轮未提交改动协调提交（git status 当前 ~25 个变更文件）
