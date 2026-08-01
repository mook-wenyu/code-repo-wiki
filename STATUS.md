# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：16（analysis 拆出 community/feature 两个子模块；incremental 新增 change）
- 违规跨模块调用：无
- 测试覆盖率：cargo clippy --all-targets -- -D warnings 0 警告；cargo test 267 通过 0 失败（207 lib + 60 集成，17 套件）；bench 6 项全过
- 代码量：约 13,400 行 / 60 .rs 文件（演进新增 ~2,700 行）

## 二、本次变更影响范围
- 修改的功能：深度演进计划 T0-T4 全部 15 项实现完成——图社区检测聚类、实体级变化分类与语义传播、生成并行化、失败隔离、反向链接与特征追溯、CoT 提示、评测基准、测试缺口补全、文档修正
- **T0 配置/排序修复**：default-config.toml 删除 [project]/[generate] 死键对齐 schema 8 段；RRF 移除引擎偏移改纯 rank（hybrid.rs）
- **T1 聚类升级**：leiden-rs 0.8.1（CPM γ=0.5 seed=42）文件级社区检测替换目录前缀凝聚聚类（community.rs）；实体级特征聚类（feature.rs，Embedder trait DIP 注入 + embedding 失败降级纯结构）；确定性命名三档（公共前缀→最多目录→module_{n}）；petgraph 0.7→0.8
- **T2 实体级增量**：change.rs 新旧实体对比分类（Added/Removed/SignatureChanged/BodyChanged，git2 读旧 commit 重解析）；impact.rs 语义传播（接口级双向、实现级仅本模块）；Chunk 加 entity_sources；增量仅接口级变化实体重生成摘要
- **T3 生成增强**：实体摘要与 Wiki 页面 Semaphore 可控并发（join_all 保序）；失败隔离（failed_modules 进 GenerationStats）；反向链接 [源码:path:start-end] + 卡片特征追溯节；CoT 分步推理引导（输出格式不变）
- **T4 评测与测试**：bench_clustering_detection（20 簇合成仓库验证还原度+确定性，200 文件 247ms）；llm.rs 请求构建/SSE/重试/Anthropic 请求体测试（AnthropicProvider 加 base_url 字段）；semantic 伪向量真实检索+阈值测试；chunk 分组测试；CLI status/note/init/sync/search 冒烟（tests/test_cli_smoke.rs）；CONTEXT.md 修正 sqlite-vec 失实描述
- 摸到的文件：src/analysis/{community,feature,module,mod}.rs、src/incremental/{change,impact,mod}.rs、src/generate/{chunk,card,wiki,prompt,mod,llm,embed}.rs、src/model/mod.rs、src/model/document.rs、src/search/{hybrid,semantic}.rs、src/output/{markdown,html,mod}.rs、src/lib.rs、Cargo.toml、default-config.toml、CONTEXT.md、README.md、benches/bench_search.rs、tests/（3 新文件）
- 是否改变了接口/契约：是（未上线无存量用户，已批准不向后兼容）——EmbeddingEngine::new 增加 rt 参数；AnthropicProvider 增加 base_url 字段；run_generation_filtered 增加 entity_changes 参数；KnowledgeCard/EntitySummary 增加字段；KnowledgeGraph 增加 features；模块命名从目录前缀改为社区检测

## 三、已知风险点（由AI诚实自曝）
- CPM 分辨率 γ=0.5/0.4 由小仓库与合成图实测选定，真实大仓库（万级文件）社区粒度未验证，需实测调参
- 特征聚类的 embedding 注入路径（0.5 语义权重）无 API key 未真实验证，仅验证纯结构降级路径
- 实体级变化分类依赖 git 仓库 + 旧 commit 内容可解析（文件级删除/重命名不分类实体）
- status 命令是桩实现（只打印就绪+配置路径，不检查产物）
- tests/fixtures/sample-repo/config.toml 是 openai provider + api_key="mock" 无 base_url——直接复用该 fixture 跑 generate 会真实触网（现有测试均在临时副本改写为 mock）
- 真实 LLM 全量生成（大仓库）端到端产物未验证；watch 端到端未验证

## 四、下次最该做的事（AI建议）
1. 大仓库（万级文件）实测社区检测粒度与 LLM 全量生成耗时，按实测调 CPM resolution
2. status 命令补真实产物检查（当前桩实现）；sample-repo fixture 配置改为 mock provider
