pub mod card;
pub mod chunk;
pub mod embed;
pub mod llm;
pub mod prompt;
pub mod schema;
pub mod wiki;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;

use crate::config::plan::ResolvedPlan;
use crate::config::schema::WikiConfig;
use crate::ingest::parser::FileInsight;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::card::CardGenerator;
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
}

/// 根据配置创建 LLM Provider
pub fn create_provider(config: &WikiConfig) -> Result<Provider> {
    match config.llm.provider {
        crate::config::schema::LlmProviderType::OpenAI => {
            Ok(Provider::OpenAi(OpenAiProvider::new(&config.llm)?))
        }
        crate::config::schema::LlmProviderType::Anthropic => {
            Ok(Provider::Anthropic(AnthropicProvider::new(&config.llm)?))
        }
        crate::config::schema::LlmProviderType::Custom => {
            Ok(Provider::OpenAi(OpenAiProvider::new(&config.llm)?))
        }
        crate::config::schema::LlmProviderType::Mock => {
            // 本地模拟：测试/CI/无 API Key 场景，返回固定文本
            Ok(Provider::Mock(crate::generate::llm::MockProvider::new()))
        }
    }
}

/// 生效的语言列表（主语言 + 扩展语言）
///
/// Knowledge Card 只按主语言生成一次，Wiki 页面按本列表逐语言独立生成。
pub fn collect_languages(config: &WikiConfig) -> Vec<String> {
    let mut languages = vec![config.wiki.language.clone()];
    languages.extend(config.wiki.expand_languages.iter().cloned());
    languages
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

    // 2.3 解析 wiki_plan.yaml 生效计划（禁用或文件缺失 → None；坏文件中断）
    let plan = crate::config::plan::resolve_plan(config)?;

    // 2.5 Level 0 实体摘要：为每个实体生成摘要（跳过已有摘要的实体）
    for chunk in &mut chunks {
        for entity in &mut chunk.entities {
            if entity.summary.is_none() {
                let prompt =
                    crate::generate::prompt::entity_summary_prompt(entity, &config.wiki.language, plan.as_ref());
                let messages = vec![Message::user(prompt)];
                match provider.complete(&messages).await {
                    Ok(summary) => entity.summary = Some(summary.trim().to_string()),
                    Err(e) => tracing::warn!("生成实体摘要失败: {}", e),
                }
            }
        }
    }

    // 3. 并行生成 Knowledge Card
    let card_gen = CardGenerator::new(
        &provider,
        config.clone(),
        config.llm.max_concurrent,
        config.wiki.language.clone(),
        plan.clone(),
    );
    let cards = card_gen
        .generate_all_cards(&chunks, extra_edits)
        .await?;
    tracing::info!("生成进度: 60% - 知识卡片生成完成，共 {} 个卡片", cards.len());

    // 4. 按语言独立生成 Wiki 页面（卡片仅主语言生成一次，各语言页面复用主语言卡片摘要）
    let languages = collect_languages(config);
    let wiki_gen = WikiGenerator::new(&provider, plan.clone());
    let mut documents = Vec::with_capacity(chunks.len() * languages.len());

    for lang in &languages {
        let mut lang_cfg = config.clone();
        lang_cfg.wiki.language = lang.clone();
        for (i, chunk) in chunks.iter().enumerate() {
            let card_summary = cards.get(i).map(|c| c.summary.as_str()).unwrap_or("");
            match wiki_gen
                .generate_wiki_page(chunk, card_summary, &lang_cfg)
                .await
            {
                Ok(doc) => documents.push(doc),
                Err(e) => tracing::warn!(
                    "跳过模块 {:?} 的 Wiki 页面生成 ({}): {}",
                    chunk.module_path,
                    lang,
                    e
                ),
            }
        }
    }
    tracing::info!("生成进度: 90% - Wiki 页面生成完成，共 {} 个页面", documents.len());

    // 5. 生成全局文档（架构概览 + 数据库 Schema，全量/增量共用同一辅助函数）
    generate_global_documents(&wiki_gen, &provider, graph, config, plan.as_ref(), &cards, &mut documents).await?;

    // 6. 按计划文档白名单过滤（严格只输出列出的页面）
    documents = filter_by_whitelist(documents, plan.as_ref());

    let elapsed = start.elapsed();
    let stats = GenerationStats {
        llm_calls: card_gen.llm_call_count() + wiki_gen.llm_call_count(),
        generation_time_ms: elapsed.as_millis() as u64,
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
/// 与 `run_generation` 类似，但仅处理 `changed_files` 中列出的文件，
/// 用于增量更新场景。未变更的文件使用已有缓存，不触发新的 LLM 调用。
/// extra_edits 语义同 run_generation（本次新检测的人工修改记录）。
pub async fn run_generation_filtered(
    graph: &KnowledgeGraph,
    insights: &[FileInsight],
    config: &WikiConfig,
    changed_files: &std::collections::HashSet<std::path::PathBuf>,
    extra_edits: &HashMap<String, Vec<String>>,
) -> Result<GenerationOutput> {
    let start = Instant::now();

    // 过滤出变更文件的 Insight（克隆为拥有数据）
    let changed_insights: Vec<FileInsight> = insights
        .iter()
        .filter(|f| changed_files.contains(&f.path))
        .cloned()
        .collect();

    if changed_insights.is_empty() {
        tracing::info!("增量生成: 无变更文件，跳过");
        return Ok(GenerationOutput {
            cards: vec![],
            documents: vec![],
            generation_stats: GenerationStats::default(),
        });
    }

    tracing::info!("增量生成: {} 个文件变更", changed_insights.len());

    // 1. AST 感知分块（仅变更文件）
    let chunks: Vec<_> = if graph.modules.is_empty() {
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

    // 2.5 解析 wiki_plan.yaml 生效计划（禁用或文件缺失 → None；坏文件中断）
    let plan = crate::config::plan::resolve_plan(config)?;

    // 3. 并行生成 Knowledge Card（仅变更块）
    let card_gen = CardGenerator::new(
        &provider,
        config.clone(),
        config.llm.max_concurrent,
        config.wiki.language.clone(),
        plan.clone(),
    );
    let cards = card_gen
        .generate_all_cards(&chunks, extra_edits)
        .await?;

    // 4. 按语言独立生成 Wiki 页面（仅变更块；卡片仅主语言生成一次，各语言页面复用主语言卡片摘要）
    let languages = collect_languages(config);
    let wiki_gen = WikiGenerator::new(&provider, plan.clone());
    let mut documents = Vec::with_capacity(chunks.len() * languages.len());

    for lang in &languages {
        let mut lang_cfg = config.clone();
        lang_cfg.wiki.language = lang.clone();
        for (i, chunk) in chunks.iter().enumerate() {
            let card_summary = cards.get(i).map(|c| c.summary.as_str()).unwrap_or("");
            match wiki_gen
                .generate_wiki_page(chunk, card_summary, &lang_cfg)
                .await
            {
                Ok(doc) => documents.push(doc),
                Err(e) => tracing::warn!(
                    "跳过变更模块 {:?} 的 Wiki 页面生成 ({}): {}",
                    chunk.module_path,
                    lang,
                    e
                ),
            }
        }
    }

    // 5. 生成全局文档（架构概览 + 数据库 Schema）
    // 全局文档与"变更了哪些模块"无关：架构概览基于完整 KnowledgeGraph 的模块列表，
    // Schema 文档基于全量 .sql 文件，都反映全仓库状态。
    // 因此即使增量只改了 1 个模块，这两类文档也必须重新生成，否则增量输出
    // 会比全量输出缺少页面，行为不一致。
    generate_global_documents(&wiki_gen, &provider, graph, config, plan.as_ref(), &cards, &mut documents).await?;

    // 6. 按计划文档白名单过滤（严格只输出列出的页面）
    documents = filter_by_whitelist(documents, plan.as_ref());

    let elapsed = start.elapsed();
    let stats = GenerationStats {
        llm_calls: card_gen.llm_call_count() + wiki_gen.llm_call_count(),
        generation_time_ms: elapsed.as_millis() as u64,
        ..Default::default()
    };

    Ok(GenerationOutput {
        cards,
        documents,
        generation_stats: stats,
    })
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

/// 生成与具体模块无关的全局文档（架构概览 + 项目概览 + 数据库 Schema），追加到 `documents`
///
/// 全量与增量两条生成路径共用，避免复制相同的调用逻辑（DRY）。
/// 这三类文档反映全仓库状态：架构概览与项目概览基于完整 KnowledgeGraph 的模块列表，
/// Schema 文档基于全量 .sql 文件，与"本次变更了哪些模块"无关，
/// 因此增量路径也必须重新生成，否则增量输出会比全量输出缺少这三类页面。
async fn generate_global_documents(
    wiki_gen: &WikiGenerator<'_, Provider>,
    provider: &Provider,
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    plan: Option<&ResolvedPlan>,
    cards: &[KnowledgeCard],
    documents: &mut Vec<WikiDocument>,
) -> Result<()> {
    // 文档类型决策：DocumentKind 是纯枚举（无 architecture 等可复用字段），
    // 且 output::wiki_page_path 按 kind 特判文件名（架构概览→architecture.md，
    // 项目概览→overview.md），因此新增 ProjectOverview 变体而非复用
    // ArchitectureOverview——复用会把概览写进 architecture.md，路径语义错位。
    // 架构概览与项目概览：没有卡片（本次没有模块被生成）时跳过，避免对空仓库发无意义的 LLM 调用
    if !cards.is_empty() {
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
            Err(e) => tracing::warn!("架构概览生成跳过: {}", e),
        }
        match wiki_gen
            .generate_overview(&output_snapshot, graph, config)
            .await
        {
            Ok(overview) => documents.push(overview),
            Err(e) => tracing::warn!("项目概览生成跳过: {}", e),
        }
    }

    // 数据库 Schema 文档：无 .sql 文件时内部直接返回空列表，不调用 LLM
    match schema::generate_schema_documents(provider, config, plan).await {
        Ok(mut schema_docs) => documents.append(&mut schema_docs),
        Err(e) => tracing::warn!("数据库 Schema 文档生成跳过: {}", e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::plan::PlanDocument;
    use crate::config::schema::WikiSection;
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

    #[test]
    fn test_collect_languages_default_single() {
        let config = WikiConfig::default();
        assert_eq!(collect_languages(&config), vec!["zh"]);
    }

    #[test]
    fn test_collect_languages_with_expand() {
        let config = WikiConfig {
            wiki: WikiSection {
                language: "zh".into(),
                expand_languages: vec!["en".into(), "ja".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(collect_languages(&config), vec!["zh", "en", "ja"]);
    }
}
