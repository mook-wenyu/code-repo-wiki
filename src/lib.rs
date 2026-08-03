pub mod config;
pub mod model;
pub mod ingest;
pub mod analysis;
pub mod generate;
pub mod output;
pub mod incremental;
pub mod search;
pub mod commands;
pub mod fs;
pub mod mcp;
pub mod project;
pub mod bench;

use std::collections::HashMap;
use std::path::Path;

use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

use anyhow::{bail, Context};

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

/// 全局 tokio 运行时（流水线与 MCP server 共用，避免重复初始化）
pub fn get_global_runtime() -> &'static Arc<Runtime> {
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
fn load_config_with_output(
    config_path: &Path,
    output: Option<&Path>,
    root: &project::ProjectRoot,
) -> anyhow::Result<config::schema::WikiConfig> {
    let config = config::load_config(config_path)?;
    let mut config = if let Some(out) = output {
        let mut c = config;
        c.output.dir = out.to_string_lossy().into_owned();
        c
    } else {
        config
    };
    // wiki_plan.yaml 的 scope 覆盖相对项目根解析（不依赖进程 cwd）
    if let Some(plan) = crate::config::plan::resolve_plan_at(root, &config)?
        && let Some(scope) = plan.scope_override
    {
        config.scope = scope;
    }
    Ok(config)
}

/// 加载保护集：旧 state 的 protected_docs + 新检测出的人工修改；force 时清空
///
/// 损坏语义（票 02，fail-loud 裁决）：state 文件不存在 = 合法首次运行
/// （返回空保护）；存在但读取/解析失败 = 状态损坏（半截写、被外部
/// 改动）——损坏状态会让 protected_docs 静默丢失，人工修改保护失效，
/// 故显式报错中断（与 sync_from_git 对损坏状态的拒绝行为一致），
/// 由用户删除 .state/ 后重新 generate 重建。
fn load_protection(
    config: &config::schema::WikiConfig,
    force: bool,
) -> anyhow::Result<(std::collections::HashSet<String>, Option<incremental::state::GenerationState>)> {
    if force {
        return Ok((std::collections::HashSet::new(), None));
    }
    let state_dir = Path::new(&config.output.dir).join(".state");
    let state_path = state_dir.join("generation_state.json");
    if !state_path.exists() {
        // 无状态文件 = 从未生成过，合法空保护
        return Ok((std::collections::HashSet::new(), None));
    }
    let state = incremental::state::GenerationState::load(&state_dir).with_context(|| {
        format!(
            "状态文件损坏或不可读: {}（删除该文件后重新运行 generate 可重建）",
            state_path.display()
        )
    })?;
    let mut protected: std::collections::HashSet<String> = state
        .protected_docs
        .iter()
        .cloned()
        .collect();
    for p in state.detect_manually_modified() {
        protected.insert(p);
    }
    Ok((protected, Some(state)))
}

/// 保存生成状态：doc_fingerprints 只记录实际写盘的文档（跳过保护集），
/// protected_docs 合并本次保护集写回
fn save_generation_state(
    root: &project::ProjectRoot,
    config: &config::schema::WikiConfig,
    insights: &[ingest::parser::FileInsight],
    documents: &[model::WikiDocument],
    cards: &[model::KnowledgeCard],
    protected: &std::collections::HashSet<String>,
    commit_hash: &str,
) {
    let output_dir = Path::new(&config.output.dir);
    let state_dir = output_dir.join(".state");
    if let Ok(mut state) = incremental::state::GenerationState::from_insights(root, insights, commit_hash) {
        let mut protected_docs: Vec<String> = protected.iter().cloned().collect();
        protected_docs.sort();
        state.protected_docs = protected_docs;
        if let Ok((fps, modules)) = incremental::state::GenerationState::record_doc_fingerprints(
            documents,
            cards,
            output_dir,
            &output::wiki_languages(config),
        ) {
            // 全量记录指纹与模块归属（含保护集文档）：受保护文档本轮被跳过
            // 写盘，磁盘上仍是人工版，记录的即人工版指纹——下次再被人为修改
            // 时指纹比对仍能命中检测，反向同步可持续生效；卡片侧的记录注入
            // 自带去重（contains 检查），同一修改不会重复同步。
            state.doc_fingerprints = fps;
            state.doc_modules = modules;
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

/// 生成模式（票 12：双流水线合并为单入口的 mode 区分）
///
/// - `Full`：全量扫描解析 + 全量 LLM 生成 + 全量索引重建（generate 命令）
/// - `Incremental`：parse 层增量（解析缓存）+ 过滤生成 + 增量索引（update 命令）
#[derive(Debug, Clone)]
pub enum GenerationMode {
    Full,
    Incremental {
        /// 外部监听事件路径（watch 传入；普通增量更新传空）
        watch_paths: Vec<std::path::PathBuf>,
        /// 监听事件携带的变更类型（Deleted 直入删除清理）
        change_kind: Option<incremental::watch::ChangeKind>,
    },
}

/// 运行完整的分析流水线（配置文件路径）
///
/// `output` 非空时覆盖配置文件中的 output.dir（对应 CLI 的 --output 参数），
/// 后续渲染、搜索索引、状态目录全部使用覆盖后的值。
/// `force` 为 true 时清空人工修改保护集并覆盖所有文档（对应 CLI 的 --force）。
/// `root` 为项目根（扫描根 + git 定位 + watch 根的注入载体，--root 参数）
/// `mode` 区分全量生成与增量更新（两者共享本函数的主干，差异点在
/// 扫描缓存、变更分析、生成过滤、索引更新四处）。
pub fn run_pipeline(
    config_path: &Path,
    output: Option<&Path>,
    force: bool,
    root: &project::ProjectRoot,
    mode: &GenerationMode,
) -> anyhow::Result<AnalysisResult> {
    run_pipeline_with_progress(config_path, output, force, root, mode, &|_| {})
}

/// 运行完整的分析流水线，并在各阶段边界回调进度事件
///
/// 事件点：scanning 10 / analyzing 25 / chunking 30 / cards 60 / wiki 90 /
/// output 95 / index 98 / done 100，对应扫描、分析、生成、渲染、索引、保存阶段。
pub fn run_pipeline_with_progress(
    config_path: &Path,
    output: Option<&Path>,
    force: bool,
    root: &project::ProjectRoot,
    mode: &GenerationMode,
    on_progress: &dyn Fn(ProgressEvent),
) -> anyhow::Result<AnalysisResult> {
    let config = load_config_with_output(config_path, output, root)?;
    let _span = tracing::info_span!("pipeline", config = %config_path.display());
    let _enter = _span.enter();
    let start = std::time::Instant::now();
    let is_incremental = matches!(mode, GenerationMode::Incremental { .. });

    // 保护集：旧 state 的 protected_docs + 检测出的人工修改；force 时清空。
    // old_state 同时供人工修改反向同步组装（collect_manual_edits → 生成前注入）
    let (protected, old_state) = load_protection(&config, force)?;

    // Phase 1: 扫描。增量模式启用解析缓存（parse 层增量：内容指纹未变复用
    // 缓存结果，仅变更文件重新 tree-sitter 解析）；全量模式直接全量解析。
    let watch_list: Vec<std::path::PathBuf> = match mode {
        GenerationMode::Incremental { watch_paths, .. } => watch_paths.clone(),
        GenerationMode::Full => Vec::new(),
    };
    // 事件路径统一相对化（相对项目根）：scan 产出的 insight 路径已是相对
    // 扫描根，watch 层外部传入的路径必须对齐同一基准，否则路径比较
    // （缓存判定/变更集判定）对绝对路径恒不命中。
    let watch_paths: Vec<std::path::PathBuf> = watch_list
        .iter()
        .map(|p| p.strip_prefix(root.path()).map(|r| r.to_path_buf()).unwrap_or_else(|_| p.clone()))
        .collect();
    let watch_set: std::collections::HashSet<std::path::PathBuf> =
        watch_paths.iter().cloned().collect();
    let file_insights = if is_incremental {
        let cache_path = Path::new(&config.output.dir).join(".state").join("insights_cache.json");
        ingest::scan_and_parse_cached_at(root, &config, &Some(cache_path), &watch_set)?
    } else {
        ingest::scan_and_parse_at(root, &config)?
    };
    on_progress(ProgressEvent { stage: "scanning", percent: 10 });
    if file_insights.is_empty() {
        bail!("未找到任何源文件");
    }
    let mut stats = AnalysisStats {
        files_scanned: file_insights.len(),
        files_parsed: file_insights.iter().filter(|f| !f.entities.is_empty()).count(),
        ..Default::default()
    };

    // Phase 2: 分析（build_graph 内部完成模块检测并写回 graph.modules，
    // 此处直接读结果供 stats，不重复运行检测）
    let mut graph = analysis::build_graph(&file_insights)?;
    attach_features(&mut graph, &config);
    on_progress(ProgressEvent { stage: "analyzing", percent: 25 });
    stats.total_entities = graph.graph.node_count();
    stats.total_edges = graph.graph.edge_count();
    stats.modules_detected = graph.modules.len();

    // Phase 2b: 增量变更分析（git diff + 实体级变化分类 + 语义传播；
    // 全量模式跳过。diff 超限/非 git 仓库时内部回退全量语义）
    let inc_result = if is_incremental {
        Some(incremental::run_incremental_update_at(root, &file_insights, &graph, &config, &watch_paths)?)
    } else {
        None
    };

    // 无代码变更短路：仅增量模式存在；此时若有新检测的人工修改仍需
    // 反向同步到卡片文件（生成路径跳过时此处的直接写盘是唯一落卡途径，
    // 记录在下次有变更的生成时经 extract_pending_manual_edits 注入 LLM 输入）
    if let Some(inc) = &inc_result
        && inc.changed_files.is_empty()
    {
        if let Some(state) = &old_state {
            let synced = sync_manual_edits_to_cards(&config, state)?;
            if synced > 0 {
                tracing::info!("人工修改已反向同步到 {} 张卡片", synced);
            }
        }
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

    // Phase 3: 生成（需要 tokio 运行时）。人工修改记录在生成前注入
    // LLM 输入（collect_manual_edits：旧状态指纹比对 + 模块归属精确匹配）。
    // 增量模式只对变更文件 + 语义传播判定的受影响模块过滤生成
    //（run_generation_filtered），全量模式全量生成。
    on_progress(ProgressEvent { stage: "chunking", percent: 30 });
    let rt = get_global_runtime();
    let extra_edits = collect_manual_edits(old_state.as_ref());
    let mut gen_output = if let Some(inc) = &inc_result {
        rt.block_on(generate::run_generation_filtered(
            &graph, &file_insights, &config, root, inc, &extra_edits,
        ))?
    } else {
        rt.block_on(generate::run_generation(&graph, &file_insights, &config, root, &extra_edits))?
    };
    on_progress(ProgressEvent { stage: "cards", percent: 60 });

    // Phase 3b: 阅读指南 index.md（仅主语言，写盘路径 wiki/{主语言}/index.md）。
    // LLM 失败重试 1 次仍失败 → 降级确定性骨架（模块入度中心度降序的链接列表）；
    // provider 构建失败（理论不可达：run_generation 已保证 LLM 配置可用）同样降级。
    // 错误处理与全局文档（架构/概览）一致：失败只告警，不中断主流程。
    let index_doc = match generate::create_provider(&config) {
        Ok(provider) => rt.block_on(generate::index::generate_index_guide(
            &provider,
            &graph,
            &gen_output.cards,
            &config,
        )),
        Err(e) => {
            tracing::warn!("阅读指南 LLM 不可用，降级为确定性骨架: {e}");
            generate::index::fallback_index_guide(&graph, &config)
        }
    };
    gen_output.documents.push(index_doc);

    // Phase 4: 输出（render_all 内部同步写导出快照；产物集合 diff 清理
    // 全量/增量统一：旧状态记录过但本次未生成的产物（含已删模块的
    // 旧页面/卡片）一律清理，module_{n} 档不再漏删）
    on_progress(ProgressEvent { stage: "wiki", percent: 90 });
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config, &protected)?;
    cleanup_stale_outputs(
        old_state.as_ref(),
        &output::rendered_paths(&gen_output.documents, &gen_output.cards, &config),
    );
    on_progress(ProgressEvent { stage: "output", percent: 95 });

    // Phase 5: 构建/增量更新搜索索引
    if config.search.enabled {
        let index_result = if is_incremental {
            let changed_set: std::collections::HashSet<std::path::PathBuf> = inc_result
                .as_ref()
                .map(|i| i.changed_files.iter().cloned().collect())
                .unwrap_or_default();
            update_search_index_incremental(&graph, &file_insights, &config, &changed_set)
        } else {
            build_search_index(&graph, &file_insights, &config)
        };
        if let Err(e) = index_result {
            tracing::warn!("搜索索引构建失败（不影响主流程）: {}", e);
        }
    }
    on_progress(ProgressEvent { stage: "index", percent: 98 });

    // Phase 6: 保存增量状态（含文档指纹用于人工修改保护）
    if config.incremental.enabled {
        let head_hash = incremental::diff::get_head_commit_hash_at(root).unwrap_or_default();
        save_generation_state(root, &config, &file_insights, &gen_output.documents, &gen_output.cards, &protected, &head_hash);
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

/// 知识卡片操作（CLI card 子命令与 Qoder /knowledge 对等）
pub fn run_card_command(
    config_path: &Path,
    root: &project::ProjectRoot,
    action: &generate::card::CardAction,
) -> anyhow::Result<()> {
    let config = load_config_with_output(config_path, None, root)?;
    // 编辑类动作要求卡片已存在：先校验（错误信息优先于 LLM API Key 检查）
    match action {
        generate::card::CardAction::Generate { .. } => {}
        generate::card::CardAction::Modify { module, .. }
        | generate::card::CardAction::Supplement { module, .. }
        | generate::card::CardAction::Rewrite { module, .. } => {
            if generate::card::read_card(&config, module)?.is_none() {
                anyhow::bail!("模块 {module} 的卡片不存在，请先运行 `repo-wiki generate` 或 `repo-wiki card generate {module}` 生成");
            }
        }
    }
    let provider = generate::create_provider(&config)?;
    let rt = get_global_runtime();
    match action {
        generate::card::CardAction::Generate { module } => {
            rt.block_on(generate::card::generate_module_card(&provider, &config, root, module))
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


/// 清理过期产物（票 10：产物集合 diff 语义，全量/增量统一）
///
/// 语义：状态中记录过的旧产物路径（doc_fingerprints/doc_modules 键，即
/// 上次生成写盘的 wiki 页与卡片全集）减去本次实际生成的产物集合
/// （output::rendered_paths：含受保护文档路径——受保护文档属于生成集，
/// 磁盘上是人工版，diff 后天然不在待删集合，不会误删人工编辑内容）。
/// 差集 = 已消失模块/重命名模块的旧产物，一律删除。
///
/// 与旧实现（cleanup_deleted_outputs 按被删文件路径推导模块名）相比：
/// 不依赖模块名路径推导，module_{n}（无目录社区）档不再漏删；全量
/// generate 也清理旧产物（旧实现仅增量路径调用）。
///
/// 删除失败显式告警（文件被占用等），不静默吞错。
pub(crate) fn cleanup_stale_outputs(
    old_state: Option<&incremental::state::GenerationState>,
    rendered: &[std::path::PathBuf],
) {
    let Some(state) = old_state else {
        return; // 无旧状态（首次生成）：不存在可清理的旧产物
    };
    let mut stale: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    stale.extend(state.doc_fingerprints.keys().map(String::as_str));
    stale.extend(state.doc_modules.keys().map(String::as_str));
    let rendered_set: std::collections::BTreeSet<String> = rendered
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut removed = 0usize;
    for path in stale {
        if rendered_set.contains(path) {
            continue;
        }
        let p = Path::new(path);
        if p.exists() {
            match std::fs::remove_file(p) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!("清理过期产物失败 {}: {}", p.display(), e),
            }
        }
    }
    if removed > 0 {
        tracing::info!("清理过期产物 {} 个", removed);
    }
}

/// 组装"人工修改 → 卡片记录"映射（模块名 → 记录文本列表）
///
/// 官方语义："人工修改反向同步到对应知识卡片"——被人工编辑过的页面不
/// 被自动更新覆盖，且修改被记录到卡片，下次生成时作为 LLM 输入提示。
///
/// 来源 = 状态中指纹不匹配的产物路径（detect_manually_modified）+ 其模块
/// 归属（doc_modules 精确映射：产物路径 → 模块名）。精确匹配杜绝了旧实现
/// stem 匹配在模块名含下划线时（src::foo_bar vs src::foo::bar 均压平为
/// src_foo_bar）的串卡片歧义；无模块归属的全局文档（api/overview/toc）跳过。
/// 记录在生成层（CardGenerator::generate_all_cards）于 LLM 输入前合并注入。
pub fn collect_manual_edits(
    state: Option<&incremental::state::GenerationState>,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Some(state) = state else { return out };
    for path in state.detect_manually_modified() {
        let Some(module) = state.doc_modules.get(&path) else {
            continue;
        };
        let summary = std::fs::read_to_string(&path)
            .map(|content| content.chars().take(200).collect::<String>())
            .unwrap_or_default();
        let note = format!("人工修改待同步: {path} 内容摘要: {summary}");
        out.entry(module.clone()).or_default().push(note);
    }
    out
}

/// 将检测到的人工修改记录直接同步到磁盘卡片（无代码变更时的反向同步路径）
///
/// 生成路径（有代码变更）由 CardGenerator 在 LLM 输入前注入记录并随卡片
/// 重写落盘；本函数覆盖"无代码变更但有人工修改"的场景——update 因
/// changed_files 为空而跳过生成时，人工修改记录也必须落到卡片文件：
/// 读现有卡片文本 → 合并记录（去重，含已有"人工修改待同步"节时在节内
/// 追加，否则在文件末尾新建节）→ 重写。记录下次生成时经
/// extract_pending_manual_edits 恢复为 LLM 输入，两条路径最终都收敛于
/// 卡片文件，保证反向同步语义在任何更新形态下都不丢。
pub fn sync_manual_edits_to_cards(
    config: &config::schema::WikiConfig,
    state: &incremental::state::GenerationState,
) -> anyhow::Result<usize> {
    let edits = collect_manual_edits(Some(state));
    if edits.is_empty() {
        return Ok(0);
    }
    let mut synced = 0usize;
    for (module, notes) in &edits {
        let card_path =
            output::card_page_path(Path::new(&config.output.dir), &config.wiki.language, module);
        let mut content = std::fs::read_to_string(&card_path).unwrap_or_default();
        let mut changed = false;
        for note in notes {
            if content.contains(note.as_str()) {
                continue;
            }
            changed = true;
            if let Some(section) = content.find("## 人工修改待同步") {
                // 节内追加：定位节后第一个空白行（节标题与列表之间）
                let insert_at = content[section..]
                    .find("\n\n")
                    .map(|i| section + i + 2)
                    .unwrap_or(content.len());
                content.insert_str(insert_at, &format!("- {note}\n"));
            } else {
                content.push_str(&format!("\n## 人工修改待同步\n\n- {note}\n"));
            }
        }
        if changed {
            std::fs::write(&card_path, content)?;
            synced += 1;
        }
    }
    Ok(synced)
}

/// 启动文件监听模式
///
/// `root` 为注入的项目根：首次全量生成与监听根均以它为基准
/// （扫描根一致，watch 常驻进程的 cwd 漂移不影响监听范围）。
pub fn run_watch(config_path: &Path, root: &project::ProjectRoot) -> anyhow::Result<()> {
    let config = config::load_config(config_path)?;
    tracing::info!("首次全量生成...");
    run_pipeline(config_path, None, false, root, &GenerationMode::Full)?;
    tracing::info!("全量生成完成，开始监听文件变更...");

    let config_path = config_path.to_path_buf();
    // 监听根 = 注入的项目根（与 scan_and_parse_at 的扫描根一致）
    let watch_root = root.path().to_path_buf();
    let watch_root_for_loop = watch_root.clone();
    incremental::watch::run_watch_loop(&watch_root_for_loop, &config, move |events| {
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
            let root = project::ProjectRoot::new(watch_root.clone());
            let mode = GenerationMode::Incremental {
                watch_paths: event.paths.clone(),
                change_kind,
            };
            if let Err(e) = run_pipeline(&config_path, None, false, &root, &mode) {
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

/// 实体级特征聚类接线（演进计划 T1.2b）
///
/// 在 build_graph 之后调用：embed 未启用或 EmbeddingEngine 初始化失败时
/// 降级为纯结构聚类（detect_features 的 embedder 参数传 None）。
/// 特征聚类失败只告警不中断主流程（特征是附加信息，不影响生成主链路）。
fn attach_features(graph: &mut model::KnowledgeGraph, config: &config::schema::WikiConfig) {
    let embedder: Option<std::sync::Arc<dyn analysis::feature::Embedder>> = if config.embed.enabled {
        match generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone()) {
            Ok(e) => {
                // 显式经中间 let 触发 unsize coercion（Option 内不自动转换）
                let engine: std::sync::Arc<dyn analysis::feature::Embedder> = std::sync::Arc::new(e);
                Some(engine)
            }
            Err(e) => {
                tracing::warn!("特征聚类 Embedding 初始化失败，降级为纯结构聚类: {e}");
                None
            }
        }
    } else {
        None
    };
    match analysis::feature::detect_features(graph, embedder.as_deref()) {
        Ok(features) => {
            graph.features = features;
            tracing::info!("特征聚类完成: {} 个特征", graph.features.len());
        }
        Err(e) => {
            tracing::warn!("特征聚类失败（不影响主流程）: {e}");
        }
    }
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
        // 票 10 时序修正：先初始化 Embedding 引擎、成功后再删旧索引——
        // 旧实现先删后初始化，key 缺失时旧索引已丢且引导误导
        //（"请启用 embed"掩盖了真实原因是 key 未配置）。
        // 失败时保留旧索引（可回退旧语义结果），并在引导中区分两种失败。
        match generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone()) {
            Ok(embedder) => {
                let _ = std::fs::remove_file(&semantic_path);
                let embedder = std::sync::Arc::new(embedder);
                let mut semantic_engine = search::semantic::SemanticEngine::open(&semantic_path, embedder, get_global_runtime().clone())?;
                semantic_engine.index_batch(&items)?;
                tracing::info!("语义索引构建完成: {} 个实体已向量化", items.len());
            }
            Err(e) => {
                tracing::warn!("语义索引构建跳过（Embedding 引擎初始化失败，保留旧索引）: {}", e);
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
            // 比较前归一化路径分隔符（票 08）：node.file_path 可能是反斜杠
            // 平台路径，changed_files 来自 git diff/watch（正斜杠或相对路径）。
            // 库内 file_path 键已统一正斜杠，比较点必须同基准，否则增量
            // 索引删除/重索引在 Windows 上永不命中。
            let node_file_norm = incremental::norm_sep(node_file);
            if !changed_files.iter().any(|f| incremental::norm_sep(&f.to_string_lossy()) == node_file_norm) {
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
            if semantic_path.exists() && let Ok(embedder) = generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone()) {
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
    root: &project::ProjectRoot,
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
                anyhow::bail!("搜索索引不存在，请先运行 `repo-wiki generate` 或 `repo-wiki update` 构建索引");
            }
            let text_engine = search::text::TextEngine::open(&text_path)?;
            let results = text_engine.search(query, top_k)?;
            Ok(search::hybrid::text_results_to_hits(results))
        }
        config::schema::SearchEngineType::Semantic => {
            if !semantic_path.exists() {
                anyhow::bail!("语义索引不存在，请在配置中启用 embed 并运行 `repo-wiki generate`");
            }
            let embedder = generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone())?;
            let embedder = std::sync::Arc::new(embedder);
            let semantic_engine = search::semantic::SemanticEngine::open(&semantic_path, embedder, get_global_runtime().clone())?;
            let results = semantic_engine.search(query, top_k)?;
            Ok(search::hybrid::semantic_results_to_hits(results))
        }
        config::schema::SearchEngineType::Hybrid => {
            // 与 Text/Semantic 分支一致：text 索引是混合检索的必需底座
            //（RRF 至少一路有效），缺失时明确报错而非打开空库。
            if !text_path.exists() {
                anyhow::bail!("搜索索引不存在，请先运行 `repo-wiki generate` 或 `repo-wiki update` 构建索引");
            }
            let text_engine = search::text::TextEngine::open(&text_path)?;
            let semantic_engine: Option<Box<dyn search::semantic::SemanticSearch>> =
                if semantic_path.exists() && config.embed.enabled {
                    generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone())
                        .ok()
                        .and_then(|e| search::semantic::SemanticEngine::open(&semantic_path, Arc::new(e), get_global_runtime().clone()).ok())
                        .map(|e| Box::new(e) as Box<dyn search::semantic::SemanticSearch>)
                } else { None };
            let mut agent = search::agent::SearchAgent::new(text_engine, semantic_engine, config.search.rrf_k as f64);
            // 调用链补全：重建知识图谱以获得 Calls 边，构建调用索引注入 agent。
            // CLI 场景单次搜索的重建开销可接受（实测本项目约 1.2s）；
            // 失败时静默降级为无补全（索引缺失等，搜索主功能不受影响）。
            if let Ok(insights) = ingest::scan_and_parse_at(root, &config)
                && let Ok(graph) = analysis::build_graph(&insights)
            {
                let index = search::callgraph::CallGraph::new(&graph).build_call_index();
                agent = agent.with_call_index(index);
            }
            Ok(agent.search(query, top_k, true))
        }
    }
}

/// 执行 AST 精确符号查找（供 CLI `ast-search` 子命令调用）
///
/// 扫描配置范围内全部源文件，对每个文件用 tree-sitter 解析 AST，
/// 定位与 `symbol` 同名的顶层定义节点（函数/结构体/trait/类等）。
/// 与索引搜索（text/semantic/hybrid，模糊匹配）互补：AST 查找返回
/// **精确的定义位置**（文件+行号+签名），不依赖搜索索引。
///
/// `language` 为源语言（rust/python/go/...），传入 None 时由文件扩展名自动推断。
pub fn execute_ast_search(
    config_path: &Path,
    root: &project::ProjectRoot,
    symbol: &str,
    language: Option<&str>,
) -> anyhow::Result<Vec<search::hybrid::SearchHit>> {
    if symbol.trim().is_empty() {
        return Ok(Vec::new());
    }
    let config = config::load_config(config_path)?;
    let insights = ingest::scan_and_parse_at(root, &config)?;

    let mut hits = Vec::new();
    for insight in &insights {
        // 语言：显式指定优先；否则按文件扩展名推断（与 parser 注册一致）
        let lang = match language {
            Some(l) => l.to_string(),
            None => match insight.path.extension().and_then(|e| e.to_str()) {
                Some("rs") => "rust".to_string(),
                Some("py") => "python".to_string(),
                Some("js") => "javascript".to_string(),
                Some("ts") => "typescript".to_string(),
                Some("go") => "go".to_string(),
                Some("cs") => "csharp".to_string(),
                _ => continue,
            },
        };
        // 直接用 AstQuery 解析查找（不经过 SearchAgent，搜索上下文不依赖索引）
        let mut q = match search::ast::AstQuery::new(&lang) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let Ok(Some(m)) = q.find_definition(&insight.source, symbol) else {
            continue;
        };
        // 捕获节点文本作为签名（如整行函数定义）；定位到文件+行号
        let signature = m
            .captures
            .get("name")
            .cloned()
            .unwrap_or_else(|| symbol.to_string());
        // 模块路径从文件父目录派生（与 chunk_by_file 同规则：Normal 组件 "::" 连接）
        let module_path: Vec<String> = insight
            .path
            .parent()
            .map(|p| {
                p.components()
                    .filter(|c| matches!(c, std::path::Component::Normal(_)))
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        hits.push(search::hybrid::SearchHit {
            node: model::CodeNode {
                id: model::NodeId::new(0),
                kind: model::NodeKind::Function,
                name: symbol.to_string(),
                file_path: Some(insight.path.to_string_lossy().to_string()),
                line_range: Some((m.start_line, m.end_line)),
                doc_comment: None,
                signature: Some(signature),
                module_path,
            },
            score: 100.0,
            source: "ast".into(),
            callers: vec![],
            callees: vec![],
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 产物集合 diff 清理（票 10）：旧状态记录过、但本次渲染集合之外的
    /// 产物路径被删除（全语言目录），本次渲染集合内的路径（含受保护文档）
    /// 一律保留。
    #[test]
    fn test_cleanup_stale_outputs_removes_unrendered_across_languages() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // 旧状态记录两个产物：src.md（双语言）与 lib.md（双语言）
        let mut state = incremental::state::GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::new(),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
        };
        for lang in ["zh", "en"] {
            let stale = dir.join("wiki").join(lang).join("src.md");
            let keep = dir.join("wiki").join(lang).join("lib.md");
            std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
            std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
            std::fs::write(&stale, "旧页面").unwrap();
            std::fs::write(&keep, "保留页面").unwrap();
            state
                .doc_fingerprints
                .insert(stale.to_string_lossy().to_string(), "fp".into());
            state
                .doc_fingerprints
                .insert(keep.to_string_lossy().to_string(), "fp".into());
        }

        // 本次渲染集合只含 lib.md（src.md 对应模块已消失，不在渲染集）
        let rendered: Vec<std::path::PathBuf> = ["zh", "en"]
            .iter()
            .map(|lang| dir.join("wiki").join(lang).join("lib.md"))
            .collect();

        cleanup_stale_outputs(Some(&state), &rendered);

        for lang in ["zh", "en"] {
            assert!(
                !dir.join("wiki").join(lang).join("src.md").exists(),
                "未渲染的旧产物应被清理（{lang}）"
            );
            assert!(
                dir.join("wiki").join(lang).join("lib.md").exists(),
                "本次渲染集合内的产物应保留（{lang}）"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 受保护文档在渲染集合内（rendered_paths 含受保护路径），diff 后
    /// 不会被误删——人工编辑内容由保护语义而非清理语义保障。
    #[test]
    fn test_cleanup_stale_outputs_keeps_rendered_protected() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_stale_protected_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut state = incremental::state::GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::new(),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
        };
        // 受保护页面被人工编辑过（指纹不匹配）——doc_fingerprints 仍记录其路径
        let manual = dir.join("wiki").join("zh").join("manual.md");
        std::fs::create_dir_all(manual.parent().unwrap()).unwrap();
        std::fs::write(&manual, "人工编辑内容").unwrap();
        state
            .doc_fingerprints
            .insert(manual.to_string_lossy().to_string(), "旧指纹".into());
        state
            .doc_modules
            .insert(manual.to_string_lossy().to_string(), "manual".into());

        // 本次渲染集合包含该路径（受保护文档属于生成集）
        let rendered = vec![manual.clone()];
        cleanup_stale_outputs(Some(&state), &rendered);

        assert!(
            manual.exists(),
            "渲染集合内的人工编辑文档不应被清理"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无旧状态（首次生成）时清理为空操作
    #[test]
    fn test_cleanup_stale_outputs_noop_without_state() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_stale_noop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        cleanup_stale_outputs(None, &[]);
        let _ = std::fs::remove_dir_all(&dir);
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
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
        };
        state.doc_fingerprints.insert(
            doc_path.to_string_lossy().to_string(),
            "与磁盘内容不同的指纹".into(),
        );
        state.doc_modules.insert(
            doc_path.to_string_lossy().to_string(),
            "src".into(),
        );
        state.save(&state_dir).unwrap();

        // force=false：保护集包含检测出的人工修改（下次生成不覆盖）
        let (protected, _) = load_protection(&config, false).unwrap();
        assert!(
            protected.contains(&doc_path.to_string_lossy().to_string()),
            "force=false 应保护人工修改的文档"
        );

        // force=true：保护集清空（render_all 将覆盖所有文档）
        let (protected, _) = load_protection(&config, true).unwrap();
        assert!(protected.is_empty(), "force=true 应清空保护集");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 票 02：state.json 存在但损坏（非 JSON）时 load_protection 必须 fail-loud，
    /// 不得静默返回空保护集（空保护会让人工修改保护在后续 update 中失效）。
    /// 与 sync_from_git 对损坏状态的拒绝行为对偶（tests/test_git_sync.rs:109-121）。
    #[test]
    fn test_load_protection_corrupt_state_fails_loud() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_corrupt_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = crate::config::schema::WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();

        // 写入损坏的状态文件（半截 JSON）
        let state_dir = dir.join(".state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("generation_state.json"), "{ 半截").unwrap();

        let err = load_protection(&config, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("状态文件损坏"), "应明确报告损坏, 实际: {msg}");

        // force=true 不受影响（清空保护是显式操作，不读状态）
        assert!(load_protection(&config, true).unwrap().0.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 票 02：状态文件不存在（首次运行）是合法场景，返回空保护不报错
    #[test]
    fn test_load_protection_missing_state_is_ok() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_missing_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = crate::config::schema::WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();

        let (protected, state) = load_protection(&config, false).unwrap();
        assert!(protected.is_empty());
        assert!(state.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
