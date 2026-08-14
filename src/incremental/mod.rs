pub mod change;
pub mod diff;
pub mod impact;
pub mod state;
pub mod watch;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::ingest::parser::FileInsight;
use crate::model::KnowledgeGraph;
use crate::project::ProjectRoot;

use self::change::{EntityChangeSet, classify_entity_changes_at, no_entity_change_files};
use self::impact::{propagate_impact, propagate_impact_semantic};
use self::state::GenerationState;

/// watch（FileWatch）模式写入状态的 last_commit_hash 哨兵值：watch 无
/// git 基准可记，用固定哨兵占位。update 命令复读该状态时须显式识别
/// （见 run_file_watch_incremental 的 has_valid_base 判断），否则哨兵会
/// 被当作 git SHA 解析报 "unable to parse OID" 并回退全量（v47 实测）。
pub(crate) const FILE_WATCH_SENTINEL: &str = "file-watch";

/// 变更面过大回退全量的三阈值（v0.7.x 文档漂移/增量传播优化）
///
/// 增量重生成收益随变更面扩大趋零（重生成页数 ≈ 全量），且逐文件
/// 过滤/传播/实体分类的开销白付——变更面超阈值时直接回退全量生成，
/// 与既有损坏状态回退全量的"宁可多生成不丢数据"哲学一致。
/// 判据语义见 should_fallback_full。
const FULL_REGEN_MIN_FILES: usize = 200;
const FULL_REGEN_FILE_RATIO: f64 = 0.5;
const FULL_REGEN_EST_TOKENS: usize = 200_000;

/// no-op 快速跳过判定（v19 t06，OpenWiki git-head 模式）
///
/// 定时 CI / watch 常驻场景下，无变更时 update 仍会全量扫描 + 分析
/// （状态推进前每次跑全量），成本可观。本判定在扫描之前完成，全部
/// 满足才跳过：
/// 1. 状态文件可读且 last_commit_hash 存在（上次成功生成过）
/// 2. 当前 git HEAD == last_commit_hash（无新提交）
/// 3. 源码范围（全仓库）内工作树无未提交变更（git status 过滤——
///    产物目录的改动不算，否则本仓库自身产物会恒阻断跳过）
/// 4. 产物信号存在（wiki 目录）——产物被删时不得跳过（no-op 固有盲区）
///
/// 为什么不需要 interrupted 标记（与 OpenWiki 方案的差异，G3 批判审查）：
/// 中途失败（Ctrl-C/panic/LLM 故障）时生成状态不推进 last_commit_hash——
/// 失败前有新提交则条件 2 不满足；失败前只有未提交变更则条件 3 不满足；
/// 失败且无任何变更（纯外部故障）则产物与代码均未变化，跳过无损失。
/// head + statuses 双判据已完备覆盖"防半程状态被误判"，interrupted 冗余。
///
/// 保守边界：状态损坏/非 git 仓库/无 HEAD/status 读取失败一律返回 false
/// （不跳过，走正常路径——正常路径对同样情况有各自的回退语义）。
pub fn should_skip_noop(root: &ProjectRoot, config: &WikiConfig) -> anyhow::Result<bool> {
    // 1. 状态可读 + 有基线（生成完成的最后 commit）
    let state_dir = config.output_dir().join(".state");
    let state = match GenerationState::load(&state_dir) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let Some(last_hash) = state.last_commit_hash else {
        return Ok(false);
    };
    // 5. 失败补偿信号（v22 修复）：上次生成有模块失败待重试时不可跳过——
    // 无变更时的快速判定会把失败模块的补生成一并跳过，使失败永久残留
    if !state.failed_modules.is_empty() {
        return Ok(false);
    }
    // 4. 产物信号：wiki 目录存在（被删时跳过会让缺失产物保持缺失）
    if !config.output_dir().join("wiki").exists() {
        return Ok(false);
    }
    // 2. 当前 HEAD 与基线一致（git2 读失败/无 HEAD = 保守不跳过）
    let repo = match git2::Repository::open(root.path()) {
        Ok(r) => r,
        Err(_) => return Ok(false),
    };
    let head_hash = match repo.head() {
        Ok(head) => head.peel_to_commit().ok().map(|c| c.id().to_string()),
        Err(_) => return Ok(false),
    };
    if head_hash.as_deref() != Some(last_hash.as_str()) {
        return Ok(false);
    }
    // 3. 源码范围（全仓库）内工作树无未提交变更
    if status_has_source_changes(&repo, config, root) {
        return Ok(false);
    }
    Ok(true)
}

/// 工作树是否有源码未提交变更（相对仓库根判断）
///
/// v30+：扫描范围已硬编码为全量遍历，任何路径都算源码变更——
/// 例外只有工具自身产物：产物目录（output_dir，含 .code-repo-wiki/.state/
/// .search）与仓库根 AGENTS.md（generate 自动写入的代理导航模板，
/// 非源码、不参与生成）。产物若算作变更则每次生成后 no-op 判定恒为
/// false（快速跳过永久失效）。被忽略目录（.gitignore 已忽略产物）
/// 不受影响，此处显式排除是为了未忽略产物的仓库也不退化。
///
/// 路径比较统一走 norm_sep（正斜杠）：git2 status 路径恒用正斜杠，
/// Windows 上 Path::starts_with 的反斜杠比较会失配。
fn status_has_source_changes(
    repo: &git2::Repository,
    config: &WikiConfig,
    root: &ProjectRoot,
) -> bool {
    // 产物目录相对仓库根归一化（output_dir 可能是 root 化注入的绝对路径）
    let out_abs = config.output_dir().to_string_lossy().replace('\\', "/");
    let root_abs = root.path().to_string_lossy().replace('\\', "/");
    let out_norm = out_abs
        .strip_prefix(&root_abs)
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or(&out_abs);
    match repo.statuses(None) {
        Ok(statuses) => statuses.iter().any(|s| {
            let Some(path) = s.path() else { return false };
            let norm = norm_sep(path);
            let in_output = norm == out_norm || norm.starts_with(&format!("{out_norm}/"));
            let is_agents_md = norm == "AGENTS.md";
            !(in_output || is_agents_md)
        }),
        // status 读取失败：保守视为有变更（不跳过）——no-op 跳过只允许
        // 在证据齐全时发生，证据缺失时必须走正常路径
        Err(_) => true,
    }
}

/// 路径分隔符归一化（Windows 兼容）
///
/// git2 的 delta 路径恒用正斜杠（"src/net/tcp.rs"），而 scanner/insight
/// 的路径在 Windows 上是反斜杠（"src\net\tcp.rs"）——所有跨来源的
/// 路径比较（影响传播起点匹配、实体变化分类、增量生成过滤）必须先
/// 归一化，否则 Windows 上传播/分类/过滤全部失效（子串 contains 与
/// HashSet 精确匹配都按字节比较）。
pub(crate) fn norm_sep(p: &str) -> String {
    p.replace('\\', "/")
}

/// 变更面过大回退全量判定（增量收益趋零时避免白付逐文件过滤/传播开销）
///
/// 三判据取或：
/// 1. 变更文件数 < FULL_REGEN_MIN_FILES → 小变更直接不触发（false）；
/// 2. 变更文件数 / 总文件数 > FULL_REGEN_FILE_RATIO → 变更覆盖大半个仓库，
///    重生成页数已 ≈ 全量，增量没有任何节省；
/// 3. 变更文件估算 token 数（Σ source.len()/3）> FULL_REGEN_EST_TOKENS →
///    绝对量逼近全量成本（即使占比不高，单个大文件或海量变更也白付开销）。
///
/// 边界：total_files == 0 时跳过比例判据（除零防护，此时也不可能有
/// 大变更面）；删除文件不在 insights 中，自然不参与 token 估算。
pub fn should_fallback_full(
    changed: &[PathBuf],
    total_files: usize,
    insights: &[FileInsight],
) -> bool {
    if changed.len() < FULL_REGEN_MIN_FILES {
        return false;
    }
    if total_files > 0 && changed.len() as f64 / total_files as f64 > FULL_REGEN_FILE_RATIO {
        return true;
    }
    // 删除文件（不在 insights）被 find 过滤掉，不计入估算
    let est_tokens: usize = changed
        .iter()
        .filter_map(|p| {
            insights
                .iter()
                .find(|i| norm_sep(&i.path.to_string_lossy()) == norm_sep(&p.to_string_lossy()))
                .map(|i| i.source.len() / 3)
        })
        .sum();
    est_tokens > FULL_REGEN_EST_TOKENS
}

/// 增量更新结果
pub struct IncrementalResult {
    /// 实际发生变更的文件路径（用于 LLM 生成过滤）
    pub changed_files: Vec<std::path::PathBuf>,
    /// 受影响的模块名称列表（用于日志和下游分析）
    pub affected_modules: Vec<String>,
    /// 实体级变化分类（GitDiff 策略产出；FileWatch 策略为空——
    /// 下游据此跳过实体级摘要过滤，见 generate::run_generation_filtered）
    pub entity_changes: EntityChangeSet,
    /// 本次变更是否含已删除文件（GitDiff 策略 = diff 删除集非空；
    /// FileWatch 策略 = 变更集中存在磁盘上已不存在的路径）。
    ///
    /// 纯删除时被删文件无 Insight、传播起点为空，changed_insights 为空
    /// 会走快照回填分支——该分支只按"整模块文件全删"过滤，多文件模块
    /// 删一文件时页面回填旧内容残留被删实体；同时全局文档（架构/概览/
    /// index）回填旧版继续列出已删模块。此信号让下游（generate 回填分支
    /// 与 lib.rs 的 index 门控）识别删除场景并走重生成路径（v21 验证轮修复）。
    pub has_deleted_files: bool,
}

/// 在指定项目根下运行增量更新分析
///
/// root 注入链路：git 仓库定位与实体变化分类的仓库根由 root 显式给出
/// 全部以 root 为基准，不再依赖进程 cwd——测试可在临时目录构造
/// ProjectRoot 验证增量逻辑，watch 常驻进程的 cwd 漂移不再改变
/// git 仓库解析目标。
pub fn run_incremental_update_at(
    root: &ProjectRoot,
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    watch_paths: &[PathBuf],
) -> Result<IncrementalResult> {
    // v30：增量策略硬编码为 FileWatch（文件内容指纹比对，不依赖 git 提交）——
    // 外部 Agent 保存文件后直接 update 即可生效；指纹=内容 SHA256，git 回滚
    // 也能检出。增量开关同样硬编码恒启用，force 参数仍可强制全量。
    let state_dir = config.output_dir().join(".state");

    // v30：恒走 FileWatch 指纹路径（GitDiff 策略已随配置字段一并删除）
    let (changed_files, affected_modules, entity_changes, has_deleted_files) =
        run_file_watch_incremental(root, insights, graph, &state_dir, watch_paths)?;

    // v22 失败补偿重试：上次生成失败的模块并入本次变更集（存活文件）。
    // 失败隔离（record_failure）只跳过失败模块不中断整体，但失败模块若
    // 源码不再变更将永远无法补生成（增量以 git diff 触发，失败模块不在
    // diff 中）。重试语义：模块的存活文件视同本次变更，走正常生成路径；
    // 依赖方由传播机制自然覆盖。清空时机：重试成功或全量生成。
    //
    // 模块→文件映射从导出快照取（cards.related_files）：failed_modules
    // 记录的是 chunk 模块名（社区名，与卡片 module_name 同体系），而
    // graph 节点的 module_path 是文件路径体系——用 module_files 匹配
    // 社区名会永远落空（Unity 实测补偿未触发）。快照是唯一同时携带
    // 两套信息的持久化载体。
    let mut changed_files = changed_files;
    let mut affected_modules = affected_modules;
    if let Ok(state) = GenerationState::load(&state_dir)
        && !state.failed_modules.is_empty()
    {
        let snapshot_path = crate::output::export_snapshot_path(config.output_dir());
        let mut failed_files: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(content) = std::fs::read_to_string(&snapshot_path)
            && let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content)
        {
            for card in &snapshot.cards {
                if state.failed_modules.contains(&card.module_name) {
                    for rf in &card.related_files {
                        let p = std::path::PathBuf::from(rf);
                        if !failed_files.contains(&p) {
                            failed_files.push(p);
                        }
                    }
                }
            }
        } else {
            // 快照缺失/损坏：补偿无法定位模块文件，保守不并入（本次
            // update 走正常路径；失败模块等下次生成自然覆盖）
            tracing::warn!(
                "失败补偿重试：导出快照不可读（{}），跳过补偿并入",
                snapshot_path.display()
            );
        }
        let mut merged = changed_files.clone();
        // 存活判定相对仓库根（快照 related_files 是相对项目根的路径，
        // 直接 exists() 会相对进程 cwd 误判）
        for f in failed_files {
            if root.path().join(&f).exists() && !merged.contains(&f) {
                merged.push(f);
            }
        }
        if merged.len() > changed_files.len() {
            tracing::warn!(
                "检测到上次生成失败模块（{} 个），并入本次变更集补偿重试",
                state.failed_modules.len()
            );
            changed_files = merged;
        }
    }

    // 删除文件的「反向引用失效」（Phase A10 / I3 根因修复）：被删文件在
    // 图谱中无节点（impact.rs find_start_nodes 找不到即跳过），语义传播永远
    // 无法标记「页面文本引用了被删文件」的模块——引用页面残留坏引用（lint
    // bad-citation/source-missing/orphan）。从旧导出快照构建反向索引，凡
    // 引用了被删文件的模块并入受影响集，使其在增量 update 中被重生成；
    // 重生成时引用校验（validate_citations_against_entities）强制 LLM 剔除
    // 对已删文件的引用，页面自愈。只失效「确实引用了它」的页面（不无差别
    // 全量重生成）；快照缺失/损坏时告警跳过（保守不并入，宁可残留也不误伤）。
    let deleted_files: std::collections::HashSet<std::path::PathBuf> = changed_files
        .iter()
        .filter(|f| !root.path().join(f).exists())
        .cloned()
        .collect();
    if !deleted_files.is_empty() {
        let snapshot_path = crate::output::export_snapshot_path(config.output_dir());
        if let Ok(content) = std::fs::read_to_string(&snapshot_path)
            && let Ok(snapshot) = serde_json::from_str::<crate::output::ExportSnapshot>(&content)
        {
            let referencing = impact::reverse_reference_affected_modules(&deleted_files, &snapshot);
            for m in &referencing {
                if !affected_modules.contains(m) {
                    affected_modules.push(m.clone());
                }
            }
            if !referencing.is_empty() {
                affected_modules.sort();
                tracing::info!(
                    "删除文件的引用页反向失效: 模块 {:?} 引用了被删文件，并入受影响集重生成",
                    referencing
                );
            }
        } else {
            tracing::warn!(
                "删除引用页反向失效：导出快照不可读（{}），跳过（无法定位引用被删文件的页面）",
                snapshot_path.display()
            );
        }
    }

    Ok(IncrementalResult {
        changed_files,
        affected_modules,
        entity_changes,
        has_deleted_files,
    })
}

/// FileWatch 策略的增量更新
///
/// root 注入：git 仓库定位与实体变化分类的仓库根都由 root 显式给出
/// （私有函数，签名由公开入口 run_incremental_update_at 统一约束）。
fn run_file_watch_incremental(
    root: &ProjectRoot,
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    state_dir: &Path,
    watch_paths: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<String>, EntityChangeSet, bool)> {
    // 重新加载状态，比较文件指纹；状态损坏/缺失时全部文件视为变更
    // （回退全量），不能静默吞错——与 GitDiff 路径的 warn 处理一致
    let state = match GenerationState::load(state_dir) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("状态文件读取失败，按无基线处理（本次全量变更）: {}", e);
            None
        }
    };
    let mut changed_files: Vec<PathBuf> = Vec::new();

    for insight in insights {
        if let Ok(true) = state
            .as_ref()
            .map(|s| s.is_file_changed(root, &insight.path))
            .unwrap_or(Ok(true))
        {
            changed_files.push(insight.path.clone());
        }
    }

    // 并入外部 watch 事件路径（去重）：事件路径是变更的直接证据，不再只依赖指纹比对。
    // 指纹比对保留，用于兜底 watch 事件丢失的变更（防抖窗口冲突、事件丢失等），两者取并集。
    // 不存在的路径（删除事件）原样进入 changed_files，供下游 cleanup_deleted_outputs
    // 以 exists() 判断并清理旧输出——删除文件不在 insights 里，指纹比对永远捕获不到。
    for p in watch_paths {
        if !changed_files.contains(p) {
            changed_files.push(p.clone());
        }
    }

    // v30 补强：纯 update（无 watch 事件）也能检出文件删除——旧指纹表中
    // 本次 insights 已不存在的路径即删除事件（磁盘已删，下游按 exists()
    // 判断清理旧输出；与 watch 删除事件同一处理路径）。
    if let Some(state) = &state {
        for path_str in state.file_fingerprints.keys() {
            let p = PathBuf::from(path_str);
            if !insights.iter().any(|i| i.path == p) && !changed_files.contains(&p) {
                changed_files.push(p);
            }
        }
    }

    if changed_files.is_empty() {
        tracing::info!("无文件变更");
        return Ok((Vec::new(), Vec::new(), EntityChangeSet::default(), false));
    }

    // v0.7.x 变更面过大回退全量：增量收益随变更面扩大趋零（重生成页数
    // ≈全量），逐文件过滤/传播/分类开销白付。命中时 changed_files =
    // 全部现存文件，宁多生成不丢数据（与状态损坏回退全量同哲学）。
    // 删除路径保留在变更集中：删除清理（compensate_deleted_files）与
    // has_deleted 判定依赖 changed_files 含已删路径——全量覆盖现存文件
    // 后，删除信息仍须上报给下游，否则被删实体的页面残留无法清除。
    if should_fallback_full(&changed_files, insights.len(), insights) {
        tracing::warn!(
            "变更面过大（{} 文件 / 共 {} 文件），回退全量重生成",
            changed_files.len(),
            insights.len()
        );
        let mut full: Vec<PathBuf> = insights.iter().map(|i| i.path.clone()).collect();
        for p in &changed_files {
            if !full.contains(p) && !root.path().join(p).exists() {
                full.push(p.clone());
            }
        }
        changed_files = full;
    }

    // 实体级变化分类（修复：FileWatch 路径原为空集，接口级变化无法驱动
    // 语义传播与实体级摘要过滤，README 声称与实现差距）。FileWatch 无
    // Git diff，但 classify_entity_changes_at 需要旧内容做对比——用状态里
    // 记录的 last_commit_hash 作基准从 Git 树读旧内容；变更集中磁盘上
    // 仍存在的文件视为 modified（新增文件对比旧树为空 → 全部 Added），
    // 磁盘上已不存在的视为 deleted。非 Git 仓库或无上次 commit 时分类
    // 内部返回空集，回退保守的双向传播（与 GitDiff 路径失败回退一致）。
    let (entity_changes, classification_failed) = if let Some(state) = &state {
        // watch 模式写入的哨兵 last_commit_hash（"file-watch"，见下方
        // FILE_WATCH_SENTINEL）不是合法 SHA——update 命令复读该状态时若
        // 直接喂给 Oid 解析会报误导性 "unable to parse OID" 并回退全量
        // （v47 实测：每次 update 全量生成）。显式识别哨兵/缺失基准：
        // 无法实体级分类 → 直接回退保守双向传播（语义与分类失败一致，
        // 不产生误导报错）。
        let has_valid_base = state
            .last_commit_hash
            .as_deref()
            .is_some_and(|h| h != FILE_WATCH_SENTINEL);
        if !has_valid_base {
            (EntityChangeSet::default(), true)
        } else {
            // git tree 查询需要相对仓库根的路径（insight.path 可能是绝对路径，
            // 直接传入会让旧实体读取 miss 而误判为 Added→接口级双向传播误伤
            // 依赖方，见 test_incremental_git_e2e 场景 A 回归）
            let rel = |p: &PathBuf| p.strip_prefix(root.path()).unwrap_or(p).to_path_buf();
            // exists() 必须以 root 为基准（changed_files 是相对路径，裸判
            // exists 落在进程 cwd 上——测试/守护进程 cwd 是仓库根时，相对
            // 路径全部误判为"已删除"→实体被误标 Removed→接口级双向传播
            // 误伤依赖方，见 test_incremental_git_e2e 场景 A 回归）
            let exists_at_root = |p: &PathBuf| root.path().join(p).exists();
            let diff = crate::incremental::diff::GitDiffResult {
                modified: changed_files
                    .iter()
                    .filter(|p| exists_at_root(p))
                    .map(rel)
                    .collect(),
                deleted: changed_files
                    .iter()
                    .filter(|p| !exists_at_root(p))
                    .map(rel)
                    .collect(),
                from_commit: state.last_commit_hash.clone().unwrap_or_default(),
            };
            match classify_entity_changes_at(root, &diff, insights) {
                Ok(set) => (set, false),
                Err(e) => {
                    tracing::warn!("FileWatch 实体级变化分类失败，回退双向传播: {}", e);
                    (EntityChangeSet::default(), true)
                }
            }
        }
    } else {
        (EntityChangeSet::default(), true)
    };
    // v23 A1：与 GitDiff 路径同口径——分类成功时才剔除无实体变更文件
    // （纯空白/注释变化），分类失败/无状态（保守回退）保留全部起点。
    let changed_for_impact = if classification_failed {
        changed_files.clone()
    } else {
        let no_entity_change = no_entity_change_files(&changed_files, &entity_changes, root);
        changed_files
            .iter()
            .filter(|f| !no_entity_change.contains(*f))
            .cloned()
            .collect()
    };
    let affected_modules = if entity_changes.changes.is_empty() {
        propagate_impact(
            &changed_for_impact,
            graph,
            crate::config::schema::IMPACT_MAX_DEPTH,
        )
    } else {
        propagate_impact_semantic(
            &changed_for_impact,
            &entity_changes,
            graph,
            crate::config::schema::IMPACT_MAX_DEPTH,
        )
    };

    // 中途存盘已移除：分析阶段推进 file_fingerprints/last_commit_hash 会使
    // 生成崩溃后下次 update 指纹比对检不出变更（changed_files 空 → 静默
    // 跳过 → 产物永久失配）。磁盘状态停留上次成功状态，保护字段天然保留；
    // 生成成功后由 lib.rs:617 save_generation_state 统一推进。
    tracing::info!(
        "FileWatch 增量分析完成: {} 个模块受影响",
        affected_modules.len()
    );
    // FileWatch 的删除判定 = 变更集中存在磁盘上已不存在的路径（删除事件）
    // 与上方 exists_at_root 同基准（root.path() 前缀）：changed_files 为
    // 相对路径，裸判 exists 落在进程 cwd 上，cwd 漂移会误判删除
    let has_deleted = changed_files.iter().any(|p| !root.path().join(p).exists());
    Ok((changed_files, affected_modules, entity_changes, has_deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRoot;

    fn make_insight(path: &str) -> FileInsight {
        FileInsight {
            path: std::path::PathBuf::from(path),
            language: "rust".into(),
            entities: Vec::new(),
            imports: Vec::new(),
            doc_comments: Vec::new(),
            source: String::new(),
        }
    }

    /// 带指定 source 的 insight（token 估算测试需要可控的 source.len()）
    fn make_insight_with_source(path: &str, source: &str) -> FileInsight {
        FileInsight {
            path: std::path::PathBuf::from(path),
            language: "rust".into(),
            entities: Vec::new(),
            imports: Vec::new(),
            doc_comments: Vec::new(),
            source: source.into(),
        }
    }

    fn make_config() -> WikiConfig {
        WikiConfig {
            ..Default::default()
        }
    }

    // ==================== v0.7.x 变更面过大回退全量 ====================

    /// 变更文件数低于 MIN_FILES → 不触发回退
    #[test]
    fn test_fallback_full_below_min_files() {
        let insights: Vec<FileInsight> = (0..100)
            .map(|i| make_insight_with_source(&format!("src/{i}.rs"), "fn f() {}"))
            .collect();
        let changed: Vec<PathBuf> = insights.iter().take(50).map(|i| i.path.clone()).collect();
        assert!(
            !should_fallback_full(&changed, insights.len(), &insights),
            "50 文件变更（< 200）不得触发回退"
        );
    }

    /// 变更文件数 / 总文件数 超 RATIO → 触发回退
    #[test]
    fn test_fallback_full_ratio_exceeded() {
        let insights: Vec<FileInsight> = (0..500)
            .map(|i| make_insight_with_source(&format!("src/{i}.rs"), "fn f() {}"))
            .collect();
        let changed: Vec<PathBuf> = insights.iter().take(300).map(|i| i.path.clone()).collect();
        // 300 >= MIN_FILES 且 300/500 = 0.6 > 0.5
        assert!(
            should_fallback_full(&changed, insights.len(), &insights),
            "变更覆盖 60% 仓库应触发回退"
        );
    }

    /// 占比不超 RATIO，但变更文件估算 token 超阈值 → 触发回退
    #[test]
    fn test_fallback_full_token_threshold() {
        // 大 source：4000 字符/文件 → 4000/3 ≈ 1333 token/文件
        let source = "x".repeat(4000);
        let insights: Vec<FileInsight> = (0..500)
            .map(|i| make_insight_with_source(&format!("src/{i}.rs"), &source))
            .collect();
        let changed: Vec<PathBuf> = insights.iter().take(200).map(|i| i.path.clone()).collect();
        // 200/500 = 0.4 < 0.5（占比不触发）；200 × 1333 = 266k > 200k（token 触发）
        assert!(
            should_fallback_full(&changed, insights.len(), &insights),
            "估算 token 超阈值应触发回退"
        );
    }

    /// total_files == 0 除零防护：不 panic，返回 false（无文件可回退）
    #[test]
    fn test_fallback_full_zero_total_files() {
        let changed = vec![PathBuf::from("src/a.rs")];
        assert!(
            !should_fallback_full(&changed, 0, &[]),
            "total_files == 0 不得触发回退（比例判据跳过，估算为 0）"
        );
    }

    /// 非 Git 目录：GitDiff 增量回退全量（changed_files = 所有 insights 路径）
    /// 行数超限：回退全量
    /// A1：删除文件后增量更新，被删路径必须进入 changed_files，
    /// 下游删除清理（cleanup_deleted_outputs）才能命中并清除旧输出
    #[test]
    fn test_deleted_files_in_changed_set() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_deleted_changed_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        // 第一次提交：src/foo.rs 存在
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("foo.rs"), "fn foo() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");
        // 基线 = 第一 commit：diff 才能覆盖第二次提交的删除变更
        let first_hash = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        // 基线 = 第一 commit 时含 foo.rs 的状态（文件仍存在，指纹可计算；
        // v30 FileWatch 删除检测=旧指纹∖本次 insights，指纹表必须有 foo.rs）
        let state_dir = dir.join(".state");
        let baseline_insights = vec![make_insight("src/foo.rs")];
        let baseline = GenerationState::from_insights(
            &crate::project::ProjectRoot::new(dir.clone()),
            &baseline_insights,
            &first_hash,
        )
        .unwrap();
        baseline.save(&state_dir).unwrap();

        // 第二次提交：删除 src/foo.rs（commit_all 的 add_all 会检测磁盘删除
        // 并暂存删除变更；必须放在建状态之后——指纹计算需要文件仍在磁盘上）
        std::fs::remove_file(src.join("foo.rs")).unwrap();
        crate::test_git::commit_all(&dir, "delete");

        // insights 只含现存文件（被删文件不在其中）
        let insights: Vec<FileInsight> = Vec::new();
        let graph = KnowledgeGraph::default();
        let state_dir = dir.join(".state");

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &graph,
            &state_dir,
            &[],
        )
        .unwrap();
        assert!(
            changed.iter().any(|p| p == Path::new("src/foo.rs")),
            "被删文件路径应计入 changed_files（否则删除清理与索引清理永不触发）: {:?}",
            changed
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FileWatch 核心修复：watch 事件传入已删除路径（磁盘不存在）时，
    /// 删除路径必须进入 changed_files，下游 cleanup_deleted_outputs 才能
    /// 清理旧输出——删除文件不在 insights 中，指纹比对永远捕获不到
    #[test]
    fn test_file_watch_deleted_path_in_changed_files() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_watch_deleted_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        // 先保存状态：a.rs 内容未变，指纹比对不命中
        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let state =
            GenerationState::from_insights(&root, std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();

        // watch 事件传入已删除路径（磁盘上不存在）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &root,
            std::slice::from_ref(&insight),
            &graph,
            &state_dir,
            std::slice::from_ref(&deleted),
        )
        .unwrap();
        assert_eq!(
            changed,
            vec![deleted],
            "删除路径必须进入 changed_files（否则删除清理永不触发）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FileWatch 并集语义：指纹命中与外部 watch 路径取并集
    /// （指纹覆盖 watch 事件丢失的变更，watch 覆盖指纹捕获不到的删除）
    #[test]
    fn test_file_watch_union_fingerprint_and_watch_paths() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_watch_union_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        // 保存状态（a.rs 指纹 v1），随后修改内容 → 指纹比对命中
        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let state =
            GenerationState::from_insights(&root, std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();
        std::fs::write(src.join("a.rs"), "v2").unwrap();

        // watch 事件传入另一路径（磁盘上不存在，模拟删除）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &root,
            std::slice::from_ref(&insight),
            &graph,
            &state_dir,
            std::slice::from_ref(&deleted),
        )
        .unwrap();
        assert!(
            changed.contains(&insight.path),
            "指纹命中的文件应计入: {:?}",
            changed
        );
        assert!(
            changed.contains(&deleted),
            "watch 路径应并入（与指纹命中取并集）: {:?}",
            changed
        );
        assert_eq!(changed.len(), 2, "指纹与 watch 路径取并集，不应重复");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 票 03 回归锚（P0-5 配套）：分析阶段不写盘推进指纹/commit。
    /// 存盘块已删除，状态推进由 lib.rs:617 save_generation_state 统一负责；
    /// 保护字段由磁盘旧状态天然承载。
    #[test]
    fn test_file_watch_midway_save_preserves_protection() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_watch_protect_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");

        // 旧状态：带人工保护（模拟上次生成完成后的状态）
        let root = crate::project::ProjectRoot::new(dir.clone());
        let mut old =
            GenerationState::from_insights(&root, std::slice::from_ref(&insight), "old").unwrap();
        old.protected_docs = vec!["wiki/zh/manual.md".to_string()];
        old.doc_fingerprints =
            std::collections::HashMap::from([("wiki/zh/manual.md".to_string(), "fp".to_string())]);
        old.save(&state_dir).unwrap();

        // 触发 FileWatch 增量（内容变更 → 指纹比对命中；分析不写盘）
        std::fs::write(src.join("a.rs"), "v2").unwrap();
        let changed = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        run_file_watch_incremental(
            &root,
            std::slice::from_ref(&insight),
            &graph,
            &state_dir,
            std::slice::from_ref(&changed),
        )
        .unwrap();

        // 磁盘状态必须保留保护字段（中途失败后人工修改保护不失效）
        let saved = GenerationState::load(&state_dir).unwrap();
        assert_eq!(
            saved.protected_docs,
            vec!["wiki/zh/manual.md"],
            "中途存盘不得清空保护集"
        );
        assert_eq!(
            saved
                .doc_fingerprints
                .get("wiki/zh/manual.md")
                .map(String::as_str),
            Some("fp")
        );
        // 分析阶段不得推进指纹/commit：磁盘状态须与旧值一致
        // （P0-5：崩溃后下次 update 用旧指纹比对，才能正确检出本次变更）
        assert_eq!(
            saved.last_commit_hash, old.last_commit_hash,
            "分析不得推进 last_commit_hash"
        );
        assert_eq!(
            saved.file_fingerprints, old.file_fingerprints,
            "分析不得推进文件指纹（旧状态含 a.rs 的 v1 指纹）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1：全新仓库首次 update（无基线状态）不得静默短路——
    /// 空 diff 时应回退全量生成，避免首用产出空 wiki
    #[test]
    fn test_first_update_no_baseline_falls_back_full() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_first_update_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        // 单 commit 仓库（HEAD 无父）：无基线时 diff 为空
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();
        let state_dir = dir.join(".state");

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &graph,
            &state_dir,
            &[],
        )
        .unwrap();
        assert_eq!(
            changed,
            vec![PathBuf::from("a.rs")],
            "首次 update 无基线时应回退全量（changed_files = 全部源文件），不得静默跳过: {:?}",
            changed
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1 配套：有基线 + 无变更 → 正常跳过（不被回退全量误伤）
    #[test]
    fn test_with_baseline_no_change_skips() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_skip_nochange_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");
        let head_hash = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();
        let state_dir = dir.join(".state");

        // 基线 = 当前 HEAD：diff 为空 → 应跳过（changed_files 为空）
        let baseline = GenerationState::from_insights(
            &crate::project::ProjectRoot::new(dir.clone()),
            &insights,
            &head_hash,
        )
        .unwrap();
        baseline.save(&state_dir).unwrap();

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &graph,
            &state_dir,
            &[],
        )
        .unwrap();
        assert!(
            changed.is_empty(),
            "有基线且无变更时应跳过，不得回退全量: {:?}",
            changed
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    ///（返回全部文件集），而非日志声称全量却返回空变更集导致跳过
    /// A6：状态文件损坏（load 失败）时不 panic、按无基线回退全量，
    /// 且不静默（warn 由 tracing 输出，行为断言为回退全量）
    #[test]
    fn test_corrupt_state_falls_back_full() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_corrupt_state_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");

        // 写损坏的状态 JSON
        let state_dir = dir.join(".state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("generation_state.json"), "{ 这不是合法 JSON").unwrap();

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();

        let (changed, _affected, _entity_changes, _has_deleted) = run_file_watch_incremental(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &graph,
            &state_dir,
            &[],
        )
        .unwrap();
        assert_eq!(
            changed,
            vec![PathBuf::from("a.rs")],
            "状态损坏应按无基线回退全量（保守：宁可全量也不产出空产物）: {:?}",
            changed
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== v19 t06：no-op 快速跳过 ====================

    /// 构造带 config 的 git 仓库 + 首个 commit，返回 (目录, 首个 HEAD hash, config)。
    /// repo 用完即弃（借用生命周期的元组约束），需要 repo 的用例自行 reopen。
    /// 目录带原子计数后缀：cargo test 并行跑多个用例，共用目录会竞争 .git/config.lock。
    fn setup_noop_fixture() -> (PathBuf, String, WikiConfig) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_noop_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "fn a() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");
        let head = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        let mut config = make_config();
        config.output_dir = Some(dir.join("out"));
        (dir, head, config)
    }

    /// 保存生成状态（基线 = 指定 commit）+ 建产物信号目录
    fn save_baseline(dir: &Path, config: &WikiConfig, commit_hash: &str) {
        let insights = vec![make_insight("src/a.rs")];
        let state = GenerationState::from_insights(
            &ProjectRoot::new(dir.to_path_buf()),
            &insights,
            commit_hash,
        )
        .unwrap();
        let state_dir = config.output_dir().join(".state");
        state.save(&state_dir).unwrap();
        std::fs::create_dir_all(config.output_dir().join("wiki").join("zh")).unwrap();
    }

    /// head 相同 + 工作树干净 + 产物存在 → 跳过（true）
    #[test]
    fn test_noop_skip_when_head_matches_and_clean() {
        let (dir, head, config) = setup_noop_fixture();
        save_baseline(&dir, &config, &head);

        assert!(
            should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "head 相同 + 工作树干净 + 产物存在应跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// head 变化（新提交）→ 不跳过（false）
    #[test]
    fn test_noop_no_skip_when_head_changed() {
        let (dir, first, config) = setup_noop_fixture();
        // 基线 = 第一 commit，随后提交第二个 commit → head 变化
        save_baseline(&dir, &config, &first);

        std::fs::write(dir.join("src").join("a.rs"), "fn a() { println!(); }\n").unwrap();
        crate::test_git::commit_all(&dir, "second");

        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "有新提交不得跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// head 相同但工作树有源码未提交变更 → 不跳过；
    /// 产物目录（output_dir）内的变更 → 仍跳过（不算源码变更）
    #[test]
    fn test_noop_status_scope_filtering() {
        let (dir, head, config) = setup_noop_fixture();
        save_baseline(&dir, &config, &head);

        // 源码（src/）未提交变更 → 不跳过
        std::fs::write(dir.join("src").join("a.rs"), "fn a() { /* dirty */ }\n").unwrap();
        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "源码未提交变更不得跳过"
        );
        // 还原后产物目录内出现新文件 → 仍跳过（产物不算源码变更）
        std::fs::write(dir.join("src").join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(config.output_dir().join("out.txt"), "not code").unwrap();
        assert!(
            should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "产物目录变更不应阻断跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物目录缺失 → 不跳过（no-op 固有盲区：产物被删不得保持缺失）
    #[test]
    fn test_noop_no_skip_when_output_missing() {
        let (dir, head, config) = setup_noop_fixture();
        // 只存基线，不建产物目录
        let insights = vec![make_insight("src/a.rs")];
        let state =
            GenerationState::from_insights(&ProjectRoot::new(dir.clone()), &insights, &head)
                .unwrap();
        let state_dir = config.output_dir().join(".state");
        state.save(&state_dir).unwrap();

        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "产物目录缺失不得跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无基线（状态缺失/无 last_commit_hash）→ 不跳过（首次 update 走正常路径）
    #[test]
    fn test_noop_no_skip_without_baseline() {
        let (dir, _head, config) = setup_noop_fixture();
        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "无基线状态不得跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v22 失败补偿重试：状态含 failed_modules 且 git 无变更时，
    /// 失败模块的存活文件（从导出快照 cards.related_files 解析）并入
    /// changed_files（下次 update 补生成），no-op 快速判定同时放行。
    #[test]
    fn test_incremental_merges_failed_modules_from_state() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_failed_retry_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let root = ProjectRoot::new(dir.clone());
        std::fs::create_dir_all(dir.join("src").join("m20")).unwrap();

        // git 仓库：一个提交（a.rs/b.rs），此后无任何变更
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(dir.join("src").join("m20").join("a.rs"), "pub fn fa() {}\n").unwrap();
        std::fs::write(dir.join("src").join("m20").join("b.rs"), "pub fn fb() {}\n").unwrap();
        crate::test_git::commit_all(&dir, "init");
        let head = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        // 状态：基线 = 当前 HEAD，failed_modules = ["src::m20"]（社区名体系）
        let state_dir = dir.join(".state");
        let mut state = GenerationState::from_insights(&root, &[], &head).unwrap();
        state.failed_modules = vec!["src::m20".into()];
        state.save(&state_dir).unwrap();

        // 导出快照：src::m20 卡片携带 related_files（与卡片 module_name 同体系）
        let snapshot = crate::output::ExportSnapshot {
            version: 1,
            documents: vec![],
            cards: vec![crate::model::KnowledgeCard {
                module_name: "src::m20".into(),
                module_type: "module".into(),
                summary: String::new(),
                key_entities: vec![],
                dependencies: vec![],
                dependents: vec![],
                design_patterns: vec![],
                todo_notes: vec![],
                related_files: vec!["src/m20/a.rs".into(), "src/m20/b.rs".into()],
                coding_spec: None,
                tech_stack: vec![],
                architecture: None,
                design_rationale: None,
                pending_manual_edits: vec![],
                features: vec![],
            }],
            modules: vec![],
        };
        std::fs::write(
            dir.join(".state").join("export_snapshot.json"),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();

        // git 无变更：changed_files 应为空 → 补偿并入失败模块存活文件
        // （output.dir 指向 dir，与 save 状态目录一致）
        let mut config = make_config();
        config.output_dir = Some(dir.to_path_buf());
        let insights = vec![make_insight("src/m20/a.rs"), make_insight("src/m20/b.rs")];
        let graph = KnowledgeGraph::default();
        let result = run_incremental_update_at(&root, &insights, &graph, &config, &[]).unwrap();
        assert!(
            result
                .changed_files
                .contains(&PathBuf::from("src/m20/a.rs")),
            "失败模块的存活文件必须并入变更集（补生成）"
        );
        assert!(
            result
                .changed_files
                .contains(&PathBuf::from("src/m20/b.rs")),
            "同模块全部存活文件都并入"
        );

        // no-op 判据：failed_modules 非空 → 不跳过（补偿重试必须可达）
        assert!(
            !should_skip_noop(&root, &config).unwrap(),
            "存在失败模块时 no-op 快速判定不得跳过"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
