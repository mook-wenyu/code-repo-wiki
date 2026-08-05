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

use self::change::{classify_entity_changes_at, EntityChangeSet};
use self::diff::analyze_git_diff;
use self::impact::{propagate_impact, propagate_impact_semantic};
use self::state::GenerationState;
use crate::config::schema::IncrementalStrategy;

/// diff 行数超过该上限时回退全量生成（防止超大变更集导致 LLM 成本失控）
const MAX_DIFF_LINES: usize = 10_000;

/// no-op 快速跳过判定（v19 t06，OpenWiki git-head 模式）
///
/// 定时 CI / watch 常驻场景下，无变更时 update 仍会全量扫描 + 分析
/// （状态推进前每次跑全量），成本可观。本判定在扫描之前完成，全部
/// 满足才跳过：
/// 1. 状态文件可读且 last_commit_hash 存在（上次成功生成过）
/// 2. 当前 git HEAD == last_commit_hash（无新提交）
/// 3. scope.include 覆盖范围内工作树无未提交变更（git status 过滤——
///    产物目录/其他目录的改动不算，否则本仓库自身产物会恒阻断跳过）
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
    let state_dir = Path::new(&config.output.dir).join(".state");
    let state = match GenerationState::load(&state_dir) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let Some(last_hash) = state.last_commit_hash else {
        return Ok(false);
    };
    // 4. 产物信号：wiki 目录存在（被删时跳过会让缺失产物保持缺失）
    if !Path::new(&config.output.dir).join("wiki").exists() {
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
    // 3. scope 范围内工作树无未提交变更
    if status_in_scope(&repo, config) {
        return Ok(false);
    }
    Ok(true)
}

/// 工作树未提交变更是否落在 scope.include 覆盖范围（相对仓库根判断）
///
/// 路径比较统一走 norm_sep（正斜杠）：git2 status 路径恒用正斜杠，
/// include glob 语义也是正斜杠分隔；Windows 上 Path::starts_with 的
/// 反斜杠比较会失配。
fn status_in_scope(repo: &git2::Repository, config: &WikiConfig) -> bool {
    let patterns: Vec<glob::Pattern> = config
        .scope
        .include
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    // include 的字面目录前缀（"src/**" → "src"；glob 无法匹配目录内文件）
    let roots: Vec<String> = config
        .scope
        .include
        .iter()
        .map(|p| {
            let dir = p.split('*').next().unwrap_or_default().trim_end_matches('/');
            norm_sep(if dir.is_empty() { "." } else { dir })
        })
        .collect();
    match repo.statuses(None) {
        Ok(statuses) => statuses.iter().any(|s| {
            let Some(path) = s.path() else { return false };
            let norm = norm_sep(path);
            patterns.iter().any(|p| p.matches(&norm))
                || roots.iter().any(|r| norm == *r || norm.starts_with(&format!("{r}/")))
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
/// root 注入链路：git 仓库定位（analyze_git_diff / classify_entity_changes_at）
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
    if !config.incremental.enabled {
        tracing::info!("增量更新已禁用，将执行全量生成");
        let (files, modules) = fallback_to_full(insights);
        return Ok(IncrementalResult { changed_files: files, affected_modules: modules, entity_changes: EntityChangeSet::default(), has_deleted_files: false });
    }

    let state_dir = Path::new(&config.output.dir).join(".state");

    // 按策略分发：GitDiff 或 FileWatch
    let (changed_files, affected_modules, entity_changes, has_deleted_files) = match config.incremental.strategy {
        IncrementalStrategy::GitDiff => {
            run_git_diff_incremental(root, insights, graph, config, &state_dir)?
        }
        IncrementalStrategy::FileWatch => {
            run_file_watch_incremental(root, insights, graph, config, &state_dir, watch_paths)?
        }
    };
    Ok(IncrementalResult { changed_files, affected_modules, entity_changes, has_deleted_files })
}

/// 回退全量生成：changed_files 为所有源文件，affected_modules 为空
/// （affected_modules 只用于日志与生成过滤的模块维度，全量回退按文件维度处理）
fn fallback_to_full(insights: &[FileInsight]) -> (Vec<std::path::PathBuf>, Vec<String>) {
    let all: Vec<std::path::PathBuf> = insights.iter().map(|i| i.path.clone()).collect();
    (all, Vec::new())
}

/// Git diff 策略的增量更新
///
/// root 注入：git 仓库定位与实体变化分类的仓库根都由 root 显式给出
/// （私有函数，签名由公开入口 run_incremental_update_at 统一约束）。
fn run_git_diff_incremental(
    root: &ProjectRoot,
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    state_dir: &Path,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>, EntityChangeSet, bool)> {
    // 1. 分析 Git diff
    // 基线 = 上次成功生成时落盘的 last_commit_hash。三种"无基线"场景：
    // ①全新仓库首次 update（状态文件不存在）；②状态文件损坏/不可读；
    // ③状态存在但 hash 为空（异常状态）。无基线时 diff 语义不可靠
    // （analyze_git_diff 只能取 HEAD^..HEAD，会漏掉更早的历史），
    // 且空 diff 短路会让首次 update 产出空 wiki——统一回退全量生成
    // （与"非 Git 仓库回退全量"同一语义，避免静默产出空产物）。
    let loaded_state = match GenerationState::load(state_dir) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("状态文件读取失败，按无基线处理（本次回退全量）: {}", e);
            None
        }
    };
    let has_baseline = loaded_state
        .as_ref()
        .and_then(|s| s.last_commit_hash.as_ref())
        .is_some();
    if !has_baseline {
        tracing::info!("无增量基线（首次更新或状态缺失），回退全量生成");
        let (files, modules) = fallback_to_full(insights);
        return Ok((files, modules, EntityChangeSet::default(), false));
    }
    let last_commit_hash = loaded_state
        .as_ref()
        .and_then(|s| s.last_commit_hash.clone());
    let diff_result = match analyze_git_diff(root.path(), last_commit_hash.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            // 非 Git 仓库：无法做增量，回退全量生成
            tracing::warn!("Git diff 分析失败，回退全量生成: {}", e);
            let (files, modules) = fallback_to_full(insights);
            return Ok((files, modules, EntityChangeSet::default(), false));
        }
    };

    if diff_result.added.is_empty() && diff_result.modified.is_empty() && diff_result.deleted.is_empty() {
        tracing::info!("无文件变更，跳过更新");
        return Ok((Vec::new(), Vec::new(), EntityChangeSet::default(), false));
    }

    if diff_result.added_lines + diff_result.deleted_lines > MAX_DIFF_LINES {
        tracing::warn!(
            "diff 行数超过上限 {}（新增 {} 行, 删除 {} 行），回退全量生成",
            MAX_DIFF_LINES,
            diff_result.added_lines,
            diff_result.deleted_lines
        );
        let (files, modules) = fallback_to_full(insights);
        return Ok((files, modules, EntityChangeSet::default(), false));
    }

    tracing::info!(
        "Git diff: {} 个新增, {} 个修改, {} 个删除, {} 个重命名",
        diff_result.added.len(),
        diff_result.modified.len(),
        diff_result.deleted.len(),
        diff_result.renamed.len()
    );

    // 2. 收集变更文件路径
    // 除 added/modified 外，deleted 与 renamed 的旧路径也必须计入：
    // 这些文件已不在磁盘上，下游（删除清理、搜索索引增量更新）依赖
    // 变更集里的"不存在文件"来清除旧输出，否则被删文件的 wiki 页/卡片/索引残留。
    // renamed 的新路径同样计入（git2 的 Renamed delta 不产生 Added delta），
    // 保证新文件被重新生成。
    let mut all_changed: Vec<std::path::PathBuf> = diff_result
        .added
        .iter()
        .chain(&diff_result.modified)
        .cloned()
        .collect();
    for deleted in &diff_result.deleted {
        if !all_changed.contains(deleted) {
            all_changed.push(deleted.clone());
        }
    }
    for (old, new) in &diff_result.renamed {
        if !all_changed.contains(old) {
            all_changed.push(old.clone());
        }
        if !all_changed.contains(new) {
            all_changed.push(new.clone());
        }
    }

    // 3. 实体级变化分类（演进计划 T2.2）：区分接口级/实现级变化，
    // 语义传播只让接口级变化（新增/删除/签名变更）影响依赖方，
    // 实现级变化（仅函数体修改）只重生成本模块——避免一次小改动
    // 触发全仓库级联重生成。分类失败时回退保守的双向传播。
    let entity_changes = match classify_entity_changes_at(root, &diff_result, insights) {
        Ok(set) => set,
        Err(e) => {
            tracing::warn!("实体级变化分类失败，回退双向传播: {}", e);
            EntityChangeSet::default()
        }
    };
    let affected_modules = if entity_changes.changes.is_empty() {
        propagate_impact(&all_changed, graph, config.incremental.max_depth)
    } else {
        propagate_impact_semantic(&all_changed, &entity_changes, graph, config.incremental.max_depth)
    };

    // 4. 保存新的状态（U03/D3 两段式：第一段只合并保护字段，不推进代码侧状态）
    // from_insights 的新状态保护字段为空，LLM 生成（Phase 3）失败时若已落盘
    // 会丢失人工修改保护——此处合并旧状态保护字段后再存，中途失败不丢保护。
    // 同时**不推进 last_commit_hash/file_fingerprints**（保持旧值）：若生成失败
    // （lib.rs Phase 3 `?` 传播），下次 update 的 git diff 仍以旧 commit 为基准，
    // 能再次看到本次变更——避免"失败变更被状态吞噬"（D3：原实现在此落盘
    // 新 commit hash，失败后重试 update 得到空 diff 短路，变更永不重生成）。
    // 生成成功后的最终保存（lib.rs Phase 6 save_generation_state）才推进代码侧状态。
    if let Ok(mut new_state) = GenerationState::from_insights(root, insights, &diff_result.to_commit) {
        // 复用前面已加载的状态（loaded_state 在此处必为 Some——无基线已提前回退），
        // 不重复读盘，也不存在"二次 load 失败静默跳过保护合并"的路径
        if let Some(old_state) = &loaded_state {
            new_state.preserve_protection(old_state);
            new_state.last_commit_hash = old_state.last_commit_hash.clone();
            new_state.file_fingerprints = old_state.file_fingerprints.clone();
        }
        if let Err(e) = new_state.save(state_dir) {
            tracing::warn!("保存生成状态失败: {}", e);
        }
    } else {
        // v16 B 组：from_insights 失败（指纹计算 IO 错误等）不再静默——
        // 中途存盘跳过意味着本次变更的代码侧状态不推进，下次 update 会
        // 以旧 commit 为基准重看本次 diff（行为与失败保存一致，但失败
        // 必须可观测，否则用户看到"增量完成"却不知状态没更新）
        tracing::warn!("构造增量状态失败，中途存盘跳过（本次变更将在下次 update 重看）");
    }

    tracing::info!("增量更新分析完成: {} 个模块受影响", affected_modules.len());
    // 删除集非空即含已删除文件（被删文件不在 insights，changed_insights
    // 过滤后可能为空 → 下游快照回填分支需要此信号改走重生成路径）
    Ok((all_changed, affected_modules, entity_changes, !diff_result.deleted.is_empty()))
}

/// FileWatch 策略的增量更新（变更文件来自外部事件 + 指纹比对）
fn run_file_watch_incremental(
    root: &ProjectRoot,
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
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
        if let Ok(true) = state.as_ref().map(|s| s.is_file_changed(root, &insight.path)).unwrap_or(Ok(true)) {
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

    if changed_files.is_empty() {
        tracing::info!("无文件变更");
        return Ok((Vec::new(), Vec::new(), EntityChangeSet::default(), false));
    }

    // 实体级变化分类（修复：FileWatch 路径原为空集，接口级变化无法驱动
    // 语义传播与实体级摘要过滤，README 声称与实现差距）。FileWatch 无
    // Git diff，但 classify_entity_changes_at 需要旧内容做对比——用状态里
    // 记录的 last_commit_hash 作基准从 Git 树读旧内容；变更集中磁盘上
    // 仍存在的文件视为 modified（新增文件对比旧树为空 → 全部 Added），
    // 磁盘上已不存在的视为 deleted。非 Git 仓库或无上次 commit 时分类
    // 内部返回空集，回退保守的双向传播（与 GitDiff 路径失败回退一致）。
    let entity_changes = if let Some(state) = &state {
        let diff = crate::incremental::diff::GitDiffResult {
            modified: changed_files
                .iter()
                .filter(|p| p.exists())
                .cloned()
                .collect(),
            deleted: changed_files
                .iter()
                .filter(|p| !p.exists())
                .cloned()
                .collect(),
            from_commit: state.last_commit_hash.clone().unwrap_or_default(),
            ..Default::default()
        };
        match classify_entity_changes_at(root, &diff, insights) {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!("FileWatch 实体级变化分类失败，回退双向传播: {}", e);
                EntityChangeSet::default()
            }
        }
    } else {
        EntityChangeSet::default()
    };
    let affected_modules = if entity_changes.changes.is_empty() {
        propagate_impact(&changed_files, graph, config.incremental.max_depth)
    } else {
        propagate_impact_semantic(&changed_files, &entity_changes, graph, config.incremental.max_depth)
    };

    // 保存新状态
    // 同上（票 03）：FileWatch 中途存盘同样合并旧状态保护字段；
    // 复用前面已加载的 state（Option），不存在二次 load 静默失败路径
    if let Ok(mut new_state) = GenerationState::from_insights(root, insights, "file-watch") {
        if let Some(old_state) = &state {
            new_state.preserve_protection(old_state);
        }
        if let Err(e) = new_state.save(state_dir) {
            tracing::warn!("保存生成状态失败: {}", e);
        }
    }

    tracing::info!("FileWatch 增量分析完成: {} 个模块受影响", affected_modules.len());
    // FileWatch 的删除判定 = 变更集中存在磁盘上已不存在的路径（删除事件）
    let has_deleted = changed_files.iter().any(|p| !p.exists());
    Ok((changed_files, affected_modules, entity_changes, has_deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::IncrementalSection;
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

    fn make_config() -> WikiConfig {
        WikiConfig {
            incremental: IncrementalSection {
                enabled: true,
                strategy: IncrementalStrategy::GitDiff,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// 非 Git 目录：GitDiff 增量回退全量（changed_files = 所有 insights 路径）
    #[test]
    fn test_git_diff_incremental_non_git_falls_back_full() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_non_git_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let insights = vec![make_insight("src/a.rs"), make_insight("src/b.rs")];
        let graph = KnowledgeGraph::default();
        let config = make_config();
        let state_dir = dir.join(".state");

        let (changed, affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().all(|p| insights.iter().any(|i| &i.path == p)));
        assert!(affected.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 行数超限：回退全量
    #[test]
    fn test_git_diff_incremental_line_limit_falls_back_full() {        let dir = std::env::temp_dir().join(format!("repo_wiki_test_line_limit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        // 第一次提交：单文件 2 行
        std::fs::write(dir.join("a.txt"), "x\ny\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        // 基线 = 第一 commit：diff 才能覆盖第二次提交的超限变更
        let first_hash = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();

        // 第二次提交：超上限行数（超过 MAX_DIFF_LINES 行的内容）
        let big = "x\n".repeat(MAX_DIFF_LINES + 1);
        std::fs::write(dir.join("a.txt"), big).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "big", &tree, &[&repo.head().unwrap().peel_to_commit().unwrap()]).unwrap();

        let insights = vec![make_insight("a.txt")];
        let graph = KnowledgeGraph::default();
        let config = make_config();
        let state_dir = dir.join(".state");

        // 基线 = 第一 commit：无基线时首次 update 会走"回退全量"（A1），
        // 超限回退分支需要基线与有变更两个前提都成立
        let baseline = GenerationState::from_insights(
            &crate::project::ProjectRoot::new(dir.clone()),
            &insights,
            &first_hash,
        )
        .unwrap();
        baseline.save(&state_dir).unwrap();

        let (changed, affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
        assert_eq!(changed.len(), 1);
        assert!(affected.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1：删除文件后增量更新，被删路径必须进入 changed_files，
    /// 下游删除清理（cleanup_deleted_outputs）才能命中并清除旧输出
    #[test]
    fn test_deleted_files_in_changed_set() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_deleted_changed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        // 第一次提交：src/foo.rs 存在
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("foo.rs"), "fn foo() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        // 基线 = 第一 commit：diff 才能覆盖第二次提交的删除变更
        let first_hash = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();

        // 第二次提交：删除 src/foo.rs（index.remove_path 后再提交）
        std::fs::remove_file(src.join("foo.rs")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("src/foo.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "delete", &tree, &[&repo.head().unwrap().peel_to_commit().unwrap()]).unwrap();

        // insights 只含现存文件（被删文件不在其中）
        let insights: Vec<FileInsight> = Vec::new();
        let graph = KnowledgeGraph::default();
        let config = make_config();
        let state_dir = dir.join(".state");

        // 基线 = 第一 commit：删除检测是"有基线增量"的语义，
        // 无基线时（A1）首次 update 直接回退全量，不经过 diff 路径
        let baseline = GenerationState::from_insights(
            &crate::project::ProjectRoot::new(dir.clone()),
            &insights,
            &first_hash,
        )
        .unwrap();
        baseline.save(&state_dir).unwrap();

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_watch_deleted_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        // 先保存状态：a.rs 内容未变，指纹比对不命中
        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let state = GenerationState::from_insights(&root, std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();

        // watch 事件传入已删除路径（磁盘上不存在）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        let config = make_config();

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_file_watch_incremental(&root, std::slice::from_ref(&insight), &graph, &config, &state_dir, std::slice::from_ref(&deleted)).unwrap();
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
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_watch_union_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        // 保存状态（a.rs 指纹 v1），随后修改内容 → 指纹比对命中
        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let state = GenerationState::from_insights(&root, std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();
        std::fs::write(src.join("a.rs"), "v2").unwrap();

        // watch 事件传入另一路径（磁盘上不存在，模拟删除）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        let config = make_config();

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_file_watch_incremental(&root, std::slice::from_ref(&insight), &graph, &config, &state_dir, std::slice::from_ref(&deleted)).unwrap();
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

    /// 票 03：中途存盘必须保留旧状态保护字段（LLM 生成失败场景的防回归）。
    /// 构造带 protected_docs/doc_fingerprints 的旧状态 → 跑 FileWatch 增量
    /// （其内部 from_insights + preserve_protection 后落盘）→ 断言磁盘状态
    /// 保护字段仍在（生成失败后下次运行保护不丢）。
    #[test]
    fn test_file_watch_midway_save_preserves_protection() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_watch_protect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "v1").unwrap();

        let insight = make_insight(src.join("a.rs").to_string_lossy().as_ref());
        let state_dir = dir.join(".state");

        // 旧状态：带人工保护（模拟上次生成完成后的状态）
        let root = crate::project::ProjectRoot::new(dir.clone());
        let mut old = GenerationState::from_insights(&root, std::slice::from_ref(&insight), "old").unwrap();
        old.protected_docs = vec!["wiki/zh/manual.md".to_string()];
        old.doc_fingerprints = std::collections::HashMap::from([("wiki/zh/manual.md".to_string(), "fp".to_string())]);
        old.save(&state_dir).unwrap();

        // 触发 FileWatch 增量（内容变更 → 中途存盘路径执行）
        std::fs::write(src.join("a.rs"), "v2").unwrap();
        let changed = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        let config = make_config();
        run_file_watch_incremental(&root, std::slice::from_ref(&insight), &graph, &config, &state_dir, std::slice::from_ref(&changed)).unwrap();

        // 磁盘状态必须保留保护字段（中途失败后人工修改保护不失效）
        let saved = GenerationState::load(&state_dir).unwrap();
        assert_eq!(saved.protected_docs, vec!["wiki/zh/manual.md"], "中途存盘不得清空保护集");
        assert_eq!(saved.doc_fingerprints.get("wiki/zh/manual.md").map(String::as_str), Some("fp"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1：全新仓库首次 update（无基线状态）不得静默短路——
    /// 空 diff 时应回退全量生成，避免首用产出空 wiki
    #[test]
    fn test_first_update_no_baseline_falls_back_full() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_first_update_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        // 单 commit 仓库（HEAD 无父）：无基线时 diff 为空
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();
        let config = make_config();
        let state_dir = dir.join(".state");

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_skip_nochange_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        let head_hash = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();
        let config = make_config();
        let state_dir = dir.join(".state");

        // 基线 = 当前 HEAD：diff 为空 → 应跳过（changed_files 为空）
        let baseline = GenerationState::from_insights(
            &crate::project::ProjectRoot::new(dir.clone()),
            &insights,
            &head_hash,
        )
        .unwrap();
        baseline.save(&state_dir).unwrap();

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
        assert!(changed.is_empty(), "有基线且无变更时应跳过，不得回退全量: {:?}", changed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A2：incremental.enabled=false 时 update 应真正执行全量生成
    ///（返回全部文件集），而非日志声称全量却返回空变更集导致跳过
    #[test]
    fn test_update_disabled_falls_back_full() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_disabled_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let insights = vec![make_insight("src/a.rs"), make_insight("src/b.rs")];
        let graph = KnowledgeGraph::default();
        let mut config = make_config();
        config.incremental.enabled = false;

        let result = run_incremental_update_at(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &graph,
            &config,
            &[],
        )
        .unwrap();
        assert_eq!(
            result.changed_files.len(),
            2,
            "增量禁用时应回退全量（changed_files = 全部源文件），不得返回空集跳过: {:?}",
            result.changed_files
        );
        assert!(result.affected_modules.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A6：状态文件损坏（load 失败）时不 panic、按无基线回退全量，
    /// 且不静默（warn 由 tracing 输出，行为断言为回退全量）
    #[test]
    fn test_corrupt_state_falls_back_full() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_corrupt_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        // 写损坏的状态 JSON
        let state_dir = dir.join(".state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("generation_state.json"), "{ 这不是合法 JSON").unwrap();

        let insights = vec![make_insight("a.rs")];
        let graph = KnowledgeGraph::default();
        let config = make_config();

        let (changed, _affected, _entity_changes, _has_deleted) =
            run_git_diff_incremental(&ProjectRoot::new(dir.clone()), &insights, &graph, &config, &state_dir).unwrap();
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
            "repo_wiki_test_noop_{}_{}",
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
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();

        let mut config = make_config();
        config.output.dir = dir.join("out").to_string_lossy().into_owned();
        config.scope.include = vec!["src/**".to_string()];
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
        let state_dir = Path::new(&config.output.dir).join(".state");
        state.save(&state_dir).unwrap();
        std::fs::create_dir_all(Path::new(&config.output.dir).join("wiki").join("zh")).unwrap();
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

        let repo = git2::Repository::open(&dir).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "fn a() { println!(); }\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&repo.head().unwrap().peel_to_commit().unwrap()]).unwrap();

        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "有新提交不得跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// head 相同但工作树有 scope 内未提交变更 → 不跳过；
    /// scope 外变更（产物目录）→ 仍跳过
    #[test]
    fn test_noop_status_scope_filtering() {
        let (dir, head, config) = setup_noop_fixture();
        save_baseline(&dir, &config, &head);

        // scope 内（src/）未提交变更 → 不跳过
        std::fs::write(dir.join("src").join("a.rs"), "fn a() { /* dirty */ }\n").unwrap();
        assert!(
            !should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "scope 内未提交变更不得跳过"
        );
        // 还原后产物目录（out/，scope 外）出现新文件 → 仍跳过（不阻断）
        std::fs::write(dir.join("src").join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            Path::new(&config.output.dir).join("out-of-scope.txt"),
            "not code",
        )
        .unwrap();
        assert!(
            should_skip_noop(&ProjectRoot::new(dir.clone()), &config).unwrap(),
            "scope 外变更（产物目录）不应阻断跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物目录缺失 → 不跳过（no-op 固有盲区：产物被删不得保持缺失）
    #[test]
    fn test_noop_no_skip_when_output_missing() {
        let (dir, head, config) = setup_noop_fixture();
        // 只存基线，不建产物目录
        let insights = vec![make_insight("src/a.rs")];
        let state = GenerationState::from_insights(
            &ProjectRoot::new(dir.clone()),
            &insights,
            &head,
        )
        .unwrap();
        let state_dir = Path::new(&config.output.dir).join(".state");
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
}
