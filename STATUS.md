# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无（output::wiki_languages 委托 generate::collect_languages，依赖方向 output → generate，无环；incremental → output 路径辅助函数为既有方向的延续）
- 测试覆盖率：cargo test 186 通过（151 unit + 5 integration + 1 progress + 1 output_override + 3 snapshot + 3 cli + 4 multilang + 11 plan + 7 protected），0 失败；另有 5 项 bench 基准可运行（cargo test --bench bench_search）
- 代码量：约 9,600 行 / 57 .rs 文件
- clippy --all-targets -- -D warnings：0 警告；cargo check --all-targets：0 错误 0 警告

## 二、本次变更影响范围
- 修改的功能：
  1. A1 增量删除清理：deleted/renamed 旧路径计入 changed_files；清理路径改为统一命名规则（wiki/{lang}/{module}.md + cards/{lang}/{module}.md，全语言目录），替换硬编码 wiki/modules/{stem}.md
  2. A2 Update 子命令新增 --force（清空人工修改保护集，与 generate --force 语义一致，复用 load_protection）
  3. A3 卡片人工修改保护：卡片指纹计入 doc_fingerprints，render_all 写卡片前查保护集（与 wiki 页同规则）
  4. A4 路径规则收敛：output/mod.rs 新增 wiki_page_path/card_page_path/card_file_stem/module_page_file_name，6 处调用点单源化
  5. fixtures/sample-repo/config.toml 恢复为干净模板（移除已入库的 pid/port 残留）；progress_test 改为 copy_dir 到唯一临时目录运行，不再污染 fixture
  6. 补全缺失测试：prompt notes 注入 / ApiRef 模板命中 / 白名单过滤（filter_by_whitelist 重构收敛两处 retain）+ 3 个 CLI 集成测试（uninstall --force 隔离验证 / --progress-json 单调递增 / card modify 端到端）
- 摸到的文件：src/lib.rs、src/main.rs、src/output/mod.rs、src/output/markdown.rs、src/incremental/mod.rs、src/incremental/state.rs、src/generate/card.rs、src/generate/mod.rs、src/generate/prompt.rs、tests/test_protected_files.rs、tests/test_cli.rs（新建）、tests/output_override_test.rs（新建）、tests/progress_test.rs、tests/fixtures/sample-repo/config.toml
- 是否改变了接口/契约：是（record_doc_fingerprints 新增 cards 参数；save_generation_state 新增 cards 参数——均为内部函数，调用点已同步；Update 子命令新增可选参数 --force，CLI 兼容旧命令）

## 三、已知风险点（由AI诚实自曝）
- A1 删除清理的模块名从文件路径派生（与 chunk_by_file 同规则），无法还原被删文件原模块聚类的归属（文件已不在图中）；若原页面按聚类深度 2-3 命名，派生名可能不精确命中。这是删除场景的信息极限，可接受
- FileWatch 增量策略不追踪文件删除（changed_files 只来自现存 insights），该策略下删除清理不触发——既有策略局限，未在本次修复范围内
- A3 卡片保护后，扩展语言目录的卡片仍会被关联文档刷新（保护判定按 doc.language 计算路径，仅主语言人工编辑场景受保护）；多语言下人工编辑非主语言卡片不在保护语义内
- ApiRef 测试断言词以真实模板特征词（API 参考/## 函数/-> Ret）为准，模板改写时需同步

## 四、下次最该做的事（AI建议）
1. FileWatch 策略补删除事件追踪（watch.rs 事件流目前不向 changed_files 传递删除路径），使删除清理在 watch 模式下也生效
2. 提交本次累积变更（fixture 干净模板 + 186 测试 + 全部修复），将已入库的 config.toml 残留从 git 历史中清除
