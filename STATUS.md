# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 212 通过 0 失败（165 lib + 47 集成）
- 代码量：约 10,000 行 / 55 .rs 文件

## 二、本次变更影响范围
- 修改的功能：hardening 计划（.scratch/repo-wiki-hardening/）7 项全部处理——模块检测接线与算法根因修复、模块名相对化、git 污染清理、兜底审计、export E2E、运行时清单
- Phase 1（模块接线）：build_graph 内部完成 detect_modules 写回 graph.modules（消除三处重复检测调用）；**算法根因修复**：count_edges 聚类集合按 Contains 边扩展为"文件+实体"、统计排除结构性 Contains 边（原实现 cohesion 恒 0 导致全量图 0 模块）；修复后真实检出 3 模块、真实 generate 输出模块级卡片
- Phase 2（相对化）：scan_and_parse 产出相对路径（strip cwd 前缀），run_incremental_pipeline 对 watch_paths 同步相对化；产物模块名从绝对路径机器名变为 src_config 等
- Phase 3（git 清理）：.gitignore 加 .repo-wiki/.scratch/.swarm/evidence；31 个误提交产物文件取消跟踪
- Phase 4（兜底审计）：opencode.rs 2 处 as_object_mut().unwrap()、commands.rs 损坏 state 静默重置 → 显式报错；新增 test_sync_corrupted_state_errors_explicitly
- Phase 5：B6 补 CLI E2E test_export_produces_html_artifacts；运行时清单 .swarm/evidence/runtime-verification-checklist.md
- 摸到的文件：src/analysis/{graph,mod,module}.rs、src/ingest/mod.rs、src/lib.rs、src/commands.rs、src/config/opencode.rs、src/generate/{card,wiki}.rs、src/model/mod.rs、tests/test_cli.rs、tests/test_git_sync.rs、.gitignore
- 是否改变了接口/契约：FileInsight.path 变为相对路径（无存量用户可接受）；ModuleCluster 补 PartialEq；watch_paths 入参语义补充（相对化）

## 三、已知风险点（由AI诚实自曝）
- 04 真实 LLM（gpt-4o）验证阻塞：OPENAI_API_KEY 未配置，真实 LLM 响应解析/重试/限流行为未验证
- 模块检测阈值（cohesion>0.3 且 coupling<0.7）为经验值，跨项目鲁棒性未验证；检测出 3 模块但存在"src 兜底吸收全部未分配文件"现象
- B1/B2/B4（插件加载/命令交互/@file 转译）需真实 opencode 会话验证（清单已建）
- 工作树 13 文件修改 + 31 个产物删除待提交

## 四、下次最该做的事（AI建议）
1. 配置 OPENAI_API_KEY 后执行真实 LLM generate 验证（04 解阻塞）
2. 提交本次 hardening 改动（13 修改 + 31 删除 + .gitignore）
