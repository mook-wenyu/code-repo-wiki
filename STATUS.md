# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 209 通过 0 失败（164 lib + 45 集成/文档）
- 代码量：约 9,800 行 / 55 .rs 文件（ast_chunker.rs 已删除）

## 二、本次变更影响范围
- 修改的功能：Qoder 对等计划（followup 1785526395084）实现收尾——插件层重建、watch 折叠、人工修改反向同步闭环、真实管道验证
- Phase 1（插件层）：repo-wiki.ts 重写为官方 Hooks 形状（12 工具映射，命名导出+Plugin 类型）；.opencode/commands/{wiki,knowledge}.md 命令模板（@file→reference 转译引导）；.opencode.json 清理无效 plugins 键；opencode.rs 重写（幂等清理+is_installed 以文件存在性为准，7 单测）
- Phase 3（watch 折叠）：process_batch/fold_events 同路径跨 kind 折叠（Modified+Deleted→Deleted 等最终态语义），5 新测试
- Phase 4（1.3 闭环）：collect_manual_edits（模块精确匹配）+ CardGenerator 生成前注入（recover 旧记录+extra_edits 合并）+ sync_manual_edits_to_cards（无变更路径直写卡片）+ save 全量记录指纹（保护持续检测）+ render_all 保护页仍写卡片；5 新测试
- Phase 2（真实冒烟）：mock provider 真实 generate/sync/update 全链路验证通过（证据 .swarm/evidence/real-pipeline.md）
- 摸到的文件：.opencode/plugins/repo-wiki.ts、.opencode/commands/{wiki,knowledge}.md、.opencode.json、src/config/opencode.rs、src/incremental/{watch,state}.rs、src/generate/{card,mod}.rs、src/output/mod.rs、src/lib.rs、tests/test_protected_files.rs、.swarm/evidence/*
- 是否改变了接口/契约：sync_manual_edits_to_cards/collect_manual_edits 新增 pub 函数；GenerationState.doc_modules 新增字段（serde default 兼容旧状态）；render_all 保护语义细化（页面跳过、卡片仍写）

## 三、已知风险点（由AI诚实自曝）
- 插件实际加载需新 opencode 会话验证（本会话早于修复启动，无法自证；已记录 .swarm/evidence/plugin-layer.md）
- 模块页文件名含绝对路径（非 git 仓库时 module_path 含全路径）→ 页面与卡片文件名冗长；与官方"按模块"语义的观感差异，功能正确
- output.dir 与 search.index_dir 的相对路径解析基准不一致（index_dir 相对 cwd），冒烟中产物出现 .repo-wiki/.repo-wiki 嵌套——既有行为，未在本次范围修复
- 工作树有大量未提交改动（多轮会话累积），提交前需协调

## 四、下次最该做的事（AI建议）
1. 新会话验证插件加载与 /knowledge 命令真实行为（启动日志+手动 /wiki sync）
2. 多轮未提交改动协调提交（git status 当前 ~30 个变更文件）
