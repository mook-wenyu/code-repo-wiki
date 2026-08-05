pub mod card;
pub mod chunk;
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

use crate::config::plan::ResolvedPlan;
use crate::config::schema::WikiConfig;
use crate::ingest::parser::FileInsight;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::card::CardGenerator;
use self::chunk::Chunk;
use self::llm::{AnthropicProvider, LlmProvider, Message, OpenAiProvider, Provider};
use self::wiki::WikiGenerator;

/// 生成流水线的输出
pub struct GenerationOutput {
    pub cards: Vec<KnowledgeCard>,
    pub documents: Vec<WikiDocument>,
    pub generation_stats: GenerationStats,
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
        crate::config::schema::LlmProviderType::OpenAI => {
            Ok(Provider::OpenAi(OpenAiProvider::new(&config.llm, crate::generate::llm::OpenAiProtocol::Responses)?))
        }
        crate::config::schema::LlmProviderType::Anthropic => {
            Ok(Provider::Anthropic(AnthropicProvider::new(&config.llm)?))
        }
        // openai-compatible = chat/completions 协议（custom 并入，v17 t02）
        crate::config::schema::LlmProviderType::OpenAiCompatible => {
            Ok(Provider::OpenAi(OpenAiProvider::new(&config.llm, crate::generate::llm::OpenAiProtocol::Chat)?))
        }
        crate::config::schema::LlmProviderType::Mock => {
            // 本地模拟：测试/CI/无 API Key 场景，返回固定文本
            Ok(Provider::Mock(crate::generate::llm::MockProvider::new()))
        }
    }
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
pub async fn run_generation(
    graph: &KnowledgeGraph,
    insights: &[FileInsight],
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    extra_edits: &HashMap<String, Vec<String>>,
) -> Result<GenerationOutput> {
    let start = Instant::now();

    // 1. AST 感知分块
    let mut chunks = if graph.modules.is_empty() {
        tracing::warn!("未检测到模块聚类，回退到文件级分块");
        insights
            .iter()
            .map(chunk::chunk_by_file)
            .collect::<Vec<_>>()
    } else {
        chunk::chunk_by_module(insights, &graph.modules, graph)
    };
    tracing::info!("生成进度: 30% - 分块完成，共 {} 个块", chunks.len());

    // 2. 创建 LLM Provider
    let provider = create_provider(config)?;

    // 2.3 解析 wiki_plan.yaml 生效计划（禁用或文件缺失 → None；坏文件中断；
    // 路径相对项目根解析，不依赖进程 cwd）
    let plan = crate::config::plan::resolve_plan_at(root, config)?;

    // 2.5 Level 0 实体摘要（并行，演进计划 T3.1）：为每个实体生成摘要
    generate_entity_summaries(
        &provider,
        &mut chunks,
        &config.wiki.language,
        plan.as_ref(),
        config.llm.max_concurrent,
        |_, _| true,
    )
    .await;

    // 3. 并行生成 Knowledge Card
    let card_gen = CardGenerator::new(
        &provider,
        config.clone(),
        config.llm.max_concurrent,
        config.wiki.language.clone(),
        plan.clone(),
    );
    let mut cards = card_gen
        .generate_all_cards(&chunks, extra_edits)
        .await?;
    // 特征追溯回填（演进计划 T3.3）：模块实体与特征实体的交集 → 特征名
    backfill_features(&mut cards, &chunks, graph);
    tracing::info!("生成进度: 60% - 知识卡片生成完成，共 {} 个卡片", cards.len());

    // 4. 按语言独立生成 Wiki 页面（并行，演进计划 T3.1；卡片仅主语言生成一次，
    // 各语言页面复用主语言卡片摘要；语言列表在 generate_wiki_pages 内部计算）
    let wiki_gen = WikiGenerator::new(&provider, plan.clone(), config.llm.max_concurrent);
    let mut documents =
        generate_wiki_pages(&wiki_gen, &chunks, &cards, config, config.llm.max_concurrent, root, &build_entity_ranges(insights)).await;
    tracing::info!("生成进度: 90% - Wiki 页面生成完成，共 {} 个页面", documents.len());

    // 5. 生成全局文档（架构概览 + 数据库 Schema，全量/增量共用同一辅助函数）
    generate_global_documents(&wiki_gen, &provider, graph, config, root, plan.as_ref(), &cards, &mut documents, &GlobalDocAffected::all(), false).await?;

    // 6. 按计划文档白名单过滤（严格只输出列出的页面）
    documents = filter_by_whitelist(documents, plan.as_ref());

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
        cards,
        documents,
        generation_stats: stats,
    })
}

/// 增量更新的过滤生成流水线
///
/// 与 `run_generation` 类似，但仅处理 `inc`（增量分析结果）中列出的变更
/// 文件 + 语义传播判定的受影响模块，用于增量更新场景。未变更的文件
/// 使用已有缓存，不触发新的 LLM 调用。
/// extra_edits 语义同 run_generation（本次新检测的人工修改记录）。
pub async fn run_generation_filtered(
    graph: &KnowledgeGraph,
    insights: &[FileInsight],
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    inc: &crate::incremental::IncrementalResult,
    extra_edits: &HashMap<String, Vec<String>>,
) -> Result<GenerationOutput> {
    let start = Instant::now();
    let changed_files = &inc.changed_files;
    let entity_changes = &inc.entity_changes;
    let affected_modules = &inc.affected_modules;

    // 2.5 解析 wiki_plan.yaml 生效计划（禁用或文件缺失 → None；坏文件中断；
    // 路径相对项目根解析，不依赖进程 cwd）。提前到纯删除分支之前：
    // 该分支的产物回填同样需要白名单过滤（与正常路径一致）。
    let plan = crate::config::plan::resolve_plan_at(root, config)?;

    // 过滤出变更文件的 Insight（克隆为拥有数据）。
    // T2 传播闭环接线：除变更文件外，语义传播判定的受影响模块文件也
    // 并入生成范围——签名/删除等接口级变化会重生成依赖方模块的文档，
    // 实现级变化（body-only）传播结果只含本模块，行为不变。
    let affected_files = crate::incremental::impact::module_files(affected_modules, graph);
    let mut changed_insights: Vec<FileInsight> = insights
        .iter()
        .filter(|f| changed_files.contains(&f.path) || affected_files.contains(&f.path))
        .cloned()
        .collect();

    if changed_insights.is_empty() {
        // 纯删除场景（P1 数据丢失修复）：changed_files 非空（删除事件）
        // 但无现存文件命中影响集（孤立模块唯一文件被删）时，旧实现直接
        // 返回空输出 → render_all 不写任何产物 → cleanup_stale_outputs
        // 差集语义把**全部**旧产物清空（无关模块页也被删）。
        // 修复：从导出快照回填未删除模块的旧产物（零 LLM 成本）；
        // 快照缺失（异常）时回退全量生成，宁可多生成也不丢数据。
        if let Ok(content) = std::fs::read_to_string(crate::output::export_snapshot_path(Path::new(&config.output.dir)))
            && let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content)
        {
            // v21 验证轮（删除场景缺陷修复）：纯删除时快照不只是回填来源，
            // 还是"被删文件 → 模块归属"的唯一完整映射（解析缓存会主动裁剪
            // 被删条目、当前 graph 无被删文件节点，传播起点为空）。
            // 新增/修改文件全部消失于磁盘（deleted_files）时，逐卡片判定：
            // - 全删模块（related_files 全部不存在）：原逻辑，回填时剔除；
            // - 部分删除模块（related_files 含被删文件但仍有存活文件）：
            //   **页面回填旧内容会残留被删实体的描述**（原实现缺陷），
            //   改为把模块的存活文件并入变更集，落入正常生成路径由 LLM
            //   重生成（卡片 + 页面 + 全局文档联动刷新）。
            let deleted_files: std::collections::HashSet<&std::path::Path> =
                changed_files.iter().filter(|f| !root.path().join(f).exists()).map(|f| f.as_path()).collect();
            let surviving: Vec<&KnowledgeCard> = if deleted_files.is_empty() {
                Vec::new()
            } else {
                snapshot
                    .cards
                    .iter()
                    .filter(|c| {
                        !c.related_files.is_empty()
                            && c.related_files.iter().any(|f| deleted_files.contains(Path::new(f)))
                            && c.related_files.iter().any(|f| root.path().join(f).exists())
                    })
                    .collect()
            };
            if !surviving.is_empty() {
                // 部分删除模块的存活文件 → 变更集 → 正常增量路径重生成
                let surviving_files: std::collections::HashSet<&std::path::Path> = surviving
                    .iter()
                    .flat_map(|c| c.related_files.iter().map(|f| Path::new(f.as_str())))
                    .collect();
                changed_insights = insights
                    .iter()
                    .filter(|i| surviving_files.contains(i.path.as_path()))
                    .cloned()
                    .collect();
                tracing::info!(
                    "增量生成: 纯删除场景（{} 个变更文件），{} 个部分删除模块的存活文件并入变更集重生成（清除被删实体残留）",
                    changed_files.len(),
                    surviving.len()
                );
            } else {
                // 无部分删除模块（全部是孤立文件全删或变更不含删除）：
                // 沿用零 LLM 回填——剔除全删模块后原样回填旧产物。
                let deleted_modules: std::collections::HashSet<String> = snapshot
                    .cards
                    .iter()
                    .filter(|c| {
                        !c.related_files.is_empty()
                            && c.related_files.iter().all(|f| !root.path().join(f).exists())
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
                    "增量生成: 纯删除场景（{} 个变更文件），从快照回填 {} 文档 {} 卡片（跳过已删模块 {} 个）",
                    changed_files.len(),
                    documents.len(),
                    cards.len(),
                    deleted_modules.len()
                );
                // 与正常路径一致：回填产物同样过白名单过滤（严格只输出列出的页面）
                let documents = filter_by_whitelist(documents, plan.as_ref());
                return Ok(GenerationOutput {
                    cards,
                    documents,
                    generation_stats: GenerationStats::default(),
                });
            }
        } else {
            tracing::warn!("增量生成: 纯删除场景但导出快照缺失，回退全量生成防止产物误清");
            // 回退全量：所有现存文件视为变更，走下方正常生成路径
            changed_insights = insights.to_vec();
        }
    } else {
        tracing::info!("增量生成: {} 个文件变更", changed_insights.len());
    }

    // 1. AST 感知分块（仅变更文件）
    let mut chunks: Vec<_> = if graph.modules.is_empty() {
        changed_insights
            .iter()
            .map(chunk::chunk_by_file)
            .collect()
    } else {
        // 按模块重新组织变更文件，保持模块上下文
        chunk::chunk_by_module(&changed_insights, &graph.modules, graph)
    };
    tracing::info!("增量分块完成: {} 个块", chunks.len());

    // 2. 创建 LLM Provider
    let provider = create_provider(config)?;

    // 2.5 增量实体摘要（演进计划 T2.3 实体级过滤 + T3.1 并行化）：
    // 仅对**接口级变化文件**中的实体重新生成摘要（新增/删除/签名变更），
    // 纯实现级变化（函数体修改）与未变化实体保留旧摘要，不浪费 LLM 调用。
    // 变化集合为空（FileWatch 策略或无接口级变化）时跳过本步骤。
    if !entity_changes.changes.is_empty() {
        let interface_files: std::collections::HashSet<std::path::PathBuf> = entity_changes
            .changes
            .iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    crate::incremental::change::EntityChangeKind::Added
                        | crate::incremental::change::EntityChangeKind::Removed
                        | crate::incremental::change::EntityChangeKind::SignatureChanged
                )
            })
            .map(|c| c.file.clone())
            .collect();
        generate_entity_summaries(
            &provider,
            &mut chunks,
            &config.wiki.language,
            plan.as_ref(),
            config.llm.max_concurrent,
            |chunk, ei| {
                chunk
                    .entity_sources
                    .get(ei)
                    .map(|f| interface_files.contains(f))
                    .unwrap_or(false)
            },
        )
        .await;
    }

    // 3. 并行生成 Knowledge Card（仅变更块）
    let card_gen = CardGenerator::new(
        &provider,
        config.clone(),
        config.llm.max_concurrent,
        config.wiki.language.clone(),
        plan.clone(),
    );
    let mut cards = card_gen
        .generate_all_cards(&chunks, extra_edits)
        .await?;
    // 特征追溯回填（演进计划 T3.3）：模块实体与特征实体的交集 → 特征名
    backfill_features(&mut cards, &chunks, graph);

    // 4. 按语言独立生成 Wiki 页面（并行，演进计划 T3.1；仅变更块；卡片仅主语言生成一次，
    // 各语言页面复用主语言卡片摘要）
    let wiki_gen = WikiGenerator::new(&provider, plan.clone(), config.llm.max_concurrent);
    let mut documents =
        generate_wiki_pages(&wiki_gen, &chunks, &cards, config, config.llm.max_concurrent, root, &build_entity_ranges(insights)).await;

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
    generate_global_documents(&wiki_gen, &provider, graph, config, root, plan.as_ref(), &cards, &mut documents, &global_affected, inc.has_deleted_files).await?;

    // 6. 按计划文档白名单过滤（严格只输出列出的页面）
    documents = filter_by_whitelist(documents, plan.as_ref());

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
        cards,
        documents,
        generation_stats: stats,
    })
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

/// 按计划文档白名单过滤输出文档
///
/// 全量与增量两条生成路径共用（DRY），避免复制过滤实现。
/// 语义：白名单列出严格只输出的页面集合，未列出的文档丢弃；
/// 白名单为 None 时原样返回（"全部生成"）。过滤发生在生成之后、
/// 渲染之前，LLM 调用成本不受白名单影响（与 Qoder 语义一致）。
fn filter_by_whitelist(
    documents: Vec<WikiDocument>,
    plan: Option<&ResolvedPlan>,
) -> Vec<WikiDocument> {
    let Some(whitelist) = plan.and_then(|p| p.whitelist.as_ref()) else {
        return documents;
    };
    let allowed: std::collections::HashSet<&str> =
        whitelist.iter().map(|d| d.title.as_str()).collect();
    documents
        .into_iter()
        .filter(|d| allowed.contains(d.title.as_str()))
        .collect()
}

/// 特征追溯回填（演进计划 T3.3）
///
/// 模块涉及的实体级特征 = 模块 chunk 实体名与特征实体名集合的交集。
/// 特征名列表写入卡片（render_knowledge_card 渲染"特征追溯"节），
/// 提供"功能 → 实现它的模块"的可追溯视图（RepoSummary 的 traceability）。
/// 特征实体名经 graph 反查 NodeId 得到；不经过 LLM，杜绝幻觉。
fn backfill_features(cards: &mut [KnowledgeCard], chunks: &[Chunk], graph: &KnowledgeGraph) {
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
    for (card, chunk) in cards.iter_mut().zip(chunks) {
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

/// 并行生成实体摘要（Level 0，演进计划 T3.1 并行化）
///
/// 只对通过 `filter` 且 summary 为 None 的实体发起 LLM 调用；
/// 并发受 max_concurrent 信号量控制（与卡片/Schema 生成一致），
/// 失败仅告警不中断。结果按收集顺序写回 chunks（join_all 保序），
/// 与串行版的产物顺序一致。
///
/// `filter` 用于增量场景的实体级过滤（T2.3）：仅接口级变化文件的
/// 实体重新生成摘要；全量场景传恒真闭包。
async fn generate_entity_summaries(
    provider: &Provider,
    chunks: &mut [Chunk],
    language: &str,
    plan: Option<&ResolvedPlan>,
    max_concurrent: usize,
    filter: impl Fn(&Chunk, usize) -> bool,
) {
    // 收集任务（只读遍历收集，避免并发写 chunks 的借用冲突）
    let tasks: Vec<(usize, usize, String)> = chunks
        .iter()
        .enumerate()
        .flat_map(|(ci, chunk)| {
            chunk
                .entities
                .iter()
                .enumerate()
                .filter(|(ei, e)| e.summary.is_none() && filter(chunk, *ei))
                .map(move |(ei, entity)| {
                    (ci, ei, prompt::entity_summary_prompt(entity, language, plan))
                })
        })
        .collect();
    if tasks.is_empty() {
        return;
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let handles: Vec<_> = tasks
        .iter()
        .map(|(_, _, prompt)| {
            let semaphore = semaphore.clone();
            let prompt = prompt.clone();
            async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .map_err(|_| anyhow::anyhow!("信号量已关闭"))?;
                provider.complete(&[Message::user(prompt)]).await
            }
        })
        .collect();
    let results = futures::future::join_all(handles).await;

    // 按收集顺序写回（join_all 保序，产物与串行版一致）
    for ((ci, ei, _), result) in tasks.iter().zip(results) {
        match result {
            Ok(summary) => chunks[*ci].entities[*ei].summary = Some(summary.trim().to_string()),
            Err(e) => tracing::warn!("实体摘要生成失败: {}", e),
        }
    }
}

/// 按语言并行生成 Wiki 页面（演进计划 T3.1 并行化）
///
/// 卡片摘要按 chunk 索引一一对应；并发受 max_concurrent 信号量控制，
/// join_all 保序收集——与串行版的产出顺序一致，页面集合不变。
/// 失败页面跳过并告警（不中断整体生成）。
async fn generate_wiki_pages<P: LlmProvider>(
    wiki_gen: &WikiGenerator<'_, P>,
    chunks: &[Chunk],
    cards: &[KnowledgeCard],
    config: &WikiConfig,
    max_concurrent: usize,
    root: &crate::project::ProjectRoot,
    entity_ranges: &crate::output::citation::EntityRanges,
) -> Vec<WikiDocument> {
    let languages = crate::output::wiki_languages(config);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let mut handles = Vec::with_capacity(chunks.len() * languages.len());
    // 记录每个任务的模块名（失败时写入 wiki_gen 的失败列表，T3.2）
    let mut task_modules = Vec::with_capacity(chunks.len() * languages.len());
    for lang in &languages {
        let mut lang_cfg = config.clone();
        lang_cfg.wiki.language = lang.clone();
        for (i, chunk) in chunks.iter().enumerate() {
            let card_summary = cards.get(i).map(|c| c.summary.clone()).unwrap_or_default();
            let semaphore = semaphore.clone();
            let lang_cfg = lang_cfg.clone();
            task_modules.push(chunk.module_path.join("::"));
            handles.push(async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .map_err(|_| anyhow::anyhow!("信号量已关闭"))?;
                wiki_gen
                    .generate_wiki_page(chunk, &card_summary, &lang_cfg, root, Some(entity_ranges))
                    .await
            });
        }
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
        Self { architecture: true, schema: true }
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
/// 参数为生成上下文的完整输入集（8 个）：wiki_gen 与 provider 是两条独立
/// LLM 通道（页面 vs 全局文档）、graph/config/root/plan/cards 是生成所需的
/// 图结构、配置、项目根、计划与卡片摘要、documents 是输出累加器。
/// 引入上下文结构体需新增类型仅服务本函数两处调用，YAGNI——保留平铺
/// 参数并在此说明，属明确的例外。
#[allow(clippy::too_many_arguments)]
async fn generate_global_documents(
    wiki_gen: &WikiGenerator<'_, Provider>,
    provider: &Provider,
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    plan: Option<&ResolvedPlan>,
    cards: &[KnowledgeCard],
    documents: &mut Vec<WikiDocument>,
    affected: &GlobalDocAffected,
    has_deleted_files: bool,
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
        if !cards.is_empty() || has_deleted_files {
            // generate_architecture / generate_overview 需要 GenerationOutput 快照（内部只用 cards 构建引用列表）
            let output_snapshot = GenerationOutput {
                cards: cards.to_vec(),
                documents: documents.clone(),
                generation_stats: GenerationStats::default(),
            };
            match wiki_gen
                .generate_architecture(&output_snapshot, graph, config)
                .await
            {
                Ok(arch) => documents.push(arch),
                // U06/D12：provider 瞬时失败不再丢页——降级为确定性骨架
                //（模块/依赖清单，零 LLM），下次成功生成时补齐摘要
                Err(e) => {
                    tracing::warn!("架构概览生成失败，降级为确定性骨架: {e}");
                    documents.push(crate::generate::wiki::fallback_architecture_doc(
                        graph,
                        config,
                        crate::model::DocumentKind::ArchitectureOverview,
                        "架构概览",
                    ));
                }
            }
            match wiki_gen
                .generate_overview(&output_snapshot, graph, config)
                .await
            {
                Ok(overview) => documents.push(overview),
                Err(e) => {
                    tracing::warn!("项目概览生成失败，降级为确定性骨架: {e}");
                    documents.push(crate::generate::wiki::fallback_architecture_doc(
                        graph,
                        config,
                        crate::model::DocumentKind::ProjectOverview,
                        "项目概览",
                    ));
                }
            }
        }
    } else if !backfill_global_docs(config, documents, &[
        crate::model::DocumentKind::ArchitectureOverview,
        crate::model::DocumentKind::ProjectOverview,
    ]) {
        // 快照不可用（首次增量/快照损坏）→ 回退生成，保证页面存在性
        tracing::info!("全局文档快照回填不可用，回退重新生成");
        let output_snapshot = GenerationOutput {
            cards: cards.to_vec(),
            documents: documents.clone(),
            generation_stats: GenerationStats::default(),
        };
        match wiki_gen
            .generate_architecture(&output_snapshot, graph, config)
            .await
        {
            Ok(arch) => documents.push(arch),
            // U06/D12：同 affected 路径——失败降级为确定性骨架而非丢页
            Err(e) => {
                tracing::warn!("架构概览生成失败，降级为确定性骨架: {e}");
                documents.push(crate::generate::wiki::fallback_architecture_doc(
                    graph,
                    config,
                    crate::model::DocumentKind::ArchitectureOverview,
                    "架构概览",
                ));
            }
        }
        match wiki_gen
            .generate_overview(&output_snapshot, graph, config)
            .await
        {
            Ok(overview) => documents.push(overview),
            Err(e) => {
                tracing::warn!("项目概览生成失败，降级为确定性骨架: {e}");
                documents.push(crate::generate::wiki::fallback_architecture_doc(
                    graph,
                    config,
                    crate::model::DocumentKind::ProjectOverview,
                    "项目概览",
                ));
            }
        }
    }

    // 数据库 Schema 文档：无 .sql 文件时内部直接返回空列表，不调用 LLM
    if affected.schema {
        match schema::generate_schema_documents_at(root, provider, config, plan).await {
            Ok(mut schema_docs) => documents.append(&mut schema_docs),
            Err(e) => tracing::warn!("数据库 Schema 文档生成跳过: {}", e),
        }
    } else if !backfill_global_docs(config, documents, &[crate::model::DocumentKind::DatabaseSchema]) {
        tracing::info!("Schema 快照回填不可用，回退重新生成");
        match schema::generate_schema_documents_at(root, provider, config, plan).await {
            Ok(mut schema_docs) => documents.append(&mut schema_docs),
            Err(e) => tracing::warn!("数据库 Schema 文档生成跳过: {}", e),
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
    let snapshot_path = crate::output::export_snapshot_path(Path::new(&config.output.dir));
    let Ok(content) = std::fs::read_to_string(&snapshot_path) else {
        return false;
    };
    let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content) else {
        tracing::warn!("导出快照解析失败（将回退重新生成全局文档）: {}", snapshot_path.display());
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
    use crate::config::plan::PlanDocument;
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
            last_updated: String::new(),
            fingerprint: None,
        }
    }

    /// 构造只含白名单标题的 ResolvedPlan（测试辅助）
    fn make_whitelist_plan(titles: &[&str]) -> ResolvedPlan {
        ResolvedPlan {
            whitelist: Some(
                titles
                    .iter()
                    .map(|t| PlanDocument {
                        title: (*t).into(),
                        goal: String::new(),
                        parent: None,
                        include_patterns: vec![],
                        exclude_patterns: vec![],
                        hints: None,
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn test_whitelist_filters_documents() {
        // 3 个文档 + 白名单 2 个 → 输出仅保留白名单内的 2 个，顺序不变
        let documents = vec![
            make_document("模块A"),
            make_document("模块B"),
            make_document("模块C"),
        ];
        let plan = make_whitelist_plan(&["模块A", "模块C"]);
        let filtered = filter_by_whitelist(documents, Some(&plan));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].title, "模块A");
        assert_eq!(filtered[1].title, "模块C");
    }

    #[test]
    fn test_whitelist_none_keeps_all_documents() {
        // 白名单为 None（未配置或空白名单折叠）→ 全部文档保留
        let documents = vec![
            make_document("模块A"),
            make_document("模块B"),
            make_document("模块C"),
        ];
        let filtered = filter_by_whitelist(documents, None);
        assert_eq!(filtered.len(), 3);
    }

    /// P1-2：导出快照回填——未受影响的全局文档从快照恢复，且不与
    /// 本次已生成文档重复（同一类型只保留一个）
    #[test]
    fn test_backfill_global_docs_from_snapshot() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_backfill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".state")).unwrap();

        let arch = WikiDocument {
            title: "架构概览".into(),
            kind: DocumentKind::ArchitectureOverview,
            content: "架构内容".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
            fingerprint: None,
        };
        let overview = WikiDocument {
            title: "项目概览".into(),
            kind: DocumentKind::ProjectOverview,
            content: "概览内容".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
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

        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().to_string();

        // 本次已生成 overview（模拟模块页变化触发概览重生成）→ 只回填架构
        let mut documents = vec![overview.clone()];
        let filled = backfill_global_docs(
            &config,
            &mut documents,
            &[DocumentKind::ArchitectureOverview, DocumentKind::ProjectOverview],
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
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_backfill_miss_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().to_string();

        let mut documents = Vec::new();
        let filled = backfill_global_docs(&config, &mut documents, &[DocumentKind::ArchitectureOverview]);
        assert!(!filled, "快照缺失时回填失败（回退生成）");
        assert!(documents.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言切换（zh→en）：快照文档语言与当前配置不一致 → 不回填，
    /// 调用方回退到新语言的 LLM 生成（旧语言内容写盘目录错位会丢页）
    #[test]
    fn test_backfill_global_docs_skips_on_language_mismatch() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_backfill_lang_{}", std::process::id()));
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

        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().to_string();
        config.wiki.language = "en".into(); // 当前配置已切换为 en

        let mut documents = Vec::new();
        let filled = backfill_global_docs(&config, &mut documents, &[DocumentKind::ArchitectureOverview]);
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
            architecture: crate::incremental::change::EntityChangeSet { changes: changes.clone() }.has_interface_change(),
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
            architecture: crate::incremental::change::EntityChangeSet { changes }.has_interface_change(),
            schema: false,
        };
        assert!(affected2.architecture, "接口级变化应触发架构重生成");
    }

    /// P1 回归：Schema 文档按 .sql 文件每份（title 含路径），回填去重必须
    /// 锚定 title+language 而非 kind——按 kind 去重会把多份 schema 页丢弃，
    /// cleanup 差集随后误删磁盘上的其余 schema 页。
    #[test]
    fn test_backfill_global_docs_dedup_by_title_not_kind() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_backfill_schema_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let schema_a = WikiDocument {
            title: "Database Schema: db/a.sql".into(),
            kind: DocumentKind::DatabaseSchema,
            content: "A 表结构".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
            fingerprint: None,
        };
        let schema_b = WikiDocument {
            title: "Database Schema: db/b.sql".into(),
            kind: DocumentKind::DatabaseSchema,
            content: "B 表结构".into(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
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

        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().to_string();

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
        assert_eq!(entity_name_from_signature("pub fn foo(x: i32) -> u32").as_deref(), Some("foo"));
        assert_eq!(entity_name_from_signature("fn main()").as_deref(), Some("main"));
        assert_eq!(entity_name_from_signature("def bar()").as_deref(), Some("bar"));
        assert_eq!(entity_name_from_signature("func Baz()").as_deref(), Some("Baz"));
        assert_eq!(entity_name_from_signature("Foo").as_deref(), Some("Foo"));
        assert_eq!(entity_name_from_signature("pub struct Alpha").as_deref(), Some("Alpha"));
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
            signature: None, visibility: None,
            module_path: vec!["net".into()],
        });
        let b = g.add_node(CodeNode {
            id: crate::model::NodeId::new(1),
            kind: NodeKind::Function,
            name: "b_fn".into(),
            file_path: Some("src/b.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec!["http".into()],
        });
        g.add_edge(a, b, crate::model::CodeEdge {
            id: petgraph::stable_graph::EdgeIndex::new(0),
            kind: EdgeKind::Calls,
            source: a,
            target: b,
            weight: 1.0,
            location: None,
        });
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
        assert!(doc.content.contains("架构概览"), "应含标题: {}", doc.content);
        assert!(doc.content.contains("net`（1 个实体）"), "应含模块与实体数");
        assert!(doc.content.contains("http`（1 个实体）"), "应含模块与实体数");
        assert!(doc.content.contains("依赖 http"), "net 应列出依赖 http");
        assert_eq!(doc.kind, crate::model::DocumentKind::ArchitectureOverview);
        // references 覆盖全部模块且按标题字典序
        let titles: Vec<&str> = doc.references.iter().map(|r| r.target_title.as_str()).collect();
        assert_eq!(titles, vec!["http", "net"], "references 应按标题字典序: {titles:?}");
        assert!(doc.references.iter().all(|r| r.target_path.starts_with("wiki/zh/")), "references 应指向主语言模块页");
    }
