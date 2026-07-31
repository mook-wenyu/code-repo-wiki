pub mod config;
pub mod model;
pub mod ingest;
pub mod analysis;
pub mod generate;
pub mod output;
pub mod incremental;
pub mod search;
pub mod commands;

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
    cards: &[model::KnowledgeCard],
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
            cards,
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
        save_generation_state(&config, &file_insights, &gen_output.documents, &gen_output.cards, &protected, &head_hash);
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
/// `force` 为 true 时清空人工修改保护集（与 run_pipeline 的 --force 语义一致，
/// 经由 load_protection 统一处理，force=false 保留保护语义）。
/// `watch_paths` 为文件监听外部传入的事件路径（FileWatch 策略使用，
/// 普通增量更新传 &[]）。
/// `change_kind` 为监听事件携带的变更类型（普通增量更新传 None）；
/// Deleted 事件直入删除清理路径，不再依赖下游对路径做 exists() 推断。
pub fn run_incremental_pipeline(
    config_path: &Path,
    output: Option<&Path>,
    force: bool,
    watch_paths: &[std::path::PathBuf],
    change_kind: Option<incremental::watch::ChangeKind>,
) -> anyhow::Result<AnalysisResult> {
    let config = load_config_with_output(config_path, output)?;
    let start = std::time::Instant::now();

    // 保护集必须在增量分析之前加载（增量分析内部会重写 state 文件）
    let (protected, old_state) = load_protection(&config, force);

    // Deleted 事件直入删除清理：watch 层已显式标记删除，直接清理
    // 对应产物（wiki 页 + 卡片）。删除路径随后仍进入增量流程
    // （FileWatch 策略并入 changed_files），驱动搜索索引清理与状态保存。
    if change_kind == Some(incremental::watch::ChangeKind::Deleted) {
        cleanup_deleted_outputs(&config, watch_paths);
    }

    // Phase 1: 扫描
    let file_insights = ingest::scan_and_parse(&config)?;

    // Phase 2: 分析
    let graph = analysis::build_graph(&file_insights)?;
    let _modules = analysis::detect_modules(&graph)?;

    // 检查增量变更（watch_paths 透传：FileWatch 策略下删除事件路径
    // 由此进入 changed_files，驱动下游删除清理）
    let inc_result = incremental::run_incremental_update(&file_insights, &graph, &config, watch_paths)?;

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
    let mut gen_output = rt.block_on(
        generate::run_generation_filtered(&graph, &file_insights, &config, &changed_set)
    )?;

    // Phase 3b: 人工修改反向同步——检测到的人工修改页写入对应卡片的
    // pending_manual_edits（下次生成时作为 LLM 输入；受保护页面本身不被覆盖）
    if let Some(state) = &old_state {
        let modified = state.detect_manually_modified();
        if !modified.is_empty() {
            inject_manual_edits(&mut gen_output.cards, &modified);
        }
    }

    // Phase 4: 全量输出（保持索引一致）
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config, &protected)?;

    // Phase 4b: 清理已删除文件对应的输出文档（wiki 页 + 卡片）
    cleanup_deleted_outputs(&config, &inc_result.changed_files);

    // Phase 5: 增量更新搜索索引
    if config.search.enabled && let Err(e) = update_search_index_incremental(&graph, &file_insights, &config, &changed_set) {
        tracing::warn!("搜索索引增量更新失败: {}", e);
    }

    // Phase 6: 保存最终状态（protected_docs 合并写回；doc_fingerprints 只记录实际写盘的文档）
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash().unwrap_or_default();
        save_generation_state(&config, &file_insights, &gen_output.documents, &gen_output.cards, &protected, &head_hash);
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

/// 从源码文件路径派生模块名（与 generate::chunk::chunk_by_file 同规则：
/// 取目录的普通组件以 "::" 连接，不含文件名）
///
/// 删除清理时被删文件已不在新 graph/insights 中，无法还原其模块聚类归属，
/// 只能按此确定性规则重建模块名；wiki 页文件名 = 模块名.replace("::","_")，
/// 与 render_all 落盘（module_path.join("_")）完全一致。
fn module_name_from_path(path: &Path) -> String {
    path.parent()
        .map(|p| {
            p.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("::")
        })
        .unwrap_or_default()
}

/// 清理已删除源文件对应的旧输出（wiki 页 + 卡片）
///
/// 只清理"已不存在"的变更文件（deleted 与 renamed 旧路径由
/// incremental::run_incremental_update 计入 changed_files）；
/// wiki 页与卡片路径复用 output 层的统一命名规则，并遍历全部语言目录
/// （主语言 + 扩展语言），与 render_all 的写盘规则一致。
/// 现存文件、已保护文档的路径不在变更集内，不受影响。
pub(crate) fn cleanup_deleted_outputs(
    config: &config::schema::WikiConfig,
    changed_files: &[std::path::PathBuf],
) {
    let output_dir = Path::new(&config.output.dir);
    let languages = output::wiki_languages(config);
    for f in changed_files {
        if f.exists() {
            continue;
        }
        let module = module_name_from_path(f);
        if module.is_empty() {
            continue;
        }
        let file_name = output::module_page_file_name(&module);
        for lang in &languages {
            // 被删文件的旧页面/旧卡片逐一尝试删除，失败静默（文件可能本就不存在）
            let _ = std::fs::remove_file(output_dir.join("wiki").join(lang).join(&file_name));
            let _ = std::fs::remove_file(output::card_page_path(output_dir, lang, &module));
        }
    }
}

/// 将人工修改的文档路径与内容摘要写入对应模块卡片的 `pending_manual_edits`
///
/// 官方语义："人工修改反向同步到对应知识卡片"——被人工编辑过的页面不
/// 被自动更新覆盖，且修改被记录到卡片，下次生成时作为 LLM 输入提示。
///
/// 定位规则：文档文件名主体（如 `src_testmodule`）与卡片文件名主体
/// （`module.replace("::","_")`，output::card_file_stem 同一来源）一致
/// 即视为对应卡片；wiki 页与卡片两类人工修改均按此规则回写。
/// 内容摘要取磁盘当前内容（人工修改版）前 200 字符。
pub fn inject_manual_edits(cards: &mut [model::KnowledgeCard], modified_paths: &[String]) {
    for path in modified_paths {
        let Some(stem) = Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
        else {
            continue;
        };
        let summary = std::fs::read_to_string(path)
            .map(|content| content.chars().take(200).collect::<String>())
            .unwrap_or_default();
        let note = format!("人工修改待同步: {path} 内容摘要: {summary}");
        for card in cards.iter_mut() {
            if output::card_file_stem(&card.module_name) == stem
                && !card.pending_manual_edits.contains(&note)
            {
                card.pending_manual_edits.push(note.clone());
            }
        }
    }
}

/// 启动文件监听模式
pub fn run_watch(config_path: &Path) -> anyhow::Result<()> {
    let config = config::load_config(config_path)?;
    tracing::info!("首次全量生成...");
    run_pipeline(config_path, None, false)?;
    tracing::info!("全量生成完成，开始监听文件变更...");

    let config_path = config_path.to_path_buf();
    // 监听根与 scan_and_parse 的扫描根保持一致（config 无项目根字段，均取当前目录）
    let root = std::env::current_dir()?;
    incremental::watch::run_watch_loop(&root, &config, move |events| {
        for event in events {
            tracing::info!(
                "检测到 {:?} {} 个文件变更，触发增量更新...",
                event.kind,
                event.paths.len()
            );
            // 事件类型显式传递：Deleted 直入删除清理（pipeline 内处理），
            // 其余 kind 走常规增量更新
            let change_kind = (event.kind == incremental::watch::ChangeKind::Deleted)
                .then_some(event.kind);
            if let Err(e) =
                run_incremental_pipeline(&config_path, None, false, &event.paths, change_kind)
            {
                tracing::error!("增量更新失败: {}", e);
            } else {
                tracing::info!("增量更新完成");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::WikiSection;
    use std::path::PathBuf;

    /// A1 集成测试：已删除源文件对应的 wiki 页与卡片在全部语言目录下被清理，
    /// 现存文件的输出不受影响。
    #[test]
    fn test_cleanup_deleted_outputs_removes_all_languages() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_cleanup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = crate::config::schema::WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();
        // 双语言：主语言 zh + 扩展语言 en
        config.wiki = WikiSection {
            language: "zh".into(),
            expand_languages: vec!["en".into()],
            ..Default::default()
        };

        // 预置被删文件 src/foo.rs 的旧输出：模块 "src" → wiki 页 src.md、卡片 src.md
        // （deleted 用仓库内相对路径构造，与 git diff 的路径形态一致）
        let deleted = PathBuf::from("src/foo.rs");
        for lang in ["zh", "en"] {
            let wiki = dir.join("wiki").join(lang).join("src.md");
            let card = dir.join("cards").join(lang).join("src.md");
            std::fs::create_dir_all(wiki.parent().unwrap()).unwrap();
            std::fs::create_dir_all(card.parent().unwrap()).unwrap();
            std::fs::write(&wiki, "旧页面").unwrap();
            std::fs::write(&card, "旧卡片").unwrap();
        }

        // 现存文件（绝对路径，存在于磁盘）不得被清理
        let alive = dir.join("lib").join("util.rs");
        std::fs::create_dir_all(alive.parent().unwrap()).unwrap();
        std::fs::write(&alive, "存活").unwrap();
        let alive_wiki = dir.join("wiki").join("zh").join("lib.md");
        std::fs::create_dir_all(alive_wiki.parent().unwrap()).unwrap();
        std::fs::write(&alive_wiki, "存活页面").unwrap();

        cleanup_deleted_outputs(&config, &[deleted, alive]);

        for lang in ["zh", "en"] {
            assert!(
                !dir.join("wiki").join(lang).join("src.md").exists(),
                "已删文件的 wiki 页应被清理（{lang}）"
            );
            assert!(
                !dir.join("cards").join(lang).join("src.md").exists(),
                "已删文件的卡片应被清理（{lang}）"
            );
        }
        assert!(dir.join("wiki").join("zh").join("lib.md").exists(), "现存文件的输出不应被清理");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1 单测：模块名派生规则（与 chunk_by_file 一致，不含文件名）
    #[test]
    fn test_module_name_from_path_uses_directories() {
        assert_eq!(module_name_from_path(Path::new("src/config.rs")), "src");
        assert_eq!(module_name_from_path(Path::new("src/foo/bar.rs")), "src::foo");
        // Windows 盘符不进入模块名（与 chunk_by_file 的 Normal 组件过滤一致）
        assert_eq!(module_name_from_path(Path::new("C:/src/config.rs")), "src");
        // 根目录文件无模块名
        assert_eq!(module_name_from_path(Path::new("config.rs")), "");
    }

    /// A2：force=true 清空保护集（含旧 protected_docs 与人工修改检测），
    /// force=false 保留保护语义 —— 与 run_pipeline 的 --force 行为一致
    #[test]
    fn test_load_protection_force_clears_protection() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_force_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = crate::config::schema::WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();

        // 构造旧 state：一个"人工修改过"的文档（磁盘内容与指纹不匹配）
        let state_dir = dir.join(".state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let doc_path = dir.join("wiki").join("zh").join("src.md");
        std::fs::create_dir_all(doc_path.parent().unwrap()).unwrap();
        std::fs::write(&doc_path, "人工修改后的内容").unwrap();
        let mut state = incremental::state::GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::new(),
            module_fingerprints: std::collections::HashMap::new(),
            doc_fingerprints: std::collections::HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
        };
        state.doc_fingerprints.insert(
            doc_path.to_string_lossy().to_string(),
            "与磁盘内容不同的指纹".into(),
        );
        state.save(&state_dir).unwrap();

        // force=false：保护集包含检测出的人工修改（下次生成不覆盖）
        let (protected, _) = load_protection(&config, false);
        assert!(
            protected.contains(&doc_path.to_string_lossy().to_string()),
            "force=false 应保护人工修改的文档"
        );

        // force=true：保护集清空（render_all 将覆盖所有文档）
        let (protected, _) = load_protection(&config, true);
        assert!(protected.is_empty(), "force=true 应清空保护集");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
