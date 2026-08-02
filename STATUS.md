# 项目状态简报 （AI自动维护，禁止贴代码）

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

### 历史记录（T0-T5 深度演进 + 航图前两轮，已完成）
- T0-T4 全部 15 项实现完成（社区检测聚类/实体级变化分类与语义传播/生成并行化/失败隔离/反向链接/CoT/评测基准/测试缺口/文档修正）；T5 清零计划全完成
- 航图第一轮：hook/插件闭环（04/05）+ provider 统一（13）

## 三、已知风险点（由AI诚实自曝）
- CPM 分辨率 γ=0.5/0.4 由小仓库与合成图实测选定，真实大仓库（万级文件）社区粒度未验证，需实测调参
- 特征聚类的 embedding 注入路径（0.5 语义权重）无 API key 未真实验证，仅验证纯结构降级路径
- 真实 LLM 全量生成（大仓库）端到端产物未验证；watch 端到端未验证
- tests/fixtures/sample-repo/config.toml 是真实 provider（无 base_url）——直接复用跑 generate 会触网（现有测试均在临时副本改写为 mock，无测试触网）
- insights 缓存（票 12）体积 = 全仓库实体元数据 + 源码文本，超大仓库磁盘占用未实测；缓存损坏自动全量重建（可观测性契约内）
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
