pub mod card;
pub mod chunk;
pub mod context;
pub mod embed;
pub mod index;
pub mod llm;
pub mod prompt;
pub mod schema;
pub mod wiki;

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::ingest::parser::FileInsight;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::card::CardGenerator;
use self::chunk::Chunk;
use self::context::{CallerContext, DependencyContext};
use self::llm::{AnthropicProvider, LlmProvider, OpenAiProvider, Provider};
use self::wiki::WikiGenerator;

/// 生成流水线的输出
pub struct GenerationOutput {
    pub cards: Vec<KnowledgeCard>,
    pub documents: Vec<WikiDocument>,
    pub generation_stats: GenerationStats,
    /// v32 8.1：分块/卡片/Wiki 页三段的内部计时（毫秒）——上层
    /// run_pipeline_with_progress 收集后落盘供 bench 回放剖析
    pub timings: crate::GenerationTimings,
}

/// 生成统计信息
#[derive(Debug, Clone, Default)]
pub struct GenerationStats {
    pub total_tokens_used: usize,
    pub llm_calls: usize,
    pub generation_time_ms: u64,
    /// 生成失败的模块名列表（演进计划 T3.2 失败隔离的可见性出口）
    pub failed_modules: Vec<String>,
}

/// 根据配置创建 LLM Provider（v17 t02：协议按 provider 类型显式绑定）
pub fn create_provider(config: &WikiConfig) -> Result<Provider> {
    match config.llm.provider {
        // openai = OpenAI Responses API 协议（base_url 可配，DeepSeek 归此）
        crate::config::schema::LlmProviderType::OpenAI => Ok(Provider::OpenAi(
            OpenAiProvider::new(&config.llm, crate::generate::llm::OpenAiProtocol::Responses)?,
        )),
        crate::config::schema::LlmProviderType::Anthropic => {
            Ok(Provider::Anthropic(AnthropicProvider::new(&config.llm)?))
        }
        // openai-compatible = chat/completions 协议（custom 并入，v17 t02）
        crate::config::schema::LlmProviderType::OpenAiCompatible => Ok(Provider::OpenAi(
            OpenAiProvider::new(&config.llm, crate::generate::llm::OpenAiProtocol::Chat)?,
        )),
        crate::config::schema::LlmProviderType::Mock => {
            // 本地模拟：测试/CI/无 API Key 场景，返回固定文本
            Ok(Provider::Mock(crate::generate::llm::MockProvider::new()))
        }
    }
}

/// 构建卡片阶段的模块级上下文（模块名 → 上下文）
///
/// 卡片并行生成，拿不到依赖方卡片摘要 → 摘要恒 None（只给模块名）；
/// 调用方/被调用方上下文 = collect_caller_context（真实调用图推导，
/// 卡片 design_rationale 的依据）。返回 (dep_contexts, caller_contexts)
/// 两表，与 CardGenerator 新增字段一一对应。
fn build_card_contexts(
    chunks: &[Chunk],
    graph: &KnowledgeGraph,
) -> (
    HashMap<String, Vec<DependencyContext>>,
    HashMap<String, Vec<CallerContext>>,
) {
    let empty_cards: HashMap<String, &crate::model::KnowledgeCard> = HashMap::new();
    let mut dep_contexts: HashMap<String, Vec<DependencyContext>> = HashMap::new();
    let mut caller_contexts: HashMap<String, Vec<CallerContext>> = HashMap::new();
    for chunk in chunks {
        let module = chunk.module_path.join("::");
        dep_contexts.insert(
            module.clone(),
            context::build_dependency_contexts(chunk, &empty_cards, &|_| None),
        );
        caller_contexts.insert(
            module,
            vec![context::collect_caller_context(chunk, graph, &|_| None)],
        );
    }
    (dep_contexts, caller_contexts)
}

/// 运行完整的生成流水线
///
/// 1. AST 感知分块（按模块分组）
/// 2. 并行生成 Knowledge Card
/// 3. 串行生成 Wiki 页面（依赖前序卡片摘要）
/// 4. 生成架构概览页面
///
/// extra_edits：本次运行新检测到的人工修改记录（模块名 → 记录文本），
/// 生成卡片前注入 LLM 输入（见 CardGenerator::generate_all_cards）；
/// 由上层（lib.rs）从状态指纹比对结果组装，无人工修改时传空表。
///
/// plan（v0.9 W1）：wiki_plan.yaml 解析后的生效计划（None=无 plan 文件，
/// 保持默认行为）；notes/template/documents 由本函数接入生成路径。
/// 注意：scope 覆盖已由调用方（lib.rs）在扫描阶段消费，这里只消费
/// notes/template/documents，不在本函数内二次解析 plan 文件。
pub async fn run_generation(
    graph: &KnowledgeGraph,
    insights: &[FileInsight],
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    extra_edits: &HashMap<String, Vec<String>>,
    on_progress: &dyn Fn(crate::ProgressEvent),
    plan: Option<&crate::config::plan::ResolvedPlan>,
) -> Result<GenerationOutput> {
    let start = Instant::now();
    // v32 8.1：三段内部计时（chunk/card/wiki）
    let chunk_start = Instant::now();

    // 1. AST 感知分块
    let chunks = if graph.modules.is_empty() {
        tracing::warn!("未检测到模块聚类，回退到文件级分块");
        insights
            .iter()
            .map(chunk::chunk_by_file)
            .collect::<Vec<_>>()
    } else {
        chunk::chunk_by_module(insights, &graph.modules, graph)
    };
    // v31 修复（C-03）：分块后统一剔除空 chunk——chunk_by_module 对全部模块
    // 产 chunk，增量只喂变更文件时未变更模块 chunk 为空；空 chunk 是确定性
    // 「无内容」而非生成失败，若放行会在生成循环里被空块 bail 记入
    // failed_modules（毒化 should_skip_noop 并引发无关模块补偿重试），且
    // 过滤必须发生在管线入口，保证 chunks/cards/wiki/backfill 全链路 1:1 对齐。
    let chunks: Vec<_> = chunks.into_iter().filter(|c| !c.is_empty()).collect();
    tracing::info!("生成进度: 30% - 分块完成，共 {} 个块", chunks.len());
    let chunk_ms = chunk_start.elapsed().as_millis() as u64;

    // 2. 创建 LLM Provider
    let provider = create_provider(config)?;

    // v0.9 W1：plan 接入点——notes（repowiki）注入 Wiki 页 prompt、
    // card_notes（knowledgecard）注入卡片 prompt、template 决定模块页模板、
    // documents 生成自定义页面。None 时全部回退默认（零破坏）。
    let wiki_notes = plan.map(|p| p.notes.clone()).unwrap_or_default();
    let card_notes = plan.map(|p| p.card_notes.clone()).unwrap_or_default();
    let template = plan.map(|p| p.template).unwrap_or_default();

    // 3. 并行生成 Knowledge Card
    let card_start = Instant::now();
    // 项目级上下文：卡片阶段摘要恒 None（并行拿不到依赖方卡片）
    let (card_dep_contexts, card_caller_contexts) = build_card_contexts(&chunks, graph);
    let card_gen = CardGenerator::new_with_card_notes(
        &provider,
        config.clone(),
        crate::config::schema::llm_effective_concurrency(&config.llm),
        config.wiki.language.clone(),
        card_dep_contexts,
        card_caller_contexts,
        card_notes,
    );
    let mut cards = card_gen
        .generate_all_cards(&chunks, extra_edits, on_progress)
        .await?;
    // 特征追溯回填（演进计划 T3.3）：模块实体与特征实体的交集 → 特征名
    backfill_features(&mut cards, &chunks, graph);
    tracing::info!(
        "生成进度: 60% - 知识卡片生成完成，共 {} 个卡片",
        cards.iter().flatten().count()
    );
    let card_ms = card_start.elapsed().as_millis() as u64;

    // 4. 按语言独立生成 Wiki 页面（并行，演进计划 T3.1；卡片仅主语言生成一次，
    // 各语言页面复用主语言卡片摘要；语言列表在 generate_wiki_pages 内部计算）
    let wiki_start = Instant::now();
    let wiki_gen = WikiGenerator::new_with_plan(
        &provider,
        crate::config::schema::llm_effective_concurrency(&config.llm),
        wiki_notes,
        template,
    );
    let mut documents = generate_wiki_pages(
        &wiki_gen,
        &chunks,
        &cards,
        config,
        crate::config::schema::llm_effective_concurrency(&config.llm),
        root,
        graph,
        &build_entity_ranges(insights),
        on_progress,
    )
    .await;
    let wiki_ms = wiki_start.elapsed().as_millis() as u64;

    // 5. 生成全局文档（架构概览 + 数据库 Schema + plan 自定义文档，
    // 全量/增量共用同一辅助函数）
    generate_global_documents(
        &wiki_gen,
        &provider,
        graph,
        config,
        root,
        &cards,
        &mut documents,
        &GlobalDocAffected::all(),
        false,
        plan,
    )
    .await?;

    let elapsed = start.elapsed();
    let stats = GenerationStats {
        llm_calls: card_gen.llm_call_count() + wiki_gen.llm_call_count(),
        generation_time_ms: elapsed.as_millis() as u64,
        // 失败隔离统计（T3.2）：卡片与页面两路失败模块名合并
        failed_modules: {
            let mut f = card_gen.failed_modules();
            f.extend(wiki_gen.failed_modules());
            f
        },
        ..Default::default()
    };

    Ok(GenerationOutput {
        cards: cards.iter().flatten().cloned().collect(),
        documents,
        generation_stats: stats,
        timings: crate::GenerationTimings {
            chunk_ms,
            card_ms,
            wiki_ms,
            ..Default::default()
        },
    })
}

/// 增量更新的过滤生成流水线
///
/// 与 `run_generation` 类似，但仅处理 `inc`（增量分析结果）中列出的变更
/// 文件 + 语义传播判定的受影响模块，用于增量更新场景。未变更的文件
/// 使用已有缓存，不触发新的 LLM 调用。
/// extra_edits 语义同 run_generation（本次新检测的人工修改记录）。
/// plan 语义同 run_generation（v0.9 W1：notes/template/documents 接入，
/// scope 由 lib.rs 在扫描阶段消费）。
// 例外说明（复杂度红线 5 参数规则）：8 个参数均为相互独立的上下文项
// （图/输入/配置/根/增量/人工修改/进度回调/plan），引入包装结构体反而
// 降低调用点可读性；与 run_generation 同构，属明确例外。
#[allow(clippy::too_many_arguments)]
pub async fn run_generation_filtered(
    graph: &KnowledgeGraph,
    insights: &[FileInsight],
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    inc: &crate::incremental::IncrementalResult,
    extra_edits: &HashMap<String, Vec<String>>,
    on_progress: &dyn Fn(crate::ProgressEvent),
    plan: Option<&crate::config::plan::ResolvedPlan>,
) -> Result<GenerationOutput> {
    let start = Instant::now();
    let changed_files = &inc.changed_files;
    let entity_changes = &inc.entity_changes;
    let affected_modules = &inc.affected_modules;
    // v32 8.1：三段内部计时
    let chunk_start = Instant::now();

    // 过滤出变更文件的 Insight（克隆为拥有数据）。
    // T2 传播闭环接线：除变更文件外，语义传播判定的受影响模块文件也
    // 并入生成范围——签名/删除等接口级变化会重生成依赖方模块的文档，
    // 实现级变化（body-only）传播结果只含本模块，行为不变。
    let affected_files = crate::incremental::impact::module_files(affected_modules, graph);
    // v23 A1 实体级分类：从生成范围排除「无实体变更」文件——git diff 报告了
    // 变化（文件在 changed_files），但实体级分类（change.rs 三元组全等判定）
    // 未产出任何该文件的变更记录（纯注释/空白/换行符变化）。
    // added/deleted 文件必有 Added/Removed 记录、接口级变化必有记录，故
    // 无记录 = 仅非实体文本变化，模块页无需重生成（旧产物内容与行号引用
    // 均仍准确）。排除后落入下方空集分支走快照回填（零 LLM 保留旧产物）。
    // 与 incremental/mod.rs 的传播起点剔除共用同一函数，保证两处同口径。
    let no_entity_change_files =
        crate::incremental::change::no_entity_change_files(changed_files, entity_changes, root);
    let mut changed_insights: Vec<FileInsight> = insights
        .iter()
        .filter(|f| {
            (changed_files.contains(&f.path) || affected_files.contains(&f.path))
                && !no_entity_change_files.contains(&f.path)
        })
        .cloned()
        .collect();

    // 纯删除场景的模块级补偿（v51 拆分为独立函数 compensate_deleted_files，
    // 行为不变）：被删文件所属模块的存活文件并入变更集，走正常重生成
    // 清除被删实体的页面残留。补偿必须独立于下方空集回填分支执行（mixed
    // 场景 deleted 与 modified 并存时 changed_insights 非空、回填分支不进入），
    // 详见函数注释。
    compensate_deleted_files(changed_files, insights, root, config, &mut changed_insights);

    if changed_insights.is_empty() {
        // 空集场景（v23 A1 起含「无实体变更」文件：纯空白/注释/换行符变化
        // 被实体级分类排除；v21 验证轮起含「整模块全删」：删除补偿未命中
        // 任何部分删除模块）：changed_files 非空但无文件命中影响集时，旧实现
        // 直接返回空输出 → render_all 不写任何产物 → cleanup_stale_outputs
        // 差集语义把**全部**旧产物清空（无关模块页也被删）。
        // v51 拆分为独立函数 snapshot_backfill：快照可用时回填未删除模块的
        // 旧产物（零 LLM 成本）；快照缺失（异常）时返回 None 回退全量生成，
        // 宁可多生成也不丢数据。
        if let Some(output) = snapshot_backfill(changed_files, root, config)? {
            return Ok(output);
        }
        changed_insights = insights.to_vec();
    } else {
        tracing::info!("增量生成: {} 个文件变更", changed_insights.len());
    }

    // 1. AST 感知分块（仅变更文件）
    let chunks: Vec<_> = if graph.modules.is_empty() {
        changed_insights.iter().map(chunk::chunk_by_file).collect()
    } else {
        // 按模块重新组织变更文件，保持模块上下文
        chunk::chunk_by_module(&changed_insights, &graph.modules, graph)
    };
    // v31 修复（C-03）：同全量路径——管线入口剔除空 chunk（增量模式未变更
    // 模块），保证 chunks/cards/wiki/backfill 全链路 1:1 对齐，且空 chunk
    // 不会经空块 bail 污染 failed_modules。
    let chunks: Vec<_> = chunks.into_iter().filter(|c| !c.is_empty()).collect();
    tracing::info!("增量分块完成: {} 个块", chunks.len());
    let chunk_ms = chunk_start.elapsed().as_millis() as u64;

    // 2. 创建 LLM Provider
    let provider = create_provider(config)?;

    // v0.9 W1：plan 接入点（语义同 run_generation，见其上注释）
    let wiki_notes = plan.map(|p| p.notes.clone()).unwrap_or_default();
    let card_notes = plan.map(|p| p.card_notes.clone()).unwrap_or_default();
    let template = plan.map(|p| p.template).unwrap_or_default();

    // 3. 并行生成 Knowledge Card（仅变更块）
    let card_start = Instant::now();
    let (card_dep_contexts, card_caller_contexts) = build_card_contexts(&chunks, graph);
    let card_gen = CardGenerator::new_with_card_notes(
        &provider,
        config.clone(),
        crate::config::schema::llm_effective_concurrency(&config.llm),
        config.wiki.language.clone(),
        card_dep_contexts,
        card_caller_contexts,
        card_notes,
    );
    let mut cards = card_gen
        .generate_all_cards(&chunks, extra_edits, on_progress)
        .await?;
    // 特征追溯回填（演进计划 T3.3）：模块实体与特征实体的交集 → 特征名
    backfill_features(&mut cards, &chunks, graph);
    let card_ms = card_start.elapsed().as_millis() as u64;

    // 4. 按语言独立生成 Wiki 页面（并行，演进计划 T3.1；仅变更块；卡片仅主语言生成一次，
    // 各语言页面复用主语言卡片摘要）
    let wiki_start = Instant::now();
    let wiki_gen = WikiGenerator::new_with_plan(
        &provider,
        crate::config::schema::llm_effective_concurrency(&config.llm),
        wiki_notes,
        template,
    );
    let mut documents = generate_wiki_pages(
        &wiki_gen,
        &chunks,
        &cards,
        config,
        crate::config::schema::llm_effective_concurrency(&config.llm),
        root,
        graph,
        &build_entity_ranges(insights),
        on_progress,
    )
    .await;
    tracing::info!(
        "生成进度: 90% - Wiki 页面生成完成，共 {} 个页面",
        documents.len()
    );
    let wiki_ms = wiki_start.elapsed().as_millis() as u64;

    // 5. 生成全局文档（架构概览 + 数据库 Schema）
    // P1-2 全局文档增量（受影响判断）：架构/概览只在接口级实体变化
    // （新增/删除/签名变更）时重生成——纯实现级（body-only）变化不改变
    // 模块间依赖视图；Schema 只在本次变更含 .sql 文件时重生成。未受影响的
    // 全局文档从导出快照回填旧版（零 LLM 成本，渲染幂等不误判人工修改），
    // 快照不可用时回退生成保证页面存在性。全量路径恒全受影响（all()）。
    let global_affected = GlobalDocAffected {
        architecture: entity_changes.has_interface_change(),
        schema: changed_files
            .iter()
            .any(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql"))),
    };
    generate_global_documents(
        &wiki_gen,
        &provider,
        graph,
        config,
        root,
        &cards,
        &mut documents,
        &global_affected,
        inc.has_deleted_files,
        plan,
    )
    .await?;

    let elapsed = start.elapsed();
    let stats = GenerationStats {
        llm_calls: card_gen.llm_call_count() + wiki_gen.llm_call_count(),
        generation_time_ms: elapsed.as_millis() as u64,
        // 失败隔离统计（T3.2）：卡片与页面两路失败模块名合并
        failed_modules: {
            let mut f = card_gen.failed_modules();
            f.extend(wiki_gen.failed_modules());
            f
        },
        ..Default::default()
    };

    // DEFECT-A 修复：增量非空路径未受影响模块回填。
    // 旧实现只把「受影响模块 + 全局文档」放进输出，未受影响仍存在的
    // 模块没有从导出快照回填 → llms.txt/_toc.md/export_snapshot.json/
    // generation_state 全以部分集合为文档集（llms.txt 是全站地图，
    // 任何一次生成含增量都必须是全模块集合）。回填后 documents/cards
    // 语义 =「当前完整文档集」，下游 render_all/cleanup/save_generation_state
    // 自动恢复正确。快照缺失/损坏时跳过合并，本次集合照常返回。
    let mut cards: Vec<KnowledgeCard> = cards.iter().flatten().cloned().collect();
    backfill_unchanged_modules(
        config,
        root,
        &mut cards,
        &mut documents,
        &stats.failed_modules,
    );

    Ok(GenerationOutput {
        cards,
        documents,
        generation_stats: stats,
        timings: crate::GenerationTimings {
            chunk_ms,
            card_ms,
            wiki_ms,
            ..Default::default()
        },
    })
}

/// 纯删除场景的模块级补偿（v51 拆分自 run_generation_filtered，行为不变）
///
/// 被删文件所属模块（快照卡片 related_files 含被删文件且仍有存活
/// 文件的卡片 = 部分删除模块）的存活文件并入变更集，走正常重生成
/// 清除被删实体的页面残留。
///
/// 补偿必须独立于空集回填分支执行：删除与修改并存（mixed）时
/// changed_insights 非空，回填分支不进入——而语义传播的起点（被删
/// 文件）在当前图中无节点（impact.rs find_start_nodes 找不到即跳过），
/// 其模块永远进不了 affected_modules，不显式并入则模块页残留旧内容。
/// 纯删除场景由本逻辑并入后同样落入正常生成路径；快照缺失/损坏时
/// 跳过补偿（空集回填分支对快照失败有全量回退兜底，不丢数据）。
/// 模块归属沿用快照 cards.related_files（与 v22 失败补偿同源机制）。
fn compensate_deleted_files(
    changed_files: &[std::path::PathBuf],
    insights: &[FileInsight],
    root: &crate::project::ProjectRoot,
    config: &WikiConfig,
    changed_insights: &mut Vec<FileInsight>,
) {
    let deleted_files: std::collections::HashSet<std::path::PathBuf> = changed_files
        .iter()
        .filter(|f| !root.path().join(f).exists())
        .cloned()
        .collect();
    let surviving_files: std::collections::HashSet<std::path::PathBuf> = if deleted_files.is_empty()
    {
        std::collections::HashSet::new()
    } else if let Ok(content) =
        std::fs::read_to_string(crate::output::export_snapshot_path(config.output_dir()))
        && let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content)
    {
        snapshot
            .cards
            .iter()
            .filter(|c| {
                !c.related_files.is_empty()
                    && c.related_files
                        .iter()
                        .any(|f| deleted_files.contains(Path::new(f)))
                    && c.related_files.iter().any(|f| root.path().join(f).exists())
            })
            .flat_map(|c| c.related_files.iter().map(std::path::PathBuf::from))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    if !surviving_files.is_empty() {
        let mut present: std::collections::HashSet<std::path::PathBuf> =
            changed_insights.iter().map(|i| i.path.clone()).collect();
        let mut merged = 0usize;
        for insight in insights {
            if surviving_files.contains(&insight.path) && present.insert(insight.path.clone()) {
                changed_insights.push(insight.clone());
                merged += 1;
            }
        }
        if merged > 0 {
            tracing::info!(
                "增量生成: 删除文件所属模块的 {} 个存活文件并入变更集，重生成清除被删实体残留",
                merged
            );
        }
    }
}

/// 空集场景的快照回填（v51 拆分自 run_generation_filtered，行为不变）
///
/// 增量变更无文件命中影响集时（纯删除/纯文本变化）：changed_files 非空
/// 但无文件命中影响集，旧实现直接返回空输出 → render_all 不写任何产物 →
/// cleanup_stale_outputs 差集语义把全部旧产物清空（无关模块页也被删）。
/// 本函数从导出快照回填未删除模块的旧产物（零 LLM 成本）；快照缺失
/// （异常）时返回 None，由调用方回退全量生成，宁可多生成也不丢数据。
fn snapshot_backfill(
    changed_files: &[std::path::PathBuf],
    root: &crate::project::ProjectRoot,
    config: &WikiConfig,
) -> Result<Option<GenerationOutput>> {
    if let Ok(content) =
        std::fs::read_to_string(crate::output::export_snapshot_path(config.output_dir()))
        && let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content)
    {
        // 快照回填：仅剔除整模块全删（related_files 全部不存在）的卡片与
        // 文档——部分删除模块的存活文件已在上方并入变更集走重生成，此处
        // 到达的只有「真无变更可生成」的文件，原样回填旧产物。
        let deleted_modules: std::collections::HashSet<String> = snapshot
            .cards
            .iter()
            .filter(|c| {
                !c.related_files.is_empty()
                    && c.related_files
                        .iter()
                        .all(|f| !root.path().join(f).exists())
            })
            .map(|c| c.module_name.clone())
            .collect();
        let cards: Vec<KnowledgeCard> = snapshot
            .cards
            .into_iter()
            .filter(|c| !deleted_modules.contains(&c.module_name))
            .collect();
        let documents: Vec<WikiDocument> = snapshot
            .documents
            .into_iter()
            .filter(|d| !deleted_modules.contains(&d.title))
            .collect();
        tracing::info!(
            "增量生成: 空集场景（{} 个变更文件），从快照回填 {} 文档 {} 卡片（跳过已删模块 {} 个）",
            changed_files.len(),
            documents.len(),
            cards.len(),
            deleted_modules.len()
        );
        Ok(Some(GenerationOutput {
            cards,
            documents,
            generation_stats: GenerationStats::default(),
            timings: crate::GenerationTimings::default(),
        }))
    } else {
        tracing::warn!("增量生成: 纯删除场景但导出快照缺失，回退全量生成防止产物误清");
        Ok(None)
    }
}

/// DEFECT-A 修复：增量非空路径的未受影响模块回填
///
/// 根因：`run_generation_filtered` 增量非空路径只把「受影响模块 + 全局
/// 文档」放进 GenerationOutput，未受影响仍存在的模块没有从导出快照回填
/// → llms.txt/_toc.md/export_snapshot.json/generation_state 全以部分集合
/// 为文档集。llms.txt 是全站地图（llmstxt.org v2）——任何一次生成（含
/// 增量）都必须是全模块集合。
///
/// 本函数在返回 GenerationOutput 前调用，把「未受影响且仍存在」的模块
/// 文档/卡片从导出快照合并进文档集，使增量路径返回「完整当前文档集」，
/// 下游 render_all/cleanup/save_generation_state 自动恢复正确。
///
/// 规则（对齐审计定位的缺陷固化测试改造方案）：
/// 1. `deleted_modules` = 快照 cards 中 `related_files` **全部不存在于磁盘**
///    的模块（复用 snapshot_backfill 的 deleted_modules 判据）。`related_files`
///    为空（由 chunk 直接填充，缺失即模块归属信息不可靠）的卡片**保守处理：
///    不并入也不判删**——判删可能误删仍在使用的模块页（审计自曝风险点）。
/// 2. 卡片：快照 cards 中 `module_name ∉ deleted_modules` 且不在本次生成
///    cards → 并入（去重锚点 `module_name`）。
/// 3. 文档：快照 documents 中 `kind == WikiPage`、`title ∉ deleted_modules`、
///    `title` 不在本次生成 documents、`language == config.wiki.language`
///    → 并入（锚点 title+language）。
/// 4. 排除 `failed_modules` 中模块（失败即缺失信号，不回填旧文档防掩盖失败）。
/// 5. 全局文档不动（已由 backfill_global_docs/index 覆盖）。
/// 6. 快照缺失/损坏时跳过合并（本次集合照常返回，不阻断）。
///
/// 放 generate 层（与空集场景 snapshot_backfill 同层内聚），使增量路径
/// 返回「完整当前文档集」，下游 render_all/cleanup/save_generation_state
/// 自动恢复正确。
fn backfill_unchanged_modules(
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    cards: &mut Vec<KnowledgeCard>,
    documents: &mut Vec<WikiDocument>,
    failed_modules: &[String],
) {
    let snapshot_path = crate::output::export_snapshot_path(config.output_dir());
    let Ok(content) = std::fs::read_to_string(&snapshot_path) else {
        return;
    };
    let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content) else {
        tracing::warn!(
            "导出快照解析失败（未受影响模块回填跳过，本次集合照常返回）: {}",
            snapshot_path.display()
        );
        return;
    };

    let failed: std::collections::HashSet<&str> =
        failed_modules.iter().map(String::as_str).collect();
    // 预构建「本次生成集合」的锚点集（owned 克隆——集合引用要跨 mutate
    // 存活，借引用会导致 push 阶段 E0502）：
    // - 卡片锚点 module_name；
    // - 文档锚点 (title, language)。
    let current_card_modules: std::collections::HashSet<String> =
        cards.iter().map(|c| c.module_name.clone()).collect();
    let current_doc_titles: std::collections::HashSet<(String, String)> = documents
        .iter()
        .map(|d| (d.title.clone(), d.language.clone()))
        .collect();

    // 已删模块：快照 cards 中 related_files 全部不存在于磁盘的模块。
    // related_files.is_empty() 保守处理（不并入也不判删）——related_files
    // 由 chunk 直接填充，为空是模块归属信息缺失信号，判删可能误删仍在
    // 使用的模块页（审计自曝风险点）。
    let deleted_modules: std::collections::HashSet<String> = snapshot
        .cards
        .iter()
        .filter(|c| {
            !c.related_files.is_empty()
                && c.related_files
                    .iter()
                    .all(|f| !root.path().join(f).exists())
        })
        .map(|c| c.module_name.clone())
        .collect();

    let cards_before = cards.len();
    let docs_before = documents.len();
    let mut merged_cards = 0usize;
    let mut merged_docs = 0usize;

    for card in &snapshot.cards {
        if card.related_files.is_empty()
            || deleted_modules.contains(&card.module_name)
            || failed.contains(card.module_name.as_str())
            || current_card_modules.contains(&card.module_name)
        {
            continue;
        }
        cards.push(card.clone());
        merged_cards += 1;
    }
    for doc in &snapshot.documents {
        if doc.kind != crate::model::DocumentKind::WikiPage
            || deleted_modules.contains(&doc.title)
            || failed.contains(doc.title.as_str())
            || doc.language != config.wiki.language
            || current_doc_titles.contains(&(doc.title.clone(), doc.language.clone()))
        {
            continue;
        }
        documents.push(doc.clone());
        merged_docs += 1;
    }

    if merged_cards > 0 || merged_docs > 0 {
        tracing::info!(
            "增量生成: 未受影响模块回填 {} 卡片 {} 文档（本次生成 {} 卡片 {} 文档，已删模块 {} 个，失败模块 {} 个）",
            merged_cards,
            merged_docs,
            cards_before,
            docs_before,
            deleted_modules.len(),
            failed.len()
        );
    }
}

/// 从全仓库解析结果构建"相对路径 → 实体行区间列表"表（v14 B 组）
///
/// 供引用区间重叠校验使用（validate_citations_against_entities）：
/// 键用 norm_sep 归一化的相对路径（与引用提取的正斜杠形态统一，Windows
/// 下不归一化会恒不命中），值 = 该文件全部实体的 (line_start, line_end)。
///
/// 必须用**全仓库** insights 而非变更文件子集——wiki 页面可能引用模块外
/// 文件（跨模块引用是正常行为），只传变更集会导致模块外引用全部误判
/// 为"无实体文件"而放行（区间校验失效）。
fn build_entity_ranges(insights: &[FileInsight]) -> crate::output::citation::EntityRanges {
    insights
        .iter()
        .map(|insight| {
            let key = crate::incremental::norm_sep(&insight.path.to_string_lossy());
            let ranges: Vec<(usize, usize)> = insight
                .entities
                .iter()
                .map(|e| (e.line_start, e.line_end))
                .collect();
            (key, ranges)
        })
        .collect()
}

/// 特征追溯回填（演进计划 T3.3）
///
/// 模块涉及的实体级特征 = 模块 chunk 实体名与特征实体名集合的交集。
/// 特征名列表写入卡片（render_knowledge_card 渲染"特征追溯"节），
/// 提供"功能 → 实现它的模块"的可追溯视图（RepoSummary 的 traceability）。
/// 特征实体名经 graph 反查 NodeId 得到；不经过 LLM，杜绝幻觉。
fn backfill_features(
    cards: &mut [Option<KnowledgeCard>],
    chunks: &[Chunk],
    graph: &KnowledgeGraph,
) {
    if graph.features.is_empty() || cards.is_empty() {
        return;
    }
    // 预构建 特征名 → 实体名集合（避免每张卡片重复遍历图）
    let feature_entities: Vec<(String, std::collections::HashSet<String>)> = graph
        .features
        .iter()
        .map(|f| {
            let names: std::collections::HashSet<String> = f
                .node_ids
                .iter()
                .filter_map(|nid| graph.graph.node_weight(*nid).map(|n| n.name.clone()))
                .collect();
            (f.name.clone(), names)
        })
        .collect();
    for (card_opt, chunk) in cards.iter_mut().zip(chunks) {
        // P1-1：失败/空 chunk 的卡片位是 None——特征回填跳过，不产生错位写入
        let Some(card) = card_opt else { continue };
        let entity_names: std::collections::HashSet<&str> =
            chunk.entities.iter().map(|e| e.name.as_str()).collect();
        let mut matched: Vec<String> = feature_entities
            .iter()
            .filter(|(_, names)| names.iter().any(|n| entity_names.contains(n.as_str())))
            .map(|(name, _)| name.clone())
            .collect();
        matched.sort();
        card.features = matched;
    }
}

// 实体摘要生成已删除（v31）：原 generate_entity_summaries 对每实体一次
// LLM 调用（全量 1500 实体=1500 次调用），但 Entity.summary 字段零消费者
// （全仓库仅自身写入/过滤读取）——纯 token 浪费。未来如需实体级语义索引，
// 应在生成时预索引重建，而非逐个惰性调用。

/// 按语言并行生成 Wiki 页面（演进计划 T3.1 并行化）
///
/// 卡片摘要按 chunk 索引一一对应；并发受 max_concurrent 信号量控制，
/// join_all 保序收集——与串行版的产出顺序一致，页面集合不变。
/// 失败页面跳过并告警（不中断整体生成）。
// 例外说明（复杂度红线 5 参数规则）：9 个参数均为相互独立的上下文项
// （生成器/输入/配置/并发/图/输出/进度回调），引入包装结构体反而
// 降低调用点可读性；进度回调为显式注入契约（v46），不并入上下文结构。
#[allow(clippy::too_many_arguments)]
async fn generate_wiki_pages<P: LlmProvider>(
    wiki_gen: &WikiGenerator<'_, P>,
    chunks: &[Chunk],
    cards: &[Option<KnowledgeCard>],
    config: &WikiConfig,
    max_concurrent: usize,
    root: &crate::project::ProjectRoot,
    graph: &KnowledgeGraph,
    entity_ranges: &crate::output::citation::EntityRanges,
    on_progress: &dyn Fn(crate::ProgressEvent),
) -> Vec<WikiDocument> {
    // v51：多语言支持已删除，恒按主语言单值生成（原按语言循环结构移除，
    // 行为不变——旧 languages 恒为单元素 [config.wiki.language]）
    let language = crate::output::primary_language(config);
    // 模块名 → 卡片表（wiki 阶段卡片已就绪：依赖方/调用方摘要可查）
    let cards_map: HashMap<String, &KnowledgeCard> = cards
        .iter()
        .flatten()
        .map(|c| (c.module_name.clone(), c))
        .collect();
    let summary_of = |m: &str| cards_map.get(m).map(|c| c.summary.clone());
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let mut handles = Vec::with_capacity(chunks.len());
    // 记录每个任务的模块名（失败时写入 wiki_gen 的失败列表，T3.2）
    let mut task_modules = Vec::with_capacity(chunks.len());
    // v46：LLM 逐页进度——并发任务完成计数（fetch_add 线程安全；单线程
    // runtime 轮询下依然成立——join_all 在同一任务内并发轮询各 future）
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = chunks.len() as u32;
    let mut lang_cfg = config.clone();
    lang_cfg.wiki.language = language.clone();
    for (i, chunk) in chunks.iter().enumerate() {
        // P1-1：卡片位 None（空 chunk/失败）→ 摘要为空串，页面照常生成（None 占位保证索引一一对应，不再前移错位）
        let card_summary = cards
            .get(i)
            .and_then(|c| c.as_ref())
            .map(|c| c.summary.clone())
            .unwrap_or_default();
        // 项目级上下文注入：依赖模块摘要（依赖方卡片摘要）+ 调用方上下文
        // （真实调用图推导）。在循环内预计算为 owned 数据再移入 async 任务
        // ——闭包 summary_of 借用 cards_map，不能跨 await 借用。
        let dep_contexts = context::build_dependency_contexts(chunk, &cards_map, &summary_of);
        let caller_contexts = context::build_caller_contexts(chunk, graph, &summary_of);
        let semaphore = semaphore.clone();
        let lang_cfg = lang_cfg.clone();
        let done = done.clone();
        task_modules.push(chunk.module_path.join("::"));
        handles.push(async move {
            let _permit = semaphore
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("信号量已关闭"))?;
            let result = wiki_gen
                .generate_wiki_page(
                    chunk,
                    &card_summary,
                    &lang_cfg,
                    root,
                    Some(entity_ranges),
                    &dep_contexts,
                    &caller_contexts,
                )
                .await;
            // 成败均计数并回报进度（失败项也算「已处理」——总数是任务数）
            // v46：wiki 项级区间 90..95（系数 5）——上限与 output 阶段点
            //（95%）相接，整条事件流百分比保持单调（90→90+…→95→98→100）
            let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            on_progress(crate::ProgressEvent {
                stage: "wiki",
                percent: (90 + (d as u64 * 5) / total.max(1) as u64) as u8,
                current: Some(d as u32),
                total: Some(total),
            });
            result
        });
    }

    let results = futures::future::join_all(handles).await;
    task_modules
        .into_iter()
        .zip(results)
        .filter_map(|(module, r)| match r {
            Ok(doc) => Some(doc),
            Err(e) => {
                // 失败隔离：记录失败的模块名（T3.2），不中断其他模块生成
                tracing::warn!("跳过 Wiki 页面生成 {}: {}", module, e);
                wiki_gen.record_failure(module);
                None
            }
        })
        .collect()
}

/// 全局文档受影响标记（P1-2 全局文档增量：受影响判断）
///
/// 增量模式按信号决定是否重生成全局文档，未受影响时从导出快照回填
/// 旧文档（零 LLM 成本）。全量模式恒为全受影响。
#[derive(Debug, Clone, Default)]
pub struct GlobalDocAffected {
    /// 架构概览/项目概览：接口级实体变化（新增/删除/签名）才受影响
    pub architecture: bool,
    /// 数据库 Schema 文档：本次变更含 .sql 文件才受影响
    pub schema: bool,
}

impl GlobalDocAffected {
    /// 全受影响（全量生成路径）
    pub fn all() -> Self {
        Self {
            architecture: true,
            schema: true,
        }
    }
}

/// 生成与具体模块无关的全局文档（架构概览 + 项目概览 + 数据库 Schema），追加到 `documents`
///
/// 全量与增量两条生成路径共用，避免复制相同的调用逻辑（DRY）。
/// 这三类文档反映全仓库状态：架构概览与项目概览基于完整 KnowledgeGraph 的模块列表，
/// Schema 文档基于全量 .sql 文件，与"本次变更了哪些模块"无关，
/// 因此增量路径也必须重新生成，否则增量输出会比全量输出缺少这三类页面。
/// 全局文档生成（架构概览 + 项目概览 + 数据库 Schema）
///
/// 参数为生成上下文的完整输入集（7 个）：wiki_gen 与 provider 是两条独立
/// LLM 通道（页面 vs 全局文档）、graph/config/root/cards 是生成所需的
/// 图结构、配置、项目根与卡片摘要、documents 是输出累加器。
/// 引入上下文结构体需新增类型仅服务本函数两处调用，YAGNI——保留平铺
/// 参数并在此说明，属明确的例外。
#[allow(clippy::too_many_arguments)]
async fn generate_global_documents(
    wiki_gen: &WikiGenerator<'_, Provider>,
    provider: &Provider,
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    cards: &[Option<KnowledgeCard>],
    documents: &mut Vec<WikiDocument>,
    affected: &GlobalDocAffected,
    has_deleted_files: bool,
    plan: Option<&crate::config::plan::ResolvedPlan>,
) -> Result<()> {
    // 文档类型决策：DocumentKind 是纯枚举（无 architecture 等可复用字段），
    // 且 output::wiki_page_path 按 kind 特判文件名（架构概览→architecture.md，
    // 项目概览→overview.md），因此新增 ProjectOverview 变体而非复用
    // ArchitectureOverview——复用会把概览写进 architecture.md，路径语义错位。
    if affected.architecture {
        // 架构概览与项目概览：没有卡片（本次没有模块被生成）时跳过，避免对空仓库发无意义的 LLM 调用
        // ——但纯删除场景例外（has_deleted_files）：删除属接口级变化，即使本次没有
        // 模块被重生成（孤立文件全删），架构/概览也必须重生成，否则回填旧版继续
        // 列出已删模块（v21 验证轮修复）。
        if cards.iter().any(|c| c.is_some()) || has_deleted_files {
            // generate_architecture / generate_overview 需要 GenerationOutput 快照（内部只用 cards 构建引用列表）
            let output_snapshot = GenerationOutput {
                cards: cards.iter().flatten().cloned().collect(),
                documents: documents.clone(),
                generation_stats: GenerationStats::default(),
                timings: crate::GenerationTimings::default(),
            };
            match wiki_gen
                .generate_architecture(&output_snapshot, graph, config, root)
                .await
            {
                Ok(arch) => documents.push(arch),
                // U06/D12：provider 瞬时失败不再丢页——降级为确定性骨架
                //（模块/依赖清单，零 LLM），下次成功生成时补齐摘要
                Err(e) => {
                    // A7.7 audit-llm P0：LLM 失败不再降级为确定性骨架（产物级
                    // 确定性降级违反「禁兜底」）。fail-fast 缺页——本页不产出，
                    // 记入 failed_modules（随生成摘要/状态可见 + 增量补偿重试）
                    // 并显式告警；架构概览缺失不中断整批（与模块页失败隔离一致）。
                    tracing::warn!("架构概览生成失败，架构概览页缺失（fail-fast）: {e}");
                    wiki_gen.record_failure("架构概览".into());
                }
            }
            match wiki_gen
                .generate_overview(&output_snapshot, graph, config, root)
                .await
            {
                Ok(overview) => documents.push(overview),
                Err(e) => {
                    // A7.7 audit-llm P0：同架构概览——fail-fast 缺页，不再产出
                    // 确定性骨架；记入 failed_modules 供摘要/状态/增量补偿可见。
                    tracing::warn!("项目概览生成失败，项目概览页缺失（fail-fast）: {e}");
                    wiki_gen.record_failure("项目概览".into());
                }
            }
        }
    } else if !backfill_global_docs(
        config,
        documents,
        &[
            crate::model::DocumentKind::ArchitectureOverview,
            crate::model::DocumentKind::ProjectOverview,
        ],
    ) {
        // 快照不可用（首次增量/快照损坏）→ 回退生成，保证页面存在性
        tracing::info!("全局文档快照回填不可用，回退重新生成");
        let output_snapshot = GenerationOutput {
            cards: cards.iter().flatten().cloned().collect(),
            documents: documents.clone(),
            generation_stats: GenerationStats::default(),
            timings: crate::GenerationTimings::default(),
        };
        match wiki_gen
            .generate_architecture(&output_snapshot, graph, config, root)
            .await
        {
            Ok(arch) => documents.push(arch),
            Err(e) => {
                // A7.7 audit-llm P0：同 affected 路径——快照回退重生成失败同样
                // fail-fast 缺页（记入 failed_modules + 显式告警），不再降级为
                // 确定性骨架（产物级确定性降级违反「禁兜底」）。
                tracing::warn!("架构概览生成失败，架构概览页缺失（fail-fast）: {e}");
                wiki_gen.record_failure("架构概览".into());
            }
        }
        match wiki_gen
            .generate_overview(&output_snapshot, graph, config, root)
            .await
        {
            Ok(overview) => documents.push(overview),
            Err(e) => {
                tracing::warn!("项目概览生成失败，项目概览页缺失（fail-fast）: {e}");
                wiki_gen.record_failure("项目概览".into());
            }
        }
    }

    // 数据库 Schema 文档：无 .sql 文件时内部直接返回空列表，不调用 LLM
    if affected.schema {
        match schema::generate_schema_documents_at(root, provider, config).await {
            Ok(mut schema_docs) => documents.append(&mut schema_docs),
            Err(e) => tracing::warn!("数据库 Schema 文档生成跳过: {}", e),
        }
    } else if !backfill_global_docs(
        config,
        documents,
        &[crate::model::DocumentKind::DatabaseSchema],
    ) {
        tracing::info!("Schema 快照回填不可用，回退重新生成");
        match schema::generate_schema_documents_at(root, provider, config).await {
            Ok(mut schema_docs) => documents.append(&mut schema_docs),
            Err(e) => tracing::warn!("数据库 Schema 文档生成跳过: {}", e),
        }
    }

    // v0.9 W1：plan 自定义文档（repowiki.documents）——与自动模块页并存，
    // LLM 按 goal/hints 生成 title 页，parent 决定 _toc 挂载。增量路径每次
    // 都重新生成（自定义页内容无模块指纹可判变更，增量语义=重生成最坏成本
    // 一次 LLM 调用/页，可接受；不做快照回填，避免「改了 plan 却回填旧页」）。
    if let Some(plan) = plan {
        for doc in &plan.documents {
            match wiki_gen.generate_custom_document(doc, config, root).await {
                Ok(custom) => documents.push(custom),
                Err(e) => {
                    // 与架构/概览同语义：fail-fast 缺页（记入 failed_modules +
                    // 显式告警），不产出无内容的占位页。
                    tracing::warn!("自定义文档「{}」生成失败，该页缺失: {}", doc.title, e);
                    wiki_gen.record_failure(doc.title.clone());
                }
            }
        }
    }

    Ok(())
}

/// 从导出快照回填指定类型的全局文档到 `documents`（P1-2 全局文档增量）
///
/// 快照是 render_all 每次写盘后的产物快照（.state/export_snapshot.json），
/// 内含完整 WikiDocument 对象。未受影响的全局文档从快照回填：
/// 渲染幂等（内容与上次一致 → 指纹一致 → 不误判人工修改）、不触发 LLM、
/// 且路径仍在 rendered_paths 中（不被陈旧清理误删）。
/// 返回是否至少回填一个（快照缺失/损坏/无该类文档 → false，调用方回退生成）。
///
/// 语言一致性：快照文档语言是上次生成时的配置语言，若当前配置
/// `wiki.language` 已切换（如 zh→en），回填的旧语言文档会写进旧语言
/// 目录，新语言目录缺失该页——视为受影响（不匹配即不回填），
/// 由调用方回退到新语言的 LLM 生成。
pub(crate) fn backfill_global_docs(
    config: &WikiConfig,
    documents: &mut Vec<WikiDocument>,
    kinds: &[crate::model::DocumentKind],
) -> bool {
    let snapshot_path = crate::output::export_snapshot_path(config.output_dir());
    let Ok(content) = std::fs::read_to_string(&snapshot_path) else {
        return false;
    };
    let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content) else {
        tracing::warn!(
            "导出快照解析失败（将回退重新生成全局文档）: {}",
            snapshot_path.display()
        );
        return false;
    };
    let mut filled = false;
    for doc in snapshot.documents {
        if kinds.contains(&doc.kind)
            // 语言一致性：快照语言 ≠ 当前主语言 → 语言配置已切换，
            // 旧语言内容不能回填（写盘目录错位），回退生成
            && doc.language == config.wiki.language
            // 去重锚定 title（写盘路径由 title 派生）而非 kind：Schema 文档
            // 按 .sql 文件每份（title 含路径），按 kind 去重会把多份同名
            // kind 的其余页丢弃 → cleanup 差集误删磁盘上的其余 schema 页
            && !documents.iter().any(|d| d.title == doc.title && d.language == doc.language)
        {
            documents.push(doc);
            filled = true;
        }
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentKind;

    /// 构造指定标题的 WikiDocument（测试辅助，其余字段留空）
    fn make_document(title: &str) -> WikiDocument {
        WikiDocument {
            title: title.into(),
            kind: DocumentKind::WikiPage,
            content: String::new(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            parent: String::new(),
            last_updated: String::new(),
            based_on_commit: None,
            fingerprint: None,
        }
    }

    /// P1-2：导出快照回填——未受影响的全局文档从快照恢复，且不与
    /// 本次已生成文档重复（同一类型只保留一个）
    #[test]
    fn test_backfill_global_docs_from_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_backfill_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".state")).unwrap();

        let arch = WikiDocument {
            title: "架构概览".into(),
            kind: DocumentKind::ArchitectureOverview,
            content: "架构内容".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        };
        let overview = WikiDocument {
            title: "项目概览".into(),
            kind: DocumentKind::ProjectOverview,
            content: "概览内容".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        };
        let snapshot = crate::output::ExportSnapshot {
            version: 1,
            documents: vec![arch.clone(), overview.clone()],
            cards: vec![],
            modules: vec![],
        };
        crate::fs::write_file_atomic(
            &dir.join(".state").join("export_snapshot.json"),
            &serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some(dir.clone()),
            ..Default::default()
        };

        // 本次已生成 overview（模拟模块页变化触发概览重生成）→ 只回填架构
        let mut documents = vec![overview.clone()];
        let filled = backfill_global_docs(
            &config,
            &mut documents,
            &[
                DocumentKind::ArchitectureOverview,
                DocumentKind::ProjectOverview,
            ],
        );
        assert!(filled, "快照存在时应回填");
        assert_eq!(documents.len(), 2, "回填架构（概览已存在不重复）");
        assert_eq!(documents[1].kind, DocumentKind::ArchitectureOverview);
        assert_eq!(documents[1].content, "架构内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-2：快照缺失 → 回填失败（调用方据此回退生成）
    #[test]
    fn test_backfill_global_docs_missing_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_backfill_miss_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = WikiConfig {
            output_dir: Some(dir.clone()),
            ..Default::default()
        };

        let mut documents = Vec::new();
        let filled = backfill_global_docs(
            &config,
            &mut documents,
            &[DocumentKind::ArchitectureOverview],
        );
        assert!(!filled, "快照缺失时回填失败（回退生成）");
        assert!(documents.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言切换（zh→en）：快照文档语言与当前配置不一致 → 不回填，
    /// 调用方回退到新语言的 LLM 生成（旧语言内容写盘目录错位会丢页）
    #[test]
    fn test_backfill_global_docs_skips_on_language_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_backfill_lang_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut arch = make_document("架构概览");
        arch.kind = DocumentKind::ArchitectureOverview;
        arch.language = "zh".into(); // 快照为旧配置语言
        let snapshot = crate::output::ExportSnapshot {
            version: 1,
            documents: vec![arch],
            cards: vec![],
            modules: vec![],
        };
        crate::fs::write_file_atomic(
            &dir.join(".state").join("export_snapshot.json"),
            &serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some(dir.clone()),
            wiki: crate::config::schema::WikiSection {
                language: "en".into(),
            },
            ..Default::default()
        };

        let mut documents = Vec::new();
        let filled = backfill_global_docs(
            &config,
            &mut documents,
            &[DocumentKind::ArchitectureOverview],
        );
        assert!(!filled, "语言不匹配时不得回填（回退生成新语言内容）");
        assert!(documents.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-2：受影响判断——接口级实体变化 → 架构受影响；纯 .sql 变更 → 仅 schema 受影响
    #[test]
    fn test_global_affected_signal() {
        use crate::incremental::change::{EntityChange, EntityChangeKind};

        let mut changes = Vec::new();
        changes.push(EntityChange {
            file: std::path::PathBuf::from("src/a.rs"),
            entity_name: "foo".into(),
            kind: EntityChangeKind::BodyChanged,
            old_range: None,
            new_range: None,
        });
        let affected = GlobalDocAffected {
            architecture: crate::incremental::change::EntityChangeSet {
                changes: changes.clone(),
            }
            .has_interface_change(),
            schema: false,
        };
        assert!(!affected.architecture, "纯实现级变化不应触发架构重生成");

        changes.push(EntityChange {
            file: std::path::PathBuf::from("src/a.rs"),
            entity_name: "bar".into(),
            kind: EntityChangeKind::Added,
            old_range: None,
            new_range: None,
        });
        let affected2 = GlobalDocAffected {
            architecture: crate::incremental::change::EntityChangeSet { changes }
                .has_interface_change(),
            schema: false,
        };
        assert!(affected2.architecture, "接口级变化应触发架构重生成");
    }

    /// P1 回归：Schema 文档按 .sql 文件每份（title 含路径），回填去重必须
    /// 锚定 title+language 而非 kind——按 kind 去重会把多份 schema 页丢弃，
    /// cleanup 差集随后误删磁盘上的其余 schema 页。
    #[test]
    fn test_backfill_global_docs_dedup_by_title_not_kind() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_backfill_schema_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let schema_a = WikiDocument {
            title: "Database Schema: db/a.sql".into(),
            kind: DocumentKind::DatabaseSchema,
            content: "A 表结构".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        };
        let schema_b = WikiDocument {
            title: "Database Schema: db/b.sql".into(),
            kind: DocumentKind::DatabaseSchema,
            content: "B 表结构".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        };
        let snapshot = crate::output::ExportSnapshot {
            version: 1,
            documents: vec![schema_a.clone(), schema_b.clone()],
            cards: vec![],
            modules: vec![],
        };
        crate::fs::write_file_atomic(
            &dir.join(".state").join("export_snapshot.json"),
            &serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some(dir.clone()),
            ..Default::default()
        };

        let mut documents = Vec::new();
        let filled = backfill_global_docs(&config, &mut documents, &[DocumentKind::DatabaseSchema]);
        assert!(filled, "快照存在时应回填");
        assert_eq!(
            documents.len(),
            2,
            "两份 schema 文档都应回填（按 title 去重，非按 kind）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-4 回归：entity-coverage 的签名实体名提取——`pub fn foo(x: i32)` 应提取
    /// foo（跳过 pub/fn 关键字），裸名 `Foo` 提取 Foo，与 api.md 权威口径一致。
    #[test]
    fn test_entity_name_from_signature() {
        use crate::output::lint::entity_name_from_signature;
        assert_eq!(
            entity_name_from_signature("pub fn foo(x: i32) -> u32").as_deref(),
            Some("foo")
        );
        assert_eq!(
            entity_name_from_signature("fn main()").as_deref(),
            Some("main")
        );
        assert_eq!(
            entity_name_from_signature("def bar()").as_deref(),
            Some("bar")
        );
        assert_eq!(
            entity_name_from_signature("func Baz()").as_deref(),
            Some("Baz")
        );
        assert_eq!(entity_name_from_signature("Foo").as_deref(), Some("Foo"));
        assert_eq!(
            entity_name_from_signature("pub struct Alpha").as_deref(),
            Some("Alpha")
        );
        assert_eq!(entity_name_from_signature(""), None);
        assert_eq!(entity_name_from_signature("   "), None);
    }
}

/// U06/D12：确定性骨架——模块名/实体数/依赖清单全部来自图，零 LLM；
/// references 指向模块页且按标题字典序（确定性输出）
#[test]
fn test_fallback_architecture_doc_skeleton() {
    use crate::model::{CodeNode, EdgeKind, NodeKind};
    use petgraph::stable_graph::StableDiGraph;

    let mut g = StableDiGraph::<CodeNode, crate::model::CodeEdge>::new();
    let a = g.add_node(CodeNode {
        id: crate::model::NodeId::new(0),
        kind: NodeKind::Function,
        name: "a_fn".into(),
        file_path: Some("src/a.rs".into()),
        line_range: None,
        doc_comment: None,
        signature: None,
        visibility: None,
        module_path: vec!["net".into()],
    });
    let b = g.add_node(CodeNode {
        id: crate::model::NodeId::new(1),
        kind: NodeKind::Function,
        name: "b_fn".into(),
        file_path: Some("src/b.rs".into()),
        line_range: None,
        doc_comment: None,
        signature: None,
        visibility: None,
        module_path: vec!["http".into()],
    });
    g.add_edge(
        a,
        b,
        crate::model::CodeEdge {
            id: petgraph::stable_graph::EdgeIndex::new(0),
            kind: EdgeKind::Calls,
            source: a,
            target: b,
            weight: 1.0,
            location: None,
        },
    );
    let graph = crate::model::KnowledgeGraph {
        graph: g,
        modules: vec![
            crate::model::ModuleCluster {
                name: "net".into(),
                node_ids: vec![a],
                cohesion: 1.0,
                coupling: 0.0,
                description: None,
            },
            crate::model::ModuleCluster {
                name: "http".into(),
                node_ids: vec![b],
                cohesion: 1.0,
                coupling: 0.0,
                description: None,
            },
        ],
        features: Vec::new(),
    };

    let config = WikiConfig::default();
    let doc = crate::generate::wiki::fallback_architecture_doc(
        &graph,
        &config,
        crate::model::DocumentKind::ArchitectureOverview,
        "架构概览",
    );
    assert!(
        doc.content.contains("架构概览"),
        "应含标题: {}",
        doc.content
    );
    assert!(doc.content.contains("net`（1 个实体）"), "应含模块与实体数");
    assert!(
        doc.content.contains("http`（1 个实体）"),
        "应含模块与实体数"
    );
    assert!(doc.content.contains("依赖 http"), "net 应列出依赖 http");
    assert_eq!(doc.kind, crate::model::DocumentKind::ArchitectureOverview);
    // references 覆盖全部模块且按标题字典序
    let titles: Vec<&str> = doc
        .references
        .iter()
        .map(|r| r.target_title.as_str())
        .collect();
    assert_eq!(
        titles,
        vec!["http", "net"],
        "references 应按标题字典序: {titles:?}"
    );
    assert!(
        doc.references
            .iter()
            .all(|r| r.target_path.starts_with("wiki/zh/")),
        "references 应指向主语言模块页"
    );
}
