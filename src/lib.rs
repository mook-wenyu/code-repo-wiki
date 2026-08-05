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
pub mod doctor;

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
    /// 扫描范围内解析失败的文件数（非 UTF-8 / tree-sitter 解析错误；
    /// B5 失败可观测——此前失败文件仅在日志出现，统计无法反映）
    pub files_failed: usize,
    pub total_entities: usize,
    pub total_edges: usize,
    pub modules_detected: usize,
    pub generation_time_ms: u64,
    /// 本次生成失败被隔离的模块名（卡片或页面生成失败，v22 修复）：
    /// 写入生成状态供下次 update 补偿重试；也供调用方（doctor/报告）观测
    pub failed_modules: Vec<String>,
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
    config_path: Option<&Path>,
    output: Option<&Path>,
    root: &project::ProjectRoot,
) -> anyhow::Result<config::schema::WikiConfig> {
    // v25：None 走默认配置链（项目级 config.toml 字段级合并覆盖用户级
    // default-config.toml，见 config::load_default_config）；Some 为显式
    // --config 单文件原样加载
    let config = match config_path {
        Some(p) => config::load_config(p)?,
        None => config::load_default_config(root)?.1,
    };
    let mut config = if let Some(out) = output {
        let mut c = config;
        c.output.dir = out.to_string_lossy().into_owned();
        c
    } else {
        config
    };
    // root 统一（v17 F 组，t09 实测发现）：output.dir 是相对路径（默认
    // .repo-wiki）时必须解析到 root，否则 --root 场景（cwd ≠ root）产物
    // 写到进程 cwd 错位。此前 main.rs 在 status/lint/note 三分支各自 root
    // 化（重复且 generate/update 漏掉），收敛到本单一入口后所有下游
    // （generate/update/lint/status/note/doctor/dry-run）行为一致。
    let output_dir = Path::new(&config.output.dir);
    if output_dir.is_relative() {
        config.output.dir = root.path().join(output_dir).to_string_lossy().into_owned();
    }
    // wiki_plan.yaml 的 scope 覆盖相对项目根解析（不依赖进程 cwd）
    if let Some(plan) = crate::config::plan::resolve_plan_at(root, &config)?
        && let Some(scope) = plan.scope_override
    {
        config.scope = scope;
    }
    Ok(config)
}

/// 加载配置并统一解析 output.dir 相对路径到 root（main.rs 各命令入口用；
/// 与 run_pipeline 内部的 load_config_with_output 同源，保证 CLI 层与
/// pipeline 层对产物目录的解析一致）
pub fn load_config_rooted(
    config_path: Option<&Path>,
    root: &project::ProjectRoot,
) -> anyhow::Result<config::schema::WikiConfig> {
    load_config_with_output(config_path, None, root)
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
/// protected_docs 合并本次保护集写回；failed_modules 记录本次失败隔离的
/// 模块（v22：下次 update 并入变更集重试，防止失败模块永远无法补生成）
///
/// 8 个参数均为不同来源的独立输入（无共享结构可归并），与
/// generate_global_documents 同一例外模式，保留平铺参数。
#[allow(clippy::too_many_arguments)]
fn save_generation_state(
    root: &project::ProjectRoot,
    config: &config::schema::WikiConfig,
    insights: &[ingest::parser::FileInsight],
    documents: &[model::WikiDocument],
    cards: &[model::KnowledgeCard],
    protected: &std::collections::HashSet<String>,
    commit_hash: &str,
    failed_modules: &[String],
) {
    let output_dir = Path::new(&config.output.dir);
    let state_dir = output_dir.join(".state");
    // t02/P1-2：三处落盘失败全部告警（此前静默——状态写失败会导致下次 update
    // 无指纹基线，人工修改保护与反向同步**静默失效**，与模块头"不静默丢失保护"
    // 的目标矛盾；与 incremental/mod.rs 前置保存的 warn 处理对齐）。
    match incremental::state::GenerationState::from_insights(root, insights, commit_hash) {
        Ok(mut state) => {
            state.failed_modules = failed_modules.to_vec();
            let mut protected_docs: Vec<String> = protected.iter().cloned().collect();
            protected_docs.sort();
            state.protected_docs = protected_docs;
            match incremental::state::GenerationState::record_doc_fingerprints(
                documents,
                cards,
                output_dir,
                &output::wiki_languages(config),
            ) {
                Ok((fps, modules)) => {
                    // 全量记录指纹与模块归属（含保护集文档）：受保护文档本轮被跳过
                    // 写盘，磁盘上仍是人工版，记录的即人工版指纹——下次再被人为修改
                    // 时指纹比对仍能命中检测，反向同步可持续生效；卡片侧的记录注入
                    // 自带去重（contains 检查），同一修改不会重复同步。
                    state.doc_fingerprints = fps;
                    state.doc_modules = modules;
                }
                Err(e) => tracing::warn!(
                    "产物指纹记录失败（下次 update 人工修改检测可能失效）: {e}"
                ),
            }
            if let Err(e) = state.save(&state_dir) {
                tracing::warn!("生成状态保存失败（下次 update 无指纹基线，人工修改保护失效）: {e}");
            }
        }
        Err(e) => tracing::warn!("生成状态构造失败（本次状态未落盘）: {e}"),
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
    config_path: Option<&Path>,
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
    config_path: Option<&Path>,
    output: Option<&Path>,
    force: bool,
    root: &project::ProjectRoot,
    mode: &GenerationMode,
    on_progress: &dyn Fn(ProgressEvent),
) -> anyhow::Result<AnalysisResult> {
    let config = load_config_with_output(config_path, output, root)?;
    let _span = tracing::info_span!("pipeline", config = %config_path.map(|p| p.display().to_string()).unwrap_or_else(|| "默认链".into()));
    let _enter = _span.enter();
    let start = std::time::Instant::now();
    let mut is_incremental = matches!(mode, GenerationMode::Incremental { .. });
    // U06/D11：force 语义补全——force 时无论增量/全量都按全量重生成。
    // 旧实现 force 只清保护集，增量仍按 diff 过滤生成，未变更的文档
    // 不会被重生成，"force 覆盖所有文档"（本函数顶部注释）名不副实。
    if force && is_incremental {
        tracing::info!("--force 与增量模式同时使用：退化为全量重生成");
        is_incremental = false;
    }

    // 保护集：旧 state 的 protected_docs + 检测出的人工修改；force 时清空。
    // old_state 同时供人工修改反向同步组装（collect_manual_edits → 生成前注入）
    let (protected, old_state) = load_protection(&config, force)?;

    // v19 t06：no-op 快速跳过（OpenWiki git-head 模式）——增量模式且上次
    // 成功生成到同一 commit 且 scope 内工作树无未提交变更且产物存在时，
    // 在扫描之前直接跳过（定时 CI/watch 免费空转；判定细节与保守边界
    // 见 incremental::should_skip_noop 注释）。与下方"无代码变更短路"
    // 同一出口：人工修改反向同步照常执行。
    if is_incremental && incremental::should_skip_noop(root, &config)? {
        tracing::info!("无文件变更，跳过更新（no-op 快速判定）");
        if let Some(state) = &old_state {
            let synced = sync_manual_edits_to_cards(&config, state)?;
            if synced > 0 {
                tracing::info!("人工修改已反向同步到 {} 张卡片", synced);
            }
        }
        let stats = AnalysisStats {
            files_scanned: 0,
            generation_time_ms: start.elapsed().as_millis() as u64,
            ..Default::default()
        };
        return Ok(AnalysisResult {
            graph: model::KnowledgeGraph::default(),
            documents: Vec::new(),
            cards: Vec::new(),
            stats,
        });
    }

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
    let scan = if is_incremental {
        let cache_path = Path::new(&config.output.dir).join(".state").join("insights_cache.json");
        ingest::scan_and_parse_cached_at(root, &config, &Some(cache_path), &watch_set)?
    } else {
        ingest::scan_and_parse_at(root, &config)?
    };
    let file_insights = scan.insights;
    let files_failed = scan.files_failed;
    on_progress(ProgressEvent { stage: "scanning", percent: 10 });
    if file_insights.is_empty() {
        bail!("未找到任何源文件");
    }
    let mut stats = AnalysisStats {
        files_scanned: file_insights.len(),
        files_parsed: file_insights.iter().filter(|f| !f.entities.is_empty()).count(),
        files_failed,
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
    //
    // U04/D8 增量门控：受影响模块为空（纯实现级变更）时 index 内容（模块列表
    // + 描述）不会变化，从导出快照回填旧 index（零 LLM 调用），与架构/概览的
    // backfill 语义一致；快照不可用（首次增量/损坏）时回退正常生成。
    // v21 验证轮：含已删除文件时**不放行**回填——纯删除场景 index 必须
    // 重生成，否则模块列表继续列出已删模块（与架构/概览的 has_deleted_files
    // 例外同一语义）。
    let gated = if is_incremental
        && inc_result
            .as_ref()
            .is_some_and(|i| i.affected_modules.is_empty() && !i.has_deleted_files)
    {
        generate::backfill_global_docs(
            &config,
            &mut gen_output.documents,
            &[crate::model::DocumentKind::TableOfContents],
        )
    } else {
        false
    };
    if !gated {
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
    }

    // v17 t06：mock 模式告警——占位内容页脚标注（产物可辨识，防误读为
    // 真实文档）。mock 产物的页面内容是占位 JSON（MockProvider 固定返回），
    // 生成层追加页脚（render 层保持纯渲染不感知 provider 类型；合成页
    // api.md 的注入在 render_all 内，共用 MOCK_FOOTER_MARK 单一来源）。
    if matches!(
        config.llm.provider,
        crate::config::schema::LlmProviderType::Mock
    ) {
        tracing::warn!("使用 mock provider：产物为占位内容，非真实文档（仅供测试/CI 演示）");
        for doc in &mut gen_output.documents {
            // 幂等追加：纯删除场景（增量快照回填）的旧文档已含页脚，
            // 重复注入会产出双页脚——已以页脚结尾的跳过（v21 F 组实测）
            if !doc.content.ends_with(crate::output::MOCK_FOOTER_MARK) {
                doc.content.push_str(crate::output::MOCK_FOOTER_MARK);
            }
        }
    }

    // Phase 4: 输出（render_all 内部同步写导出快照；产物集合 diff 清理
    // 全量/增量统一：旧状态记录过但本次未生成的产物（含已删模块的
    // 旧页面/卡片）一律清理，module_{n} 档不再漏删）
    on_progress(ProgressEvent { stage: "wiki", percent: 90 });
    output::render_all(&gen_output.documents, &gen_output.cards, &graph, &config, &protected)?;
    // 保留集 = 当前扫描的全部模块（graph.modules 基于全部 insights 检测，
    // 含增量未受影响的模块）：增量只重新生成受影响模块，未受影响模块的
    // 旧页面须保留（v17 F 组，t09 实测修复——误删会制造断链）
    let preserved_modules: std::collections::HashSet<String> = graph
        .modules
        .iter()
        .map(|m| m.name.clone())
        .collect();
    cleanup_stale_outputs(
        old_state.as_ref(),
        &output::rendered_paths(&gen_output.documents, &gen_output.cards, &config),
        &preserved_modules,
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
        // A3（v14）：git 基线获取失败显式区分——非 git 仓库（info：预期
        // 场景，无基线则状态不推进、下次 update 回退全量）与 git 仓库内
        // 失败（warn：仓库损坏/无 HEAD/HEAD 无目标等）。此前 unwrap_or_default
        // 把两者混为一谈静默吞掉，git 命令失败时用户无从知晓状态为何不推进。
        let head_hash = match incremental::diff::get_head_commit_hash_at(root) {
            Ok(h) => h,
            Err(e) => {
                if e.downcast_ref::<git2::Error>()
                    .map(|g| g.code() == git2::ErrorCode::NotFound)
                    .unwrap_or(false)
                {
                    tracing::info!("非 git 仓库，无 git 基线（增量状态不推进）: {}", e);
                } else {
                    tracing::warn!("获取 git HEAD 失败（增量状态不推进）: {}", e);
                }
                String::new()
            }
        };
        save_generation_state(root, &config, &file_insights, &gen_output.documents, &gen_output.cards, &protected, &head_hash, &gen_output.generation_stats.failed_modules);
    }

    on_progress(ProgressEvent { stage: "done", percent: 100 });
    stats.generation_time_ms = start.elapsed().as_millis() as u64;
    // 展示用统计（失败模块真源在 generation_stats；save 调用已直接使用
    // generation_stats.failed_modules——顺序修复：此前在此处才赋值，晚于
    // Phase 6 的 save_generation_state，导致失败模块恒为空数组落盘，
    // v22 补偿机制对全量 generate 的失败静默失效（v23 C 组实测发现））
    stats.failed_modules = gen_output.generation_stats.failed_modules.clone();
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
    config_path: Option<&Path>,
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
    preserved_modules: &std::collections::HashSet<String>,
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
        // v17 F 组（t09 实测）：root 统一（output.dir 绝对化）后，旧状态
        // 键可能仍是相对路径（迁移前的生成记录）——相对键无法与绝对
        // rendered 集可靠比较（旧 cwd 已不可考），保守保留，避免把合成页
        // 等无模块归属的产物误删（实测：api/architecture/index/overview
        // 四页被误删）。一次全量生成后状态键全部更新为绝对，后续增量
        // 的清理语义恢复正常（收敛点明确，非兜底）。
        if Path::new(path).is_relative() {
            continue;
        }
        // v17 F 组（t09 实测修复）：增量模式下本次只重新生成受影响模块，
        // 未受影响模块的旧页面是**有效产物**（源码仍在），不能当过期
        // 清理——否则引用它的页面断链（实测：src_fs.md 被清理后 6 页
        // broken）。判据：该页面归属的模块仍在当前扫描结果中（preserved
        // 集合来自 graph.modules——基于全部 insights 的模块检测，未受
        // 影响模块也在内）→ 保留；模块已从扫描消失（源文件删除）→ 清理。
        if state
            .doc_modules
            .get(path)
            .is_some_and(|m| preserved_modules.contains(m))
        {
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
        // 卡片读取失败（含不存在/损坏/权限）显式告警并跳过该卡片——
        // 原实现 unwrap_or_default 会把"读不到"当作"空卡片"，随后追加
        // 人工修改节写盘，凭空重建被删除的卡片，且吞掉损坏错误。
        let mut content = match std::fs::read_to_string(&card_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("读取卡片失败，跳过人工修改反向同步 {}: {}", card_path.display(), e);
                continue;
            }
        };
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
            crate::fs::write_file_atomic(&card_path, &content)?;
            synced += 1;
        }
    }
    Ok(synced)
}

/// 启动文件监听模式
///
/// `root` 为注入的项目根：首次全量生成与监听根均以它为基准
/// （扫描根一致，watch 常驻进程的 cwd 漂移不影响监听范围）。
pub fn run_watch(config_path: Option<&Path>, root: &project::ProjectRoot) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => config::load_config(p)?,
        None => config::load_default_config(root)?.1,
    };
    tracing::info!("首次全量生成...");
    run_pipeline(config_path, None, false, root, &GenerationMode::Full)?;
    tracing::info!("全量生成完成，开始监听文件变化...");

    let config_path = config_path.map(|p| p.to_path_buf());
    // 监听根 = 注入的项目根（与 scan_and_parse_at 的扫描根一致）
    let watch_root = root.path().to_path_buf();
    let watch_root_for_loop = watch_root.clone();
    // v14 F 组（t06 拍板）：Ctrl-C 优雅退出——专用线程等待 SIGINT 后置
    // 停止标记；run_watch_loop 每 500ms 轮询标记，置位时等当前增量
    // 生成完成再退出（不会在状态落盘中途打断）。
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = stop_flag.clone();
        let rt = get_global_runtime();
        std::thread::spawn(move || {
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::info!("收到 Ctrl-C，等待当前增量更新完成后退出...");
            });
        });
    }
    incremental::watch::run_watch_loop(
        &watch_root_for_loop,
        &config,
        stop_flag,
        move |events| {
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
                if let Err(e) = run_pipeline(config_path.as_deref(), None, false, &root, &mode) {
                    tracing::error!("增量更新失败: {}", e);
                } else {
                    tracing::info!("增量更新完成");
                }
            }
        },
    )
}

// ==================== 搜索索引集成 ====================

/// 获取搜索索引目录的绝对路径
fn search_index_dir(config: &config::schema::WikiConfig) -> std::path::PathBuf {
    Path::new(&config.output.dir).join(config::schema::SEARCH_INDEX_DIR)
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

    // 收集所有需要索引的实体（U04/D2：与增量路径共用 collect_index_items，
    // 过滤规则单一来源）
    let items = collect_index_items(graph, &source_map);

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

    // 重新索引变更文件中的实体（全量 items 提取复用：collect_index_items
    // 是全量路径与增量路径的公共过滤骨架，避免两处过滤规则漂移）
    let source_map = build_source_map(file_insights);
    let items: Vec<(model::CodeNode, String)> = collect_index_items(graph, &source_map)
        .into_iter()
        .filter(|(node, _)| {
            // 只索引属于变更文件的实体
            let Some(node_file) = node.file_path.as_deref() else {
                return false;
            };
            // 比较前归一化路径分隔符（票 08）：node.file_path 可能是反斜杠
            // 平台路径，changed_files 来自 git diff/watch（正斜杠或相对路径）。
            // 库内 file_path 键已统一正斜杠，比较点必须同基准，否则增量
            // 索引删除/重索引在 Windows 上永不命中。
            let node_file_norm = incremental::norm_sep(node_file);
            changed_files
                .iter()
                .any(|f| incremental::norm_sep(&f.to_string_lossy()) == node_file_norm)
        })
        .collect();

    text_engine.index_batch(&items)?;

    // 增量更新语义索引（如已启用）
    if config.embed.enabled {
        let semantic_path = index_dir.join("semantic_index.db");
        // A1（v14）：入口失败显式告警——此前两处 `if let Ok(...)` 静默吞掉
        // EmbeddingEngine::new（key 缺失）与 SemanticEngine::open（DB 损坏）的
        // 失败，增量语义更新在用户不知情时整段跳过（与全量路径 :702/:707 的
        // warn 语义对齐：保留旧索引可观测，不静默）。
        if semantic_path.exists() {
            match generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone()) {
                Ok(embedder) => {
                    let embedder = std::sync::Arc::new(embedder);
                    match search::semantic::SemanticEngine::open(&semantic_path, embedder.clone(), get_global_runtime().clone()) {
                        Ok(mut semantic_engine) => {
                            // U04/D2：embedding 维度探测——换模型（维度变化）时，增量
                            // 删除 + 只回填变更集会把既有全部向量丢掉（vecdb 维度不匹配
                            // 重建 DROP 全表，仅 warn）。探测到维度变化则回退全量重建
                            // 语义索引（clear + 全量 items），与全量路径行为一致。
                            let probe_dim = if items.is_empty() {
                                None
                            } else {
                                match get_global_runtime().block_on(embedder.embed(&items[0].1)) {
                                    Ok(v) => Some(v.len()),
                                    Err(e) => {
                                        tracing::warn!("embedding 维度探测失败，跳过维度重建检查: {}", e);
                                        None
                                    }
                                }
                            };
                            // 维度探测失败（数据库损坏/权限）显式告警并跳过重建检查，
                            // 不静默当作"维度未变"——保持行为的同时错误可见
                            let dim_changed = match semantic_engine.table_dimension() {
                                Ok(existing_dim) => probe_dim
                                    .zip(existing_dim)
                                    .is_some_and(|(new_dim, existing)| new_dim != existing),
                                Err(e) => {
                                    tracing::warn!("读取语义索引维度失败，跳过维度重建检查: {}", e);
                                    false
                                }
                            };
                            if dim_changed {
                                tracing::warn!(
                                    "embedding 维度变化，回退全量重建语义索引（增量删除+回填会丢全部既有向量）"
                                );
                                let all_items = collect_index_items(graph, &source_map);
                                semantic_engine.clear()?;
                                semantic_engine.index_batch(&all_items)?;
                            } else {
                                // t01/P1-1：删除与回填错误显式传播（与同函数 text 路径一致）。
                                // 此前 `let _` 吞错：文本索引已更新而向量库停留旧态（新旧混存），
                                // 搜索返回陈旧/错位结果且无任何日志；语义索引是搜索功能的一部分，
                                // 静默失败不可接受。函数级隔离哲学不变——调用方（lib.rs Phase 5）
                                // 仍以 warn 包装，不中断主流程。
                                for file in changed_files {
                                    semantic_engine.remove_by_file(&file.to_string_lossy())?;
                                }
                                semantic_engine.index_batch(&items)?;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("语义索引打开失败，增量语义更新跳过（保留旧索引）: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Embedding 引擎初始化失败，增量语义更新跳过（保留旧索引）: {}", e);
                }
            }
        }
    }

    tracing::info!("搜索索引增量更新: 删除 {} 条, 新增 {} 条", total_removed, items.len());
    Ok(())
}

/// 收集全部可索引实体（项目/模块/文件级节点跳过），全量与增量路径共用
///
/// U04/D2 提取：增量路径的"变更文件过滤"是 collect 之后的选择，
/// 维度变化回退全量重建直接复用本函数，保证过滤规则单一来源。
fn collect_index_items(
    graph: &model::KnowledgeGraph,
    source_map: &std::collections::HashMap<String, String>,
) -> Vec<(model::CodeNode, String)> {
    graph
        .graph
        .node_indices()
        .filter_map(|idx| {
            let node = graph.graph.node_weight(idx)?;
            // 跳过项目/模块/文件级别的节点，只索引具体实体
            if matches!(
                node.kind,
                model::NodeKind::Project | model::NodeKind::Module | model::NodeKind::File
            ) {
                return None;
            }
            let source = extract_entity_source(node, source_map);
            Some((node.clone(), source))
        })
        .collect()
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
    config_path: Option<&Path>,
    root: &project::ProjectRoot,
    query: &str,
    top_k: usize,
    engine_type: &config::schema::SearchEngineType,
) -> anyhow::Result<Vec<search::hybrid::SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    // v25：None 走默认配置链（项目级字段级合并覆盖用户级）
    let config = match config_path {
        Some(p) => config::load_config(p)?,
        None => config::load_default_config(root)?.1,
    };
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
            // U04/P3：与 hybrid 分支一致——embed 未启用时显式报错而非
            // 继续打开索引（旧行为只查索引存在性，配置关闭 embed 时
            // 语义搜索照常运行，与配置意图矛盾且无任何提示）。
            if !config.embed.enabled {
                anyhow::bail!("语义搜索未启用（配置 embed.enabled = false），请在配置中启用 embed 后重新运行 `repo-wiki generate`");
            }
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
            // hybrid 语义一路：语义引擎构建失败（embedding 配置缺失/key
            // 无效/数据库损坏）显式告警并降级为纯 text——搜索结果少一路
            // 召回，但错误可见而非静默（v5 审计：全 .ok() 链把失败全吞掉，
            // 用户配置了 embed 却永远收不到语义结果且无任何提示）
            let semantic_engine: Option<Box<dyn search::semantic::SemanticSearch>> =
                if semantic_path.exists() && config.embed.enabled {
                    match generate::embed::EmbeddingEngine::new(&config.embed, get_global_runtime().handle().clone()) {
                        Ok(e) => match search::semantic::SemanticEngine::open(
                            &semantic_path,
                            Arc::new(e),
                            get_global_runtime().clone(),
                        ) {
                            Ok(engine) => Some(Box::new(engine) as Box<dyn search::semantic::SemanticSearch>),
                            Err(e) => {
                                tracing::warn!("语义索引打开失败，hybrid 降级为纯 text: {}", e);
                                None
                            }
                        },
                        Err(e) => {
                            tracing::warn!("embedding 引擎初始化失败，hybrid 降级为纯 text: {}", e);
                            None
                        }
                    }
                } else { None };
            let mut agent = search::agent::SearchAgent::new(text_engine, semantic_engine, config::schema::SEARCH_RRF_K);
            // 调用链补全：重建知识图谱以获得 Calls 边，构建调用索引注入 agent。
            // CLI 场景单次搜索的重建开销可接受（实测本项目约 1.2s）；
            // 失败时静默降级为无补全（索引缺失等，搜索主功能不受影响）。
            if let Ok(scan) = ingest::scan_and_parse_at(root, &config)
                && let Ok(graph) = analysis::build_graph(&scan.insights)
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
    config_path: Option<&Path>,
    root: &project::ProjectRoot,
    symbol: &str,
    language: Option<&str>,
) -> anyhow::Result<Vec<search::hybrid::SearchHit>> {
    if symbol.trim().is_empty() {
        return Ok(Vec::new());
    }
    let config = match config_path {
        Some(p) => config::load_config(p)?,
        None => config::load_default_config(root)?.1,
    };
    let insights = ingest::scan_and_parse_at(root, &config)?.insights;

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
                signature: Some(signature), visibility: None,
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
            tool_version: None,
            failed_modules: vec![],
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

        // preserved 为空：src.md 的模块 src::foo 不在保留集 → 按原语义清理
        cleanup_stale_outputs(Some(&state), &rendered, &std::collections::HashSet::new());

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
            tool_version: None,
            failed_modules: vec![],
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
        cleanup_stale_outputs(Some(&state), &rendered, &std::collections::HashSet::new());

        assert!(
            manual.exists(),
            "渲染集合内的人工编辑文档不应被清理"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v17 F 组（t09 实测修复）：增量模式下未受影响模块的旧页面必须保留
    /// ——模块仍在当前扫描（preserved 集合）中，即使本次未重新生成，
    /// 清理也须跳过（误删会制造断链）
    #[test]
    fn test_cleanup_stale_outputs_preserves_modules_still_in_scan() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_stale_preserve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut state = incremental::state::GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::new(),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
            tool_version: None,
            failed_modules: vec![],
        };
        // 旧状态：src::fs 模块的页面（模拟增量前生成的产物）
        let fs_page = dir.join("wiki").join("zh").join("src_fs.md");
        std::fs::create_dir_all(fs_page.parent().unwrap()).unwrap();
        std::fs::write(&fs_page, "旧内容").unwrap();
        state
            .doc_fingerprints
            .insert(fs_page.to_string_lossy().to_string(), "fp".into());
        state
            .doc_modules
            .insert(fs_page.to_string_lossy().to_string(), "src::fs".into());
        // 旧状态：src::deleted 模块的页面（模拟源文件已删除的模块）
        let gone_page = dir.join("wiki").join("zh").join("src_deleted.md");
        std::fs::write(&gone_page, "旧内容").unwrap();
        state
            .doc_fingerprints
            .insert(gone_page.to_string_lossy().to_string(), "fp".into());
        state
            .doc_modules
            .insert(gone_page.to_string_lossy().to_string(), "src::deleted".into());

        // 本次渲染集不含任何上述页面（增量只生成其他模块）；
        // 保留集含 src::fs（模块仍在扫描）但不含 src::deleted（已删除）
        let preserved: std::collections::HashSet<String> =
            ["src::fs".to_string()].into_iter().collect();
        cleanup_stale_outputs(Some(&state), &[], &preserved);

        assert!(fs_page.exists(), "仍在扫描的模块页面应保留");
        assert!(!gone_page.exists(), "已删除模块的页面应清理");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无旧状态（首次生成）时清理为空操作
    #[test]
    fn test_cleanup_stale_outputs_noop_without_state() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_stale_noop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        cleanup_stale_outputs(None, &[], &std::collections::HashSet::new());
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
            tool_version: None,
            failed_modules: vec![],
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
