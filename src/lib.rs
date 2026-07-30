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

/// 运行完整的分析流水线（配置文件路径）
pub fn run_pipeline(config_path: &Path) -> anyhow::Result<AnalysisResult> {
    let config = config::load_config(config_path)?;
    let _span = tracing::info_span!("pipeline", config = %config_path.display());
    let _enter = _span.enter();
    let start = std::time::Instant::now();

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;
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
    stats.total_entities = graph.graph.node_count();
    stats.total_edges = graph.graph.edge_count();
    stats.modules_detected = modules.len();

    // Phase 3: 生成（需要 tokio 运行时）
    let rt = get_global_runtime();
    let gen_output = rt.block_on(generate::run_generation(&graph, &file_insights, &config))?;

    // Phase 4: 输出
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config)?;

    // Phase 5: 构建搜索索引
    if config.search.enabled && let Err(e) = build_search_index(&graph, &file_insights, &config) {
        tracing::warn!("搜索索引构建失败（不影响主流程）: {}", e);
    }

    // Phase 6: 保存增量状态（含文档指纹用于人工修改保护）
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash().unwrap_or_default();
        if let Ok(mut state) = incremental::state::GenerationState::from_insights(&file_insights, &head_hash) {
            let output_dir = std::path::Path::new(&config.output.dir);
            if let Ok(doc_fps) = incremental::state::GenerationState::record_doc_fingerprints(&gen_output.documents, output_dir) {
                state.doc_fingerprints = doc_fps;
            }
            let state_dir = output_dir.join(".state");
            let _ = state.save(&state_dir);
        }
    }

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

/// 运行增量更新流水线
pub fn run_incremental_pipeline(config_path: &Path) -> anyhow::Result<AnalysisResult> {
    let config = config::load_config(config_path)?;
    let start = std::time::Instant::now();

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;

    // Phase 2: 分析
    let graph = analysis::build_graph(&file_insights)?;
    let _modules = analysis::detect_modules(&graph)?;

    // 检查增量变更
    let inc_result = incremental::run_incremental_update(&file_insights, &graph, &config)?;

    if inc_result.affected_modules.is_empty() {
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
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config)?;

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
    run_pipeline(config_path)?;
    tracing::info!("全量生成完成，开始监听文件变更...");

    let config_path = config_path.to_path_buf();
    incremental::watch::run_watch_loop(&config, move || {
        tracing::info!("检测到文件变更，触发增量更新...");
        if let Err(e) = run_incremental_pipeline(&config_path) {
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
