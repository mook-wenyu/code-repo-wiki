pub mod config;
pub mod model;
pub mod ingest;
pub mod analysis;
pub mod generate;
pub mod output;
pub mod incremental;
pub mod search;

use std::path::Path;

use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

use anyhow::bail;

/// 仓库分析结果，完整流水线的输出
pub struct AnalysisResult {
    pub graph: model::KnowledgeGraph,
    pub documents: Vec<model::WikiDocument>,
    pub cards: Vec<model::KnowledgeCard>,
    pub stats: AnalysisStats,
}

/// 分析统计信息
#[derive(Debug, Clone, Default)]
pub struct AnalysisStats {
    pub files_scanned: usize,
    pub files_parsed: usize,
    pub total_entities: usize,
    pub total_edges: usize,
    pub modules_detected: usize,
    pub generation_time_ms: u64,
}

fn get_global_runtime() -> &'static Arc<Runtime> {
    static RT: OnceLock<Arc<Runtime>> = OnceLock::new();
    RT.get_or_init(|| Arc::new(Runtime::new().expect("创建 tokio Runtime 失败")))
}

/// 加载配置，并用 CLI 传入的 output 路径覆盖配置文件中的 output.dir
///
/// output.dir 是相对路径（默认 .repo-wiki），覆盖后渲染、搜索索引、状态目录
/// 等所有下游引用自然指向新目录。
///
/// 同时消费 wiki_plan.yaml 的 scope_override：生效计划存在且提供 scope 时
/// 覆盖 config.scope（plan.enabled=false 或文件缺失时不影响）。
fn load_config_with_output(config_path: &Path, output: Option<&Path>) -> anyhow::Result<config::schema::WikiConfig> {
    let config = config::load_config(config_path)?;
    let mut config = if let Some(out) = output {
        let mut c = config;
        c.output.dir = out.to_string_lossy().into_owned();
        c
    } else {
        config
    };
    if let Some(plan) = crate::config::plan::resolve_plan(&config)?
        && let Some(scope) = plan.scope_override
    {
        config.scope = scope;
    }
    Ok(config)
}

/// 加载保护集：旧 state 的 protected_docs + 新检测出的人工修改；force 时清空
fn load_protection(
    config: &config::schema::WikiConfig,
    force: bool,
) -> (std::collections::HashSet<String>, Option<incremental::state::GenerationState>) {
    if force {
        return (std::collections::HashSet::new(), None);
    }
    let state_dir = Path::new(&config.output.dir).join(".state");
    let state = incremental::state::GenerationState::load(&state_dir).ok();
    let mut protected: std::collections::HashSet<String> = state
        .as_ref()
        .map(|s| s.protected_docs.iter().cloned().collect())
        .unwrap_or_default();
    if let Some(s) = &state {
        for p in s.detect_manually_modified() {
            protected.insert(p);
        }
    }
    (protected, state)
}

/// 保存生成状态：doc_fingerprints 只记录实际写盘的文档（跳过保护集），
/// protected_docs 合并本次保护集写回
fn save_generation_state(
    config: &config::schema::WikiConfig,
    insights: &[ingest::parser::FileInsight],
    documents: &[model::WikiDocument],
    protected: &std::collections::HashSet<String>,
    commit_hash: &str,
) {
    let output_dir = Path::new(&config.output.dir);
    let state_dir = output_dir.join(".state");
    if let Ok(mut state) = incremental::state::GenerationState::from_insights(insights, commit_hash) {
        let mut protected_docs: Vec<String> = protected.iter().cloned().collect();
        protected_docs.sort();
        state.protected_docs = protected_docs;
        if let Ok(fps) = incremental::state::GenerationState::record_doc_fingerprints(
            documents,
            output_dir,
            &output::wiki_languages(config),
        ) {
            state.doc_fingerprints = fps.into_iter().filter(|(p, _)| !protected.contains(p)).collect();
        }
        let _ = state.save(&state_dir);
    }
}

/// 流水线进度事件（供 CLI --progress-json 与插件进度展示使用）
#[derive(Debug, Clone, Copy)]
pub struct ProgressEvent {
    /// 阶段名：scanning/analyzing/chunking/cards/wiki/output/index/done
    pub stage: &'static str,
    /// 进度百分比（0-100）
    pub percent: u8,
}

/// 运行完整的分析流水线（配置文件路径）
///
/// `output` 非空时覆盖配置文件中的 output.dir（对应 CLI 的 --output 参数），
/// 后续渲染、搜索索引、状态目录全部使用覆盖后的值。
/// `force` 为 true 时清空人工修改保护集并覆盖所有文档（对应 CLI 的 --force）。
pub fn run_pipeline(config_path: &Path, output: Option<&Path>, force: bool) -> anyhow::Result<AnalysisResult> {
    run_pipeline_with_progress(config_path, output, force, &|_| {})
}

/// 运行完整的分析流水线，并在各阶段边界回调进度事件
///
/// 事件点：scanning 10 / analyzing 25 / chunking 30 / cards 60 / wiki 90 /
/// output 95 / index 98 / done 100，对应扫描、分析、生成、渲染、索引、保存阶段。
pub fn run_pipeline_with_progress(
    config_path: &Path,
    output: Option<&Path>,
    force: bool,
    on_progress: &dyn Fn(ProgressEvent),
) -> anyhow::Result<AnalysisResult> {
    let config = load_config_with_output(config_path, output)?;
    let _span = tracing::info_span!("pipeline", config = %config_path.display());
    let _enter = _span.enter();
    let start = std::time::Instant::now();

    // 保护集：旧 state 的 protected_docs + 检测出的人工修改；force 时清空
    let (protected, _old_state) = load_protection(&config, force);

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;
    on_progress(ProgressEvent { stage: "scanning", percent: 10 });
    if file_insights.is_empty() {
        bail!("未找到任何源文件");
    }
    let mut stats = AnalysisStats {
        files_scanned: file_insights.len(),
        files_parsed: file_insights.iter().filter(|f| !f.entities.is_empty()).count(),
        ..Default::default()
    };

    // Phase 2: 分析
    let graph = analysis::build_graph(&file_insights)?;
    let modules = analysis::detect_modules(&graph)?;
    on_progress(ProgressEvent { stage: "analyzing", percent: 25 });
    stats.total_entities = graph.graph.node_count();
    stats.total_edges = graph.graph.edge_count();
    stats.modules_detected = modules.len();

    // Phase 3: 生成（需要 tokio 运行时）
    on_progress(ProgressEvent { stage: "chunking", percent: 30 });
    let rt = get_global_runtime();
    let gen_output = rt.block_on(generate::run_generation(&graph, &file_insights, &config))?;
    on_progress(ProgressEvent { stage: "cards", percent: 60 });

    // Phase 4: 输出
    on_progress(ProgressEvent { stage: "wiki", percent: 90 });
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config, &protected)?;
    on_progress(ProgressEvent { stage: "output", percent: 95 });

    // Phase 5: 构建搜索索引
    if config.search.enabled && let Err(e) = build_search_index(&graph, &file_insights, &config) {
        tracing::warn!("搜索索引构建失败（不影响主流程）: {}", e);
    }
    on_progress(ProgressEvent { stage: "index", percent: 98 });

    // Phase 6: 保存增量状态（含文档指纹用于人工修改保护）
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash().unwrap_or_default();
        save_generation_state(&config, &file_insights, &gen_output.documents, &protected, &head_hash);
    }

    on_progress(ProgressEvent { stage: "done", percent: 100 });
    stats.generation_time_ms = start.elapsed().as_millis() as u64;
    tracing::info!("流水线完成: {} 个文件, {} 个实体, {} 条边, {} 个模块, 耗时 {}ms",
        stats.files_scanned, stats.total_entities, stats.total_edges,
        stats.modules_detected, stats.generation_time_ms);

    Ok(AnalysisResult {
        graph,
        documents: gen_output.documents,
        cards: gen_output.cards,
        stats,
    })
}

/// 执行知识卡片操作（CLI card 子命令与 Qoder /knowledge 对等）
pub fn run_card_command(config_path: &Path, action: &generate::card::CardAction) -> anyhow::Result<()> {
    let config = load_config_with_output(config_path, None)?;
    // 编辑类动作要求卡片已存在：先校验（错误信息优先于 LLM API Key 检查）
    match action {
        generate::card::CardAction::Generate { .. } => {}
        generate::card::CardAction::Modify { module, .. }
        | generate::card::CardAction::Supplement { module, .. }
        | generate::card::CardAction::Rewrite { module, .. } => {
            if generate::card::read_card(&config, module)?.is_none() {
                anyhow::bail!("模块 {module} 的卡片不存在，请先运行 `repo-wiki generate` 全量生成");
            }
        }
    }
    let provider = generate::create_provider(&config)?;
    let rt = get_global_runtime();
    match action {
        generate::card::CardAction::Generate { module } => {
            rt.block_on(generate::card::generate_module_card(&provider, &config, module))
        }
        generate::card::CardAction::Modify { module, instruction, references } => {
            rt.block_on(generate::card::edit_card(
                &provider, &config, module, instruction, references,
                generate::card::CardEditMode::Modify,
            ))
        }
        generate::card::CardAction::Supplement { module, instruction, references } => {
            rt.block_on(generate::card::edit_card(
                &provider, &config, module, instruction, references,
                generate::card::CardEditMode::Supplement,
            ))
        }
        generate::card::CardAction::Rewrite { module, instruction, references } => {
            rt.block_on(generate::card::edit_card(
                &provider, &config, module, instruction, references,
                generate::card::CardEditMode::Rewrite,
            ))
        }
    }
}

/// 运行增量更新流水线
///
/// `output` 非空时覆盖配置文件中的 output.dir（对应 CLI 的 --output 参数）。
/// `force` 为 true 时清空人工修改保护集（Update 命令当前固定传 false）。
pub fn run_incremental_pipeline(config_path: &Path, output: Option<&Path>, force: bool) -> anyhow::Result<AnalysisResult> {
    let config = load_config_with_output(config_path, output)?;
    let start = std::time::Instant::now();

    // 保护集必须在增量分析之前加载（增量分析内部会重写 state 文件）
    let (protected, _old_state) = load_protection(&config, force);

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;

    // Phase 2: 分析
    let graph = analysis::build_graph(&file_insights)?;
    let _modules = analysis::detect_modules(&graph)?;

    // 检查增量变更
    let inc_result = incremental::run_incremental_update(&file_insights, &graph, &config)?;

    // 回退全量时 changed_files 非空但 affected_modules 为空，仅凭 changed_files 判断是否跳过
    if inc_result.changed_files.is_empty() {
        tracing::info!("无变更，跳过生成");
        let stats = AnalysisStats {
            files_scanned: file_insights.len(),
            generation_time_ms: start.elapsed().as_millis() as u64,
            ..Default::default()
        };
        return Ok(AnalysisResult {
            graph,
            documents: Vec::new(),
            cards: Vec::new(),
            stats,
        });
    }

    // Phase 3: 增量生成
    let rt = get_global_runtime();
    let changed_set: std::collections::HashSet<std::path::PathBuf> = inc_result.changed_files.iter().cloned().collect();
    let gen_output = rt.block_on(
        generate::run_generation_filtered(&graph, &file_insights, &config, &changed_set)
    )?;

    // Phase 4: 全量输出（保持索引一致）
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config, &protected)?;

    // Phase 4b: 清理已删除文件对应的输出文档
    let output_dir = Path::new(&config.output.dir).join("wiki").join("modules");
    for f in &inc_result.changed_files {
        if !f.exists() {
            let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let wiki_path = output_dir.join(format!("{}.md", stem));
            let card_path = Path::new(&config.output.dir).join("cards").join(format!("{}.md", stem));
            let _ = std::fs::remove_file(&wiki_path);
            let _ = std::fs::remove_file(&card_path);
        }
    }

    // Phase 5: 增量更新搜索索引
    if config.search.enabled && let Err(e) = update_search_index_incremental(&graph, &file_insights, &config, &changed_set) {
        tracing::warn!("搜索索引增量更新失败: {}", e);
    }

    // Phase 6: 保存最终状态（protected_docs 合并写回；doc_fingerprints 只记录实际写盘的文档）
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash().unwrap_or_default();
        save_generation_state(&config, &file_insights, &gen_output.documents, &protected, &head_hash);
    }

    let stats = AnalysisStats {
        files_scanned: file_insights.len(),
        files_parsed: file_insights.iter().filter(|f| !f.entities.is_empty()).count(),
        total_entities: graph.graph.node_count(),
        total_edges: graph.graph.edge_count(),
        modules_detected: _modules.len(),
        generation_time_ms: start.elapsed().as_millis() as u64,
    };

    Ok(AnalysisResult {
        graph,
        documents: gen_output.documents,
        cards: gen_output.cards,
        stats,
    })
}

/// 启动文件监听模式
pub fn run_watch(config_path: &Path) -> anyhow::Result<()> {
    let config = config::load_config(config_path)?;
    tracing::info!("首次全量生成...");
    run_pipeline(config_path, None, false)?;
    tracing::info!("全量生成完成，开始监听文件变更...");

    let config_path = config_path.to_path_buf();
    incremental::watch::run_watch_loop(&config, move || {
        tracing::info!("检测到文件变更，触发增量更新...");
        if let Err(e) = run_incremental_pipeline(&config_path, None, false) {
            tracing::error!("增量更新失败: {}", e);
        } else {
            tracing::info!("增量更新完成");
        }
    })
}

// ==================== 搜索索引集成 ====================

/// 获取搜索索引目录的绝对路径
fn search_index_dir(config: &config::schema::WikiConfig) -> std::path::PathBuf {
    Path::new(&config.output.dir).join(&config.search.index_dir)
}

/// 全量构建搜索索引
///
/// 遍历知识图谱中所有实体节点，从 FileInsight 中提取对应源码片段，
/// 批量索引到 TextEngine。如果 embed 已启用则同时构建 SemanticEngine。
fn build_search_index(
    graph: &model::KnowledgeGraph,
    file_insights: &[ingest::parser::FileInsight],
    config: &config::schema::WikiConfig,
) -> anyhow::Result<()> {
    let index_dir = search_index_dir(config);
    std::fs::create_dir_all(&index_dir)?;

    // 构建文件路径 → 源码的查找表
    let source_map = build_source_map(file_insights);

    // 收集所有需要索引的实体
    let items: Vec<(model::CodeNode, String)> = graph.graph.node_indices()
        .filter_map(|idx| {
            let node = graph.graph.node_weight(idx)?;
            // 跳过项目/模块/文件级别的节点，只索引具体实体
            if matches!(node.kind, model::NodeKind::Project | model::NodeKind::Module | model::NodeKind::File) {
                return None;
            }
            let source = extract_entity_source(node, &source_map);
            Some((node.clone(), source))
        })
        .collect();

    // 全量重建 TextEngine
    let text_path = index_dir.join("text_index.db");
    let _ = std::fs::remove_file(&text_path); // 清除旧索引
    let mut text_engine = search::text::TextEngine::open(&text_path)?;
    text_engine.index_batch(&items)?;

    // 如果 embed 已启用，构建语义索引
    if config.embed.enabled {
        let semantic_path = index_dir.join("semantic_index.db");
        let _ = std::fs::remove_file(&semantic_path);
        match generate::embed::EmbeddingEngine::new(&config.embed) {
            Ok(embedder) => {
                let embedder = std::sync::Arc::new(embedder);
                let mut semantic_engine = search::semantic::SemanticEngine::open(&semantic_path, embedder, get_global_runtime().clone())?;
                semantic_engine.index_batch(&items)?;
                tracing::info!("语义索引构建完成: {} 个实体已向量化", items.len());
            }
            Err(e) => {
                tracing::warn!("语义索引构建跳过（Embedding 引擎初始化失败）: {}", e);
            }
        }
    }

    tracing::info!("搜索索引构建完成: {} 个实体已索引", items.len());
    Ok(())
}

/// 增量更新搜索索引
///
/// 只删除变更文件的旧实体，再重新索引变更文件中的新实体。
/// 同时处理 TextEngine 和 SemanticEngine（如已启用）。
fn update_search_index_incremental(
    graph: &model::KnowledgeGraph,
    file_insights: &[ingest::parser::FileInsight],
    config: &config::schema::WikiConfig,
    changed_files: &std::collections::HashSet<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let index_dir = search_index_dir(config);
    let text_path = index_dir.join("text_index.db");

    // 索引不存在时回退到全量构建
    if !text_path.exists() {
        return build_search_index(graph, file_insights, config);
    }

    let mut text_engine = search::text::TextEngine::open(&text_path)?;

    // 删除变更文件的旧索引
    let mut total_removed = 0;
    for file in changed_files {
        let file_str = file.to_string_lossy();
        total_removed += text_engine.remove_by_file(&file_str)?;
    }

    // 重新索引变更文件中的实体
    let source_map = build_source_map(file_insights);
    let items: Vec<(model::CodeNode, String)> = graph.graph.node_indices()
        .filter_map(|idx| {
            let node = graph.graph.node_weight(idx)?;
            if matches!(node.kind, model::NodeKind::Project | model::NodeKind::Module | model::NodeKind::File) {
                return None;
            }
            // 只索引属于变更文件的实体
            let node_file = node.file_path.as_deref()?;
            if !changed_files.iter().any(|f| f.to_string_lossy() == node_file) {
                return None;
            }
            let source = extract_entity_source(node, &source_map);
            Some((node.clone(), source))
        })
        .collect();

    text_engine.index_batch(&items)?;

    // 增量更新语义索引（如已启用）
    if config.embed.enabled {
        let semantic_path = index_dir.join("semantic_index.db");
            if semantic_path.exists() && let Ok(embedder) = generate::embed::EmbeddingEngine::new(&config.embed) {
                let embedder = std::sync::Arc::new(embedder);
                if let Ok(mut semantic_engine) = search::semantic::SemanticEngine::open(&semantic_path, embedder, get_global_runtime().clone()) {
                    for file in changed_files {
                        let _ = semantic_engine.remove_by_file(&file.to_string_lossy());
                    }
                    let _ = semantic_engine.index_batch(&items);
                }
            }
    }

    tracing::info!("搜索索引增量更新: 删除 {} 条, 新增 {} 条", total_removed, items.len());
    Ok(())
}

/// 构建文件路径 → 文件源码的查找表（直接使用 FileInsight.source 避免重复 I/O）
fn build_source_map(insights: &[ingest::parser::FileInsight]) -> std::collections::HashMap<String, String> {
    insights.iter()
        .map(|i| (i.path.to_string_lossy().to_string(), i.source.clone()))
        .collect()
}

/// 从源码中提取实体对应的代码片段
///
/// 根据实体的 line_range 从源文件中截取对应行。
fn extract_entity_source(
    node: &model::CodeNode,
    source_map: &std::collections::HashMap<String, String>,
) -> String {
    let file_path = match &node.file_path {
        Some(p) => p,
        None => return node.signature.clone().unwrap_or_default(),
    };
    let source = match source_map.get(file_path) {
        Some(s) => s,
        None => return node.signature.clone().unwrap_or_default(),
    };
    let (start, end) = match node.line_range {
        Some(r) => r,
        None => return node.signature.clone().unwrap_or_default(),
    };
    // 截取对应行（1-based 转 0-based）
    source.lines()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 执行搜索查询（供 CLI search 子命令调用）
///
/// 加载持久化索引，根据引擎类型执行搜索，返回结果列表。
/// - Text: 仅 BM25 全文搜索
/// - Semantic: 仅向量语义搜索（需 embed.enabled）
/// - Hybrid: 两者结果经 RRF 合并
pub fn execute_search(
    config_path: &Path,
    query: &str,
    top_k: usize,
    engine_type: &config::schema::SearchEngineType,
) -> anyhow::Result<Vec<search::hybrid::SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let config = config::load_config(config_path)?;
    let index_dir = search_index_dir(&config);
    let text_path = index_dir.join("text_index.db");
    let semantic_path = index_dir.join("semantic_index.db");

    match engine_type {
        config::schema::SearchEngineType::Text => {
            if !text_path.exists() {
                anyhow::bail!("搜索索引不存在，请先运行 `repo-wiki generate` 构建索引");
            }
            let text_engine = search::text::TextEngine::open(&text_path)?;
            let results = text_engine.search(query, top_k)?;
            Ok(search::hybrid::text_results_to_hits(results))
        }
        config::schema::SearchEngineType::Semantic => {
            if !semantic_path.exists() {
                anyhow::bail!("语义索引不存在，请在配置中启用 embed 并运行 `repo-wiki generate`");
            }
            let embedder = generate::embed::EmbeddingEngine::new(&config.embed)?;
            let embedder = std::sync::Arc::new(embedder);
            let semantic_engine = search::semantic::SemanticEngine::open(&semantic_path, embedder, get_global_runtime().clone())?;
            let results = semantic_engine.search(query, top_k)?;
            Ok(search::hybrid::semantic_results_to_hits(results))
        }
        config::schema::SearchEngineType::Hybrid => {
            let text_engine = search::text::TextEngine::open(&text_path)?;
            let semantic_engine = if semantic_path.exists() && config.embed.enabled {
                generate::embed::EmbeddingEngine::new(&config.embed)
                    .ok()
                    .and_then(|e| search::semantic::SemanticEngine::open(&semantic_path, Arc::new(e), get_global_runtime().clone()).ok())
            } else { None };
            let agent = search::agent::SearchAgent::new(text_engine, semantic_engine, config.search.rrf_k as f64);
            Ok(agent.search(query, top_k, true))
        }
    }
}
