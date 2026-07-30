pub mod config;
pub mod model;
pub mod ingest;
pub mod analysis;
pub mod generate;
pub mod output;
pub mod incremental;
pub mod search;

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

/// 运行完整的分析流水线（配置文件路径）
pub fn run_pipeline(config_path: &std::path::Path) -> anyhow::Result<AnalysisResult> {
    let config = config::load_config(config_path)?;
    let _span = tracing::info_span!("pipeline", config = %config_path.display());
    let _enter = _span.enter();
    let start = std::time::Instant::now();

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;
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
    let runtime = tokio::runtime::Runtime::new()?;
    let gen_output = runtime.block_on(generate::run_generation(&graph, &file_insights, &config))?;

    // Phase 4: 输出
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config)?;

    // Phase 5: 保存增量状态
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash().unwrap_or_default();
        if let Ok(state) = incremental::state::GenerationState::from_insights(&file_insights, &head_hash) {
            let state_dir = std::path::Path::new(&config.output.dir).join(".state");
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
pub fn run_incremental_pipeline(config_path: &std::path::Path) -> anyhow::Result<AnalysisResult> {
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
    let runtime = tokio::runtime::Runtime::new()?;
    let changed_set: std::collections::HashSet<std::path::PathBuf> = inc_result.changed_files.into_iter().collect();
    let gen_output = runtime.block_on(
        generate::run_generation_filtered(&graph, &file_insights, &config, &changed_set)
    )?;

    // Phase 4: 全量输出（保持索引一致）
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config)?;

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
pub fn run_watch(config_path: &std::path::Path) -> anyhow::Result<()> {
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
