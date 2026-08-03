# 项目状态简报 （AI自动维护，禁止贴代码）

## 十五、外部对标 v7 深度分析（2026-08-03，联网权威检索）
- 方法：本地全量检索（59 src 文件/19,554 行/17 模块/369 测试全绿）+ 实时联网检索（Karpathy gist 原文、LangChain OpenWiki、FSoft CodeWiki ACL 2026、he-yufeng RepoWiki、repositories-wiki、repowikiagent、doc-wiki、repowiki-cli、AGENTS.md 标准、arXiv 2604.15385/2312.10349/2509.14273、Anthropic Memory/Dreaming）
- 结论：repo-wiki 是 Karpathy LLM Wiki 模式工程化完成度最高的开源实现之一——增量三层/引用契约/确定性测试/检索多样性四项领先；主要差距在消费引导层与质量闭环
- **新识别 4 项差距**：G1（高）AGENTS.md/CLAUDE.md 指令注入缺失（OpenWiki/repositories-wiki/repowikiagent 全部标配）；G2（中）Mermaid 无校验-降级-修复循环（OpenWiki degrade-and-repair）；G3（中）无 index.md/PageRank 阅读路径（仅 _toc.md 简单目录）；G4（低）无 CI 工作流模板 + LLM 分层
- 未验证边界保持：真实大仓库端到端/CPM 调参/embedding 注入/缓存磁盘占用/语义阈值
- 报告：.scratch/research/ANALYSIS-v7-2026-08-03.md

## 十七、G3 阅读指南 index.md 实施（2026-08-03，本会话）
- 修改的功能：新建 src/generate/index.rs——LLM 生成阅读指南（输入=模块列表[名/卡片摘要描述]+依赖/入度，输出=wiki/{主语言}/index.md，仅主语言）；LLM 失败重试 1 次仍失败→降级确定性骨架（模块入度中心度降序、同入度名称字典序的链接列表，BTreeSet 边集+全序 sort_by，不依赖 HashMap 迭代序）；lib.rs run_pipeline Phase 3b 接线（create_provider 失败同样降级，与全局文档"失败只告警"策略一致）；文档 kind=TableOfContents（避免 WikiPage 空串模块归属污染反向同步），references 填模块引用
- 摸到的文件：src/generate/index.rs（新建）、src/generate/mod.rs（+pub mod index）、src/lib.rs（Phase 3b 接线）、tests/test_index_guide.rs（新建，5 测试）
- 是否改变了接口/契约：否（新增 pub 函数 generate_index_guide/fallback_index_guide，无既有接口变更；新产物 wiki/{lang}/index.md）
- 验证：cargo build 通过；cargo test --test test_index_guide 5 passed；cargo clippy --all-targets -- -D warnings 0 告警；全量 cargo test 各二进制全绿（含 test_determinism 未受影响——Mock provider 下 index.md 内容确定性一致；test_determinism.rs 排除列表由主代理处理）
- 测试覆盖：①LLM 失败→确定性降级（同输入两次输出一致+排序断言）②重试 1 次语义（恰好 2 次调用）③正常路径产物含模块链接+元数据 ④降级骨架入度排序/字典序/references ⑤pipeline 级仅主语言（expand_languages=["en"] 时 en 目录无 index.md）+ 产物含链接
- 已知边界：index.md 不参与 plan 白名单过滤（lib.rs 在 filter_by_whitelist 之后 push，白名单用户会多出此页）；全量 cargo test 会把 mock 产物写入仓库根 wiki/（既有测试相对 output.dir 行为，与本次改动无关，跑完全量测试需 git checkout -- wiki/ 恢复）

## 十六、wayfinder 建图:repo-wiki 差距实施(G1-G3)(2026-08-03,本会话)
- 触发:v7 分析识别三项差距,用户启动 wayfinder 建决策地图
- 用户拍板(question 两轮):Destination=G1-G4 实施但 G4 答"不要 CI"(歧义,见 fog);G1=AGENTS.md 引用段+install 命令;G2=lint 检查+生成期校验重试;G3=LLM 生成阅读指南+排除出确定性快照+自动纳入 generate/update;tracker=本地 markdown;交付=计划+地图,实施另起;兼容性完全自由
- 研究子代理(并行 2 个,已 resolve):①AGENTS.md 注入—OpenWiki 用 `<!-- OPENWIKI:START/END -->` 标记对+indexOf 整块替换幂等,双写 CLAUDE.md 内容一致,建议 `<!-- REPO-WIKI:START -->` 自家标记;②Mermaid 校验—Rust 侧唯一候选 Merman(467★,纯 Rust,对齐 mermaid@11.15.0,alpha),OpenWiki 降级格式=注释+text fence,修复靠下次 run,错误可读性待 t03 POC
- 本地核查:citation 重试先例 wiki.rs:32 CITATION_RETRY_MAX=2 + wiki.rs:96-127 循环 + citation.rs:166 retry_feedback(可对齐);test_determinism.rs:43 已有排除列表(加 index.md);install/uninstall 命令已存在(MCP 配置,非 AGENTS.md)
- 产物:.scratch/wayfinder/map.md + tickets/t01-t06(2 research 已 resolve,1 prototype POC + 3 grilling open)+ IMPLEMENTATION-PLAN.md
- 地图:Destination=G1-G3 落地;frontier=t03/t04/t05/t06;Blocking:t03←t05
- 摸到的文件:STATUS.md、.scratch/wayfinder/*(新建 7 文件)

## 一、架构健康度
- 当前模块总数：17（analysis 拆出 community/feature；incremental 拆出 change；新增 project 模块承载 ProjectRoot）
- 违规跨模块调用：无（output→generate 反向依赖已消除——wiki_languages 移入 output 自持；generate 层依赖 ingest FileInsight 属既定边界，经 lib.rs 管线传入）
- 测试覆盖率：cargo clippy --all-targets -- -D warnings 0 错误 0 警告；cargo test 全绿 305 个（215 lib + 90 集成，含真实 git 差分 e2e/确定性快照/聚类稳定性/parser 去重 7 语言/hook 与插件安装）；bench 编译通过；cargo machete 零未使用依赖
- 代码量：src 约 16,800 行 / 55 .rs 文件 + tests 17 文件

## 二、本次变更影响范围
- 修改的功能：wayfinder 航图 18 票全部 work-through 完成（2026-08-02）——缺陷修复 + 债务清理全落地：
  - **票 04/05 入口闭环**：git hook 去不存在的 --quiet（command -v PATH 探测）+ install/uninstall 插件文件闭环（include_str 模板、已存在不覆盖、幂等删除）；opencode.rs 写入前 create_dir_all（新环境 .config/opencode 缺失 bug）
  - **票 06 export**：--skip-generate 从 .state/export_snapshot.json 快照恢复导出（render_all 末尾同步写快照），不再全量重生成；-o 对齐 generate；--root 支持
  - **票 07**：9 处 Leiden expect 全部补 R1 证据化注释（权重来源/契约/fail-loud 语义），Embedder 契约文档化
  - **票 08**：搜索索引 file_path 键写入时归一化（store.rs 4 函数）+ 比较点 norm_sep——Windows 增量索引删除永不命中 bug 修复
  - **票 09**：hybrid 分支补 text 索引存在性检查；RRF 去重键 name→(name,file_path)（同名不同文件折叠修复）+ 防回归测试
  - **票 10**：清理改产物集合 diff 语义（doc_fingerprints∪doc_modules − rendered − 保护集），全量/增量统一，module_n 漏删消除，替换旧路径推导法
  - **票 11**：HTML 与 markdown 同构命名 wiki/{lang}/{file}.html + .md 链接重写 .html（外部/源码定位不重写）+ 文档-卡片精确关联（修子串误匹配）
  - **票 12**：双流水线合并为单入口 run_pipeline(…, root, GenerationMode)；真增量扫描（insights 持久化缓存 .state/insights_cache.json，内容指纹失效，变更文件重 parse）；8 参 → 6 参（IncrementalResult 合并）
  - **票 13**：provider 重试/SSE 统一（指数退避 + 抖动、429/5xx/超时白名单、4xx 立即失败、SSE 解析共享、Anthropic 流式 base_url bug 修复）
  - **票 14**：parser 去重——SharedProcessor trait（公共 walk + kinds()/handle_special/fallback 差异点钩子），7 语言删公共骨架约 800 行
  - **票 15**：--root 全链路贯穿（ProjectRoot 注入 9+ 处，删除 7 个 cwd 委托变体：scan_and_parse/load_plan/resolve_plan/get_head_commit_hash/collect_sql_files/generate_schema_documents/run_incremental_update；generate 层 root 参数化；config 冗余 plan 探测删除）
  - **票 16**：CONTEXT.md 边界 2 处失实修正
  - **票 18**：tempfile 评估后不引入（pid 命名已防冲突、引入成死依赖）
  - 过程中修复的既有 bug：rewrite_md_links_to_html 输出重复累积；opencode 写入缺父目录
- 摸到的文件：43 个（src 27 + tests 14 + benches 1 + CONTEXT.md/STATUS.md）
- 是否改变了接口/契约：是（未上线无存量用户，已批准不向后兼容）——run_pipeline/run_incremental_pipeline（删除）→ run_pipeline(config_path, output, force, root, mode)；run_generation/run_generation_filtered 加 root；execute_search/execute_ast_search/run_card_command 加 root；CLI 各子命令加 --root；Export 加 --skip-generate/-o；ExportSnapshot 新契约；GenerationMode 新枚举

### root 化改造收尾（2026-08-02，本次会话）
- 修改的功能：tests/progress_test.rs 与 tests/output_override_test.rs 两个集成测试从"切 cwd"模式（CWD_LOCK 全局互斥锁 + set_current_dir + with_cwd 包裹）改为显式 ProjectRoot 注入——删除 CWD_LOCK/with_cwd、config 路径与 config.toml 写入绝对化（work_dir.join）、run_pipeline(_with_progress) 传 &root；断言语义与 mock server 行为零改动；测试现可并行（无 cwd 互斥）
- 摸到的文件：tests/progress_test.rs、tests/output_override_test.rs、STATUS.md
- 是否改变了接口/契约：否（纯测试机制改造，cargo test --test progress_test --test output_override_test 全绿：2 passed）
- 说明：src/ 下 project 模块与既有 root 化入口未动；其余测试仍用 from_cwd 委托（兼容路径，主代理后续轮换）

### root 化改造收尾·第三批（2026-08-02，本次会话）
- 修改的功能：tests/test_plan.rs 同样改造——删除 CWD_LOCK/Mutex/with_cwd（含仅被 with_cwd 使用的 std::path::Path 导入），6 处 with_cwd 块改为显式 ProjectRoot::new(dir.clone()) 注入 + dir.join 绝对路径写文件 + load_plan_at/resolve_plan_at 传 &root；断言与临时目录清理零改动；test_resolve_plan_disabled 无文件系统访问（enabled=false）保留 from_cwd
- 摸到的文件：tests/test_plan.rs、STATUS.md
- 是否改变了接口/契约：否（纯测试机制改造）；验证：cargo test --test test_plan 11 passed；cargo clippy --all-targets -- -D warnings 0 告警

### root 化改造收尾·第四批（2026-08-02，本次会话）
- 修改的功能：tests/test_clustering.rs、tests/test_clustering_stability.rs、tests/test_determinism.rs、tests/test_plan.rs 四处 cwd 残留收尾——删除全部 CWD_LOCK/_guard/set_current_dir/恢复 cwd 逻辑及 Mutex 导入；scan_and_parse_at/from_cwd 改为显式 ProjectRoot::new(tmp/dir.clone()) 注入传 &root；test_plan.rs:148 resolve_plan_at 改用 ProjectRoot::new(temp_dir.join("unused"))（enabled=false 不碰文件系统，resolve_plan_at 入口早退已确认）；test_determinism.rs 删纯死代码 CWD_LOCK；文件顶部失实 doc 注释同步修正；断言逻辑零改动
- 摸到的文件：tests/test_clustering.rs、tests/test_clustering_stability.rs、tests/test_determinism.rs、tests/test_plan.rs、STATUS.md
- 是否改变了接口/契约：否（纯测试机制改造）；验证：cargo test --test test_clustering --test test_clustering_stability --test test_determinism --test test_plan 全绿 14 passed；cargo clippy --all-targets -- -D warnings 0 告警
- 说明：全仓库测试已无 CWD_LOCK/set_current_dir 残留（grep 验证）；src/ 下 from_cwd 委托入口保留为兼容路径

### 历史记录（T0-T5 深度演进 + 航图前两轮，已完成）
- T0-T4 全部 15 项实现完成（社区检测聚类/实体级变化分类与语义传播/生成并行化/失败隔离/反向链接/CoT/评测基准/测试缺口/文档修正）；T5 清零计划全完成
- 航图第一轮：hook/插件闭环（04/05）+ provider 统一（13）

### 票 14 本地方案验证（2026-08-02，本次会话，仅新建测试不碰 src/）
- 修改的功能：新建 tests/test_watch_e2e.rs 三项验证——①watch 端到端（spawn 线程跑 run_watch，改 src 文件后 api.md 出现新函数名，实测通过）；②insights 缓存占用（10 文件实测 7,952 bytes，60 文件外推 ≈ 47,712 bytes）；③./ 前缀路径边界（固化当前行为：strip_prefix 组件比较失败 → `./src/foo.rs` 未相对化原样透传，传播子串匹配不命中，由指纹比对兜底，无功能损失，候选修复方向已注释）
- 摸到的文件：tests/test_watch_e2e.rs（新建）、STATUS.md
- 是否改变了接口/契约：否；验证：cargo check --test test_watch_e2e 通过，cargo test --test test_watch_e2e 3 passed（0.78s）
- 诚实边界：watch e2e 未 join 线程（run_watch 死循环无停止接口，进程退出即终止）；FileWatch 指纹比对相对路径读盘依赖 cwd，测试进程 cwd 非临时仓库 → 全量生成时指纹表为空 → 事件触发后为全量重生成，测试验证"事件→重跑→产物变化"链路而非纯增量短路

## 三、已知风险点（由AI诚实自曝）
- CPM 分辨率 γ=0.5/0.4 由小仓库与合成图实测选定，真实大仓库（万级文件）社区粒度未验证，需实测调参
- 特征聚类的 embedding 注入路径（0.5 语义权重）无 API key 未真实验证，仅验证纯结构降级路径
- 真实 LLM 全量生成（大仓库）端到端产物未验证；watch 端到端已在小仓库（2 文件）验证通过（票 14）
- tests/fixtures/sample-repo/config.toml 是真实 provider（无 base_url）——直接复用跑 generate 会触网（现有测试均在临时副本改写为 mock，无测试触网）
- insights 缓存（票 12）体积 = 全仓库实体元数据 + 源码文本，超大仓库磁盘占用未实测（票 14 已实测 10 文件 7.9KB，60 文件外推 ≈ 47.7KB，万级文件仍需实测）；缓存损坏自动全量重建（可观测性契约内）
- 导出快照（票 06）写入失败仅告警——快照缺失时 export --skip-generate 明确报错（契约内，非兜底）

## 四、下次最该做的事（AI建议）
1. 大仓库（万级文件）实测社区检测粒度 + insights 缓存磁盘占用 + LLM 全量生成耗时，按实测调 CPM resolution
2. 真实 LLM 端到端验证（generate/update/export --skip-generate/watch 全链路跑通一次大仓库）
3. fixture 配置改 mock provider（消除触网隐患）
4. 语义 lint（LLM 跨页矛盾/过期检查）+ 自建评测（CodeWikiBench 协议 3-5 仓库跑分）
5. 提交本次 43 文件变更（git commit 尚未执行）

## 五、航图收尾
- 18 票状态：01/02（研究）/03（失败语义总则）/17（MCP 移除，YAGNI）已解析；04-16 全部实现；18 评估后收窄。map 与 tickets 存档于 .scratch/repo-wiki-complete/
- 防回归契约生效：test_determinism 锁产物集合、test_clustering_stability 锁聚类确定性、test_incremental_git_e2e 锁真实 git 增量、test_parser_dedup_7lang 锁 7 语言解析一致性、test_hook_install/test_install_opencode 锁安装闭环

## 六、审计收尾（2026-08-02 第二轮未完成项审计）
- 本轮新修复：第 8 个漏删委托 classify_entity_changes（含生产 from_cwd 残留）；project.rs 过时注释；7 个测试文件 CWD_LOCK/set_current_dir 全部清除（显式 ProjectRoot::new）；src 层 cwd 依赖归零（from_cwd 仅剩 CLI 默认）
- 剩余 P2：fixture config 触网隐患（一行改 mock）；插件 wiki_export 未用 --skip-generate（每次导出全量重生成）
- 剩余 P3：update 无 --progress-json；insights 缓存/export 快照恢复无直接单测；watch 端到端未验证
- 未验证：真实 LLM 大仓库端到端、CPM γ 大仓库调参、embedding 注入路径、缓存磁盘占用
- 审计报告：INCOMPLETE_REPORT-v2.md

## 七、审计收尾（2026-08-02 第三轮：插件对齐 + 状态契约 + 竞态）
- P1×5：①load_protection .ok() 静默损坏状态→人工保护失效（lib.rs:82，与 sync 显式拒绝不一致）②Phase 2b 中途存盘清空保护集（incremental/mod.rs:180,225，纯推证未复现）③export_snapshot 写失败仅 warn→陈旧快照静默导出④插件 readExistingCards 读 cards/ 顶层与实际 cards/{lang}/ 不符→wiki_query/module_info 恒空⑤插件 wiki_export 缺 --skip-generate→每次导出全量重生成
- P2×15：fixture 触网/update 无 progress/缓存快照无单测/write_card_atomic 非原子/README 滞后/version 死契约/embed.provider 死字段/init vs install 双默认配置/语义索引先删后失败误导/插件 --config 硬编码/reference 逗号/插件能力未暴露/跨进程无锁/引导措辞
- 审计报告：INCOMPLETE_REPORT-v3.md
- 验证基线：305 测试全绿、clippy 0 告警、machete 干净

## 八、v3-fix 航图实施完成（2026-08-02，14 票全部落地）
- **P1 状态可靠性**：01 原子写（新 src/fs.rs write_file_atomic，state/快照/缓存/卡片四处统一，3 单测）；02 损坏 fail-loud（load_protection 区分不存在/损坏，2 测试）；03 中途存盘保护保留（state.rs preserve_protection + 两处 save 合并，2 测试）；04 快照陈旧检测（latest_wiki_page_mtime + export 报错，1 测试）
- **P1 插件**：05 读卡递归 cards/{lang}/；06 wiki_export --skip-generate；12 打磨（root 逃生口/reference 多参/force/engine/init）
- **P2**：07 fixture mock + README 补全；08 update --progress-json；09 缓存 5 测试 + export CLI 3 测试；10 config 清理（embed.provider 删/version 校验/索引时序/引导措辞）；11 init/install 统一模板；13 单进程契约（README/CONTEXT/3 模块头）；14 watch e2e 3 测试 + 缓存实测 7,952B/10 文件
- 验证基线：324 测试全绿（229 lib + 95 集成，新增 19）、clippy -D warnings 0、machete 干净
- 新发现（子代理 C）：compute_file_fingerprint 相对路径读盘依赖 cwd——FileWatch 指纹比对路径的生产隐患，已固化行为测试并标注，未修（需独立票）

## 九、外部对标深度评估 v4（2026-08-02，本轮）
- 方法：本地 17,516 行全量审计 + 权威检索（ACL 2026 CodeWiki/arXiv MemDocAgent/Anthropic 上下文工程/DeepWiki/repowise/RepoDocs/Google Code Wiki/Karpathy llm-wiki）
- **4 项结构性差距**：P0-1 LLM 正文无强制源码引用契约（业界全部标配"禁止编造+强制引用"，OpenDeepWiki GATHER-THINK-WRITE、RepoDocs 纠正重试）；P1-1 semantic 全量暴力余弦 O(N) 无 ANN（应换 sqlite-vec）；P1-2 全局文档每次增量全量重生成（mod.rs:279 确认，增量承诺被吃掉）；P1-3 插件绑定 OpenCode 非 MCP 标准
- 确认领先项：增量三层机制/人工保护/混合检索+AST 补全/确定性测试/单二进制——业界第一梯队
- 路线建议：P0-1 引用契约（1-2 天）→ P1-3 MCP（2-3 天）→ P1-2 全局文档增量（1-2 天）→ P1-4 引用/实体覆盖评测（1 天）→ P1-1 sqlite-vec（2-3 天）；不做重多智能体/向量服务化/自动重编译（论文与工程双证伪）
- 报告：.scratch/research/ANALYSIS-v4-benchmark-2026-08-02.md

## 十、v4 实施完成（2026-08-02，P0-1/P1-2/P1-3/P1-4 + 2 遗留修复）
- **P0-1 引用契约**：新 src/output/citation.rs（extract/validate/retry_feedback，8 测试，含 Windows 反斜杠与盘符排除）；wiki_page_system_prompt 加"源码引用契约"约束段（每节至少一条 file:line）；generate_wiki_page 生成后校验 + 重试（CITATION_RETRY_MAX=2，注入 retry_feedback，耗尽报错）；3 测试（重试成功/耗尽报错/无引用放行）
- **P1-2 全局文档增量**：GlobalDocAffected{architecture, schema} 影响信号；architecture=entity_changes.has_interface_change()、schema=changed_files 含 .sql；未受影响时从 export_snapshot.json 回填旧文档（backfill_global_docs，零 LLM 调用），回填失败回退生成；3 测试
- **P1-3 MCP server**：新 src/mcp.rs（rmcp 3.1.0 官方 SDK，stdio transport）；5 工具 search/ast_search/read_wiki_page/read_card/status，全部复用 lib 入口不复制逻辑；main.rs 加 `repo-wiki mcp --root .` 子命令；get_global_runtime 转 pub；tokio 加 process feature；tests/test_mcp.rs 进程级 stdio JSON-RPC 全链路测试（握手/list/call）
- **P1-4 零成本评测**：lint 新增 2 类检查——bad-citation（产物引用存在性，output_dir.parent() 项目根解析）、entity-coverage（页面核心实体须在 api.md 权威清单中，仅主语言）；5 测试
- **遗留修复**：①compute_file_fingerprint cwd 依赖——from_insights/is_file_changed 加 root 参数（root.path().join 解析相对路径），lib.rs save_generation_state 同步，测试全更新；②FileWatch 实体级分类——run_file_watch_incremental 用 state.last_commit_hash 构造伪 GitDiffResult 接 classify_entity_changes_at（非 git 回退空集），传播升级为语义传播
- 验证基线：**343 测试全绿**（247 lib + 96 集成，新增 19）、clippy -D warnings 0、machete 干净
- 未做（按路线延后）：P1-1 sqlite-vec ANN（P1-1 保留为下一优先级）

## 十一、v5 审计（2026-08-02，未完成项全面复查）
- **新修复 P1×3**：①删除孤立模块唯一文件 → cleanup 差集清空全部旧产物（run_generation_filtered 空集短路，v4 前既有 bug，既有测试被空结果满足）→ 快照回填未删模块 + 快照缺失回退全量，新增 git e2e 测试；②backfill_global_docs 按 kind 去重 → 多 .sql 仓库丢页 → 锚定 title+language；③lint entity-coverage 提取首个标识符取到 pub/fn 关键字 → 系统性误报 → entity_name_from_signature（'(' 前最后标识符）两侧统一
- **新修复 P2×4**：wiki 空内容放行→重试；MCP 路径穿越净化；related_files 空误判全删；纯删除回填补白名单
- 验证基线：**347 测试全绿**（249 lib + 98 集成，本轮新增 4）、clippy 0、machete 干净
- 未完成项清单（详见 INCOMPLETE_REPORT-v5.md）：P1-1 sqlite-vec ANN（改动面已评估收敛于 store.rs+semantic.rs，风险=vec0 固定维度 vs 混合维度测试/换模型重建）；agent.rs 语义分支零测试；语义 0.3 阈值硬编码；MCP 便捷接入（写 opencode.json 有 Unrecognized key 风险，需实测 schema）；mcp top_k 无上限；watch 绝对路径混存；citation P3 边缘×4
- 报告：.scratch/research/INCOMPLETE_REPORT-v5.md

## 十二、v6 决策分析（2026-08-02，待拍板项全部裁定）
- **决策 1**：P1-1 ANN → sqlite-vector-rs **0.3.1**（用户拍板；版本修正：0.2.2 非最新，0.3.0 yanked）。理由：rusqlite >=0.32 显式兼容、exact+hnsw 双模式同库、DELETE/事务完整；对比 sqlite-vec pre-v1 breaking change 与 DiskANN 仍在 alpha。迁移 6 要点：虚表 dim 固定需维度校验/换模型重建、0.3 相似度阈值→distance 语义转换（现有测试锚定）、伪向量混合维度（3+16）统一、借迁移抽象 SemanticSearch trait 补 agent 语义分支零测试、arrow×4+usearch 依赖重量实测记录、pre-1.0 风险登记
- **决策 2**：MCP 便捷接入 → install 写项目根 **.mcp.json**（Claude Code 官方 mcpServers 标准，跨 Cursor/VS Code 复用，首次使用需批准为安全网）；opencode 用户走文档（opencode2 `mcp add` CLI，1.x opencode.json mcp 键）；opencode 不读 .mcp.json（getmcp 指南确认）
- **决策 3**：citation 4 个 P3 边缘**全修**（C:/ 正斜杠盘符、v2.0 版本号、-src 连字符吸收、../ 逃逸）——lint bad-citation 已是 CI 门禁，误报直接阻断 CI
- **决策 4**：语义 0.3 阈值**保持硬编码**（OpenAI 官方参考线，YAGNI）；仅做迁移时相似度→距离语义保真转换
- 附带：mcp top_k clamp ≤50；watch 路径混存/语言切换回填错位留独立票
- 实施顺序：①citation 4 项 ②.mcp.json+top_k ③sqlite-vector-rs 迁移
- 报告：.scratch/research/DECISIONS-v6-2026-08-02.md

## 十三、citation 4 个 P3 边缘修复（2026-08-02，本轮，决策 3 实施）
- 修改的功能：src/output/citation.rs 四项边缘缺陷修复——①盘符识别扩展为 `^[A-Za-z]:[\\/]` 两种形态（原仅 `C:\`；补"连字符剔除后路径以盘符开头"的形态判定）；②新增最后一段扩展名规则（has_valid_extension，点后字母开头非空扩展名），排除 v2.0/1.2 版本号误报，src/v1.5.rs 正常放行；③回溯后剔除前导连字符（-src/fs.rs 列表项形态，my-file.rs 中间连字符不受影响）；④validate_citations 层拒绝含 .. 段引用（reason="路径含越界段 .."），extract 层保留完整路径
- 摸到的文件：src/output/citation.rs、STATUS.md
- 是否改变了接口/契约：否（Vec<Citation>/Vec<InvalidCitation> 结构不变，仅内部逻辑）；验证：citation 12 passed（原 8 + 新 4）、lint 10 passed、generate 62 passed、全量 lib 260 passed、clippy --all-targets -- -D warnings 0 告警
- 证据：红-绿流程——新增 4 测试先行，修复前 3 个失败（连字符/版本号/越界），修复后全绿；正斜杠盘符测试修复前已通过（该形态恰被前导 `/` 拒绝规则意外拦截），仍按决策补语义化识别
- 说明：任务开始时遇到的 src/mcp.rs clamp_top_k E0425 为瞬态增量编译问题，二次编译即消失，未修改该文件

## 十四、v6 实施完成（2026-08-02，决策 1/2/3 落地 + 遗留修复）
- **决策 1 修正（关键转折）**：sqlite-vector-rs 依赖链 Windows/MSVC 三重阻断实测不可编译——sqlite3_ext 0.2.1 `cfg!(unix)` 运行时宏门控编译期 `use std::os::unix`（connection.rs:195-202，纯 Rust 问题 gcc 不可解）；numkong 7.7.1 `-std:c99` MSVC 不认 + serial.h:890 C99 混合声明 C2059；→ 按用户裁决改 **sqlite-vec 0.1.9**（纯 C 静态嵌入 cc 编译，无 usearch/numkong/sqlite3_ext 链，Windows 编译通过，探针实测全链路）
- **③sqlite-vec 迁移**：新 src/search/vecdb.rs（vec0 虚表封装：OnceLock 进程级扩展注册/延迟建表（维度首探）/KNN 循环扩样+node_json 去重（阈值语义与旧全量余弦完全等价）/维度不匹配重建/remove/clear/count，8 测试）；semantic.rs 重写（SemanticSearch trait 抽象 + SemanticEngine 固有方法薄封装委托，空表短路免空库 embed 请求，相似度 0.3↔距离 0.7 换算保真）；agent.rs 改依赖 Box<dyn SemanticSearch>（补语义回溯分支 3 测试，原 P1 缺口）；store.rs 删向量死代码（回归纯 FTS5，4 测试删）；阈值换算锚定测试（MAX_COSINE_DISTANCE==0.7）
- **④语言切换回填错位修复**：backfill_global_docs 加语言一致性校验（快照语言 ≠ 当前主语言 → 不回填回退生成，防旧语言写盘目录错位丢页），新增测试
- **决策 2/3**：.mcp.json install/uninstall 生成移除 + mcp top_k clamp≤50（工作区已有未提交实现，审查质量达标接受，6+1 测试）；citation 4 项（见十三节）
- 验证基线：**369 测试全绿**（271 lib + 98 集成，本轮新增 21）、clippy -D warnings 0、machete 干净、release 构建通过
- 遗留（已知边界，独立票）：watch 绝对路径混存（v4 已 strip_prefix 相对化，复核无问题）、sqlite-vec pre-1.0 breaking change 风险登记
- 报告：.scratch/research/DECISIONS-v6-2026-08-02.md（决策记录）+ 本回复三张验证清单


## 十八、G1+G2 实施完成(2026-08-03,本会话)
- **G1 AGENTS.md 注入**:新命令 install-wiki/uninstall-wiki(commands.rs:331-487:WIKI_BLOCK_START/END/TEMPLATE + inject_wiki_block/remove_wiki_block/wiki_block_state + install_wiki/uninstall_wiki,复用 fs::write_file_atomic;main.rs:170-184 + 523-532 dispatch);标记 <!-- REPO-WIKI:START/END -->;幂等=完整标记对整块替换/无标记尾部追加/半标记报错;uninstall-wiki 无标记提示"未安装"exit 0;--also-claude 双写开关默认关;8 单测(commands.rs:526-650)+ 5 集成测试(tests/test_install_wiki.rs);真实冒烟:注入保留人工内容/幂等 START=1/卸载恢复/未安装提示
- **G2 Mermaid 校验-重试-降级**:新 src/output/mermaid_check.rs(merman-core 0.7 权威解析;Engine::new()+parse_diagram_sync(strict);MERMAID_RETRY_MAX=2;validate_mermaid_blocks/mermaid_retry_feedback/degrade_mermaid_blocks(坏块→<!-- repo-wiki: mermaid parse failed: 单行 --> + ```text,好块保留);9 单测);generate_wiki_page 合并 citation+mermaid 双校验循环(引用耗尽仍 bail,mermaid 耗尽 degrade 页面保留);新 complete_with_mermaid_guard 供架构/概览(重试耗尽 degrade 不中断);lint 新增第6类检查 bad-mermaid(兜历史产物/人工编辑/增量遗留);真实冒烟:lint 报 Unterminated node label 完整错误,exit=1
- t03 POC 结论:merman-core 0.7.0 错误消息人类可读(LexError message 直接可喂 LLM),已写入 tickets t03 RESOLVED
- 摸到的文件:Cargo.toml(+merman-core="0.7")、Cargo.lock、src/output/mermaid_check.rs(新)、src/output/mod.rs、src/generate/wiki.rs、src/output/lint.rs、src/commands.rs、src/main.rs、tests/test_install_wiki.rs(新)
- 验证基线:**292 lib + 集成全绿**(lib 271→292 新增21:mermaid_check 9 + wiki mermaid 3 + lint bad-mermaid 1 + G1 commands 8)、clippy --all-targets -D warnings 0、全量 cargo test 0 失败
- 已知边界:测试跑完全量后需 git checkout -- wiki/ 恢复(mock 产物泄漏到仓库根 wiki/,既有行为);merman alpha 状态风险已登记(t05 拍板接受)

## 二十二、重大事故记录与恢复(2026-08-03,本会话)
- **事故**:U10 验收时运行 repo-wiki bench(本仓库为评测对象),Update Recall 回放用 git reset --hard 逐 commit 回滚,吞噬 U01-U10 全部未提交改动(20+ src + 10+ tests 文件)
- **根因**:measure_update_recall 无工作区干净检查(评测语义缺陷);已提交工作不受影响
- **止损**:git checkout 75b5918 -- . 恢复工作树 + git update-ref refs/heads/master 75b5918 恢复分支;已提交(G1-G3/MCP/T0-T5/v8-v9)完整恢复;292 lib 测试全绿
- **防护持久化**:src/bench/mod.rs 回放前强制 git statuses 干净检查(非空即 bail),已提交 76739e0
- **损失**:U01-U10 未提交改动丢失,方案记录完整(压缩摘要),需重做;src/bench 文件幸存但接线(lib.rs pub mod bench + main.rs Bench)丢失
- 分析报告:.scratch/research/ANALYSIS-v10-2026-08-03.md(含未完成项全景/优先级/防再丢建议)

## 二十三、wayfinder 建图:U01-U11 重做与完成(v11)(2026-08-03,本会话)
- 触发:事故(U01-U10 被 bench 回放吞噬)后,用户启动 wayfinder 重新规划恢复路线
- 用户拍板(question 两轮 7 项):Destination=重做 U01-U10 + 完成评测 U10/U11 全部;产出=纯规划(地图+决策票+完整实现计划);提交策略=每完成一个 U 立即 git commit(强制执行无需批准);U11=纳入(裁判模型实施时实测选型);顺序=接受 ANALYSIS-v10 9 步路线;bench 安全闸=纯硬闸(脏工作区拒绝,无逃生口);评测仓库=先本仓库,公开仓库进 fog
- 产物:.scratch/wayfinder-v11/map.md + issues/t01-t02(2 research 票)+ IMPLEMENTATION-PLAN.md(9 步完整计划,每 U 含实现要点/文件/测试/提交)
- 地图:Destination=全量落地;frontier=t01/t02(待 fire 子代理);无未决 grilling(全部拍板)
- 摸到的文件:STATUS.md、.scratch/wayfinder-v11/*(新建 4 文件)

## 二十四、U01-U11 全部实现完成(2026-08-03,本会话,wayfinder-v11 落地)
- **11 次提交全落地**:U01(S1 MCP lang 净化,506f66b)→U02(模板动态路径+root 补齐族,879c51b)→U03(D1/D3/D4/D5,f00fa54)→U04(D2/D6/D7/D8+P3,7e698a5)→U05(D9 HTML,210970e)→U06(D11/D12,cc4cff1)→U07(N2-N8,8b17517)→U08(N9-N20,3abd22f)→U09(parser 4 语言,cd28d92)→U10(bench 五维自动评测,86f6991)→U11(TQS LLM 裁判层,3cbb06c)
- **验证基线:321 lib + 118 集成 = 439 测试全绿**、clippy --all-targets -D warnings 干净、cargo machete 零未使用依赖、git 工作树干净
- 新功能:bench 评测子命令(五维自动层+--judge TQS 裁判层)、MCP lang 净化、watch 多根、渲染原子写、fence 精确识别、增量两段式状态、force 退化全量、架构骨架降级、配置边界校验、插件清理、parser 覆盖补齐
- 4 项验证与三张清单详见本会话最终回复;事故恢复路线(wayfinder-v11)全部完成

## 二十五、v11 深度分析报告(2026-08-03,本会话,/goal 会话)
- 方法:本地 62 src 文件全面审计子代理(12 项发现)+ 联网 11 次检索(RepoDoc 四阶段增量/CoREB reranker 结论/MVVP 评测协议/OpenDeepWiki v2/sqlite-knowledge-graph/tree-sitter 新范式)
- **P1×2**:lib.rs:769-772 增量语义索引吞错(新旧向量混存);lib.rs:127-145 状态落盘三重吞错(人工修改保护静默失效,与 lib.rs:81 注释矛盾)
- **P2×5**:bench HEAD 恢复无 RAII guard(事故同源);llm.rs 非真流式;incremental/state 静默降级×2;agent 索引损坏对 MCP 表现无命中
- **P3×5**:build_call_edges 零单测(回归风险最高);bench diff 判定吞错;module.rs 测试残渣;embed filter_map 丢元素;chunk 同名去重边界
- **网络对照结论**:repo-wiki 第一梯队(Karpathy 生态);增量缺"一致性校验"环节(RepoDoc 四阶段);搜索层需评估 reranker(短关键词 nDCG≈0);评测基准补 MVVP 协议(κ+复测+style 消偏)
- 报告:.scratch/research/ANALYSIS-v11-2026-08-03.md(六节:现状/审计 12 项/网络对照/优先级路线/反思/三张清单)

## 二十六、wayfinder 建图:repo-wiki v11 路线全量落地 + Unity 仓库评测(v12)(2026-08-03,本会话)
- 触发:v11 分析报告(12 项发现+网络对照)后用户启动 wayfinder 规划恢复/增强路线
- 用户拍板(question 三轮 13 项):Destination=全部(短中长)+Unity 仓库评测;产出=纯规划;P1-1=传播错误;MVVP 裁判=DeepSeek V4 Flash;reranker=候选对比实测;图谱=research 对照;公开 Rust 集不纳入(只测 Unity);C# parser 提前增强;Unity 仓库=评测+端到端验证;规模=先扫描;长期项全部纳入
- 重要新对象:D:/UnityProjects/Project Strategy(用户 Unity 游戏仓库)——评测主对象+真实大仓库端到端验证(解决历史未验证项)
- 产物:.scratch/wayfinder-v12/map.md + issues/t01-t12(12 票,全 open)+ IMPLEMENTATION-PLAN.md(实施蓝图)
- 地图:Destination=全量落地;frontier=t01-t10,t12;t11 blocked by t06;t08 需 t06 数据(方法论可先行)
- 摸到的文件:STATUS.md、.scratch/wayfinder-v12/*(新建 14 文件)

## 二十七、v12 全部实现完成(2026-08-03,本会话,wayfinder-v12 落地)
- **10 次提交全落地**:t01-t04(短期四项,4effa9f)+t04 clippy(ed1206e)+t07(C# Unity 形态,0225101)+t05(MVVP,346d965)+t09(真流式,87725a9)+t09 修复(06cce95)+t12(reranker 评估,dd9a4f2);t06/t08/t10/t11 为研究/任务票(结论已 resolve 于票)
- **验证基线:327 lib + 118 集成 = 445 测试全绿**、clippy -D warnings 干净、machete 干净
- 关键成果:t06 Unity 仓库探测(2165 .cs/238K LOC)→t07 发现并修复 C# 字段三层结构缺陷(SerializeField 字段此前全部丢失);t08 实测 γ=0.5 在 Unity 仓库产生 526 模块(过细,需下调);t11 mock 全链路验证(526 页/522 卡,generate 333s/export 0.6s);t09 流式 LLM 消除长生成总超时截断
- 决策落地:t10 图谱范式=记录不重构;t12 reranker=不引入(FTS5 短查询基线+论文证据)

## 二十八、v13 深度分析报告(2026-08-03,本会话,/goal 会话)
- 方法:本地 62 src 文件审计子代理 + 联网 11 次检索(2026-08 最新)+ 主代理逐项查证(子代理三项 P1/P2 全部回源属实)
- **P1×1**:t09 真流式纸面化——complete 仍非流式(resp.json().await,llm.rs:361)+ client 总超时仍在(267/414)+ complete_stream 零生产调用;v12 87725a9 声称移除总超时实际未生效(PowerShell 转义失败)
- **P2×3**:TQS κ 混合新旧文档(bench/mod.rs:670 恒取第一份);mermaid degrade 块号与 validate 不一致(嵌套围栏坏块漏降级);collect_sse 逐块边界零测试
- **P3×7**:死代码(agent set_rrf_k/llm:46)、吞错(mod.rs:398)、过时注释、多 declarator 无测试等
- 网络对照:Agent Retrieval Bench edit2ripple(增量评测可借鉴);style bias(0.76-0.92)≫position bias;robust-llm-wiki 分层 lint;Karpathy 生态背书
- 报告:.scratch/research/ANALYSIS-v13-2026-08-03.md(七节:状态/发现/对照/未完成项全景/需求/反思/三清单)
- 教训:445 全绿测试覆盖纸面路径;声称完成必须有真实调用点证据;语义级测试(非存在性断言)

## 二十九、v13 全量落地完成(2026-08-04,本会话,wayfinder-v13)
- **A 组 P1 正确性 10 项(f8ae1c8)**:A1 首次 update 无基线回退全量(原静默空 diff 短路,wiki 恒空);A2 incremental 禁用时 update 真正全量;A3 mermaid 依赖图/调用图确定性排序(原 HashSet/HashMap 字节漂移);A4 LLM 生产路径统一流式(complete 默认实现委托 complete_stream,移除 client 120s/180s 总超时,SSE 60s 空闲超时保护——v12 t09 纸面化修复,附 A4 回归 6469fea:test_cli mock 改 SSE);A5 文档指纹读失败保守计入保护集;A6 状态读取失败显式告警;A7 插件全部工具补 --root;A8 卡片恢复/反向同步读失败告警+被删卡片不重建;A9 semantic 吞错链修复+hybrid 降级 warn;A10 文档修复(README/CONTEXT/fixture/测试注释)
- **E 组 全局配置链(3e30f9d,用户拍板)**:新 global_config_dir(Windows %APPDATA%/repo-wiki,其他 ~/repo-wiki,USERPROFILE 优先);默认配置搜索链=项目级→全局→创建全局;15 处 --config 改 Option 全部入口统一 resolve;Init 缺省走链创建全局;Bench config 走链;测试隔离核查(全部 CLI 测试显式 --config,不触达真实 APPDATA)
- **B 组 P2 可维护性 11 项(c9039f0)**:B1/B2 已于 A 组完成;死代码 4 处(SearchAgent::text_engine/set_rrf_k、NodeKind::priority、ParserRegistry::supported_extensions);bench diff 吞错 warn+保守计入;ScanOutput{insights,files_failed} 解析失败统计+AnalysisStats.files_failed;embed 非数字元素显式 bail;lint.rs 六类检查各一函数(每函数<60 行);tests/common/mod.rs helper 收敛(9 文件去重);benches root 参数化;update --progress-json 冒烟
- **C 组 P3 卫生 3 项(eabfa0a)**:测试残渣(module.rs/agent.rs/integration_test 死变量)+lint 5 处读失败 warn+semantic 注释;wiki/ 整体 .gitignore+git rm --cached(仓库根 wiki/ 是 mock 泄漏产物,真实产物在 .repo-wiki/);P3-4 八项核对补齐 2 测试(卡片读失败不中断/被删卡片不重建)
- **D 组(c424210)**:D1 stale-entity 符号漂移 lint(api.md 清单实体∉源码 AST→报错,entity-coverage 反向);D2 update 尾部 lint 全量复核 warn 不阻断;D3 前置查证=seed 本就固定(LEIDEN_SEED=42 双处),detect_communities_with_resolution 参数化+新 benchmarks/gamma_scan.rs 评测工具;γ 实测(Unity 2950 文件):γ 0.2-0.6→模块 521-724(差异≤25%),单文件占比 59-69% 不敏感——低模块化是图结构特性,维持默认 0.5,t08 结论修正
- **验证基线:463 passed**(346 lib + 117 集成,较 v12 445 +18)、clippy -D warnings 0、machete 干净、工作树干净
- 已知边界:wiki_note 需二进制在 PATH(本机 G1 环境问题,已用 cargo run 完成);wiki 自更新需真实 LLM key 未执行;γ 扫描工具保留于 benches/gamma_scan.rs 供大仓库复测
