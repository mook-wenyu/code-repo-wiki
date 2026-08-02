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
        return Ok(IncrementalResult { changed_files: Vec::new(), affected_modules: Vec::new(), entity_changes: EntityChangeSet::default() });
    }

    let state_dir = Path::new(&config.output.dir).join(".state");

    // 按策略分发：GitDiff 或 FileWatch
    let (changed_files, affected_modules, entity_changes) = match config.incremental.strategy {
        IncrementalStrategy::GitDiff => {
            run_git_diff_incremental(root, insights, graph, config, &state_dir)?
        }
        IncrementalStrategy::FileWatch => {
            let (files, modules) = run_file_watch_incremental(insights, graph, config, &state_dir, watch_paths)?;
            (files, modules, EntityChangeSet::default())
        }
    };
    Ok(IncrementalResult { changed_files, affected_modules, entity_changes })
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
) -> Result<(Vec<std::path::PathBuf>, Vec<String>, EntityChangeSet)> {
    // 1. 分析 Git diff
    let last_commit_hash = GenerationState::load(state_dir)
        .ok()
        .and_then(|s| s.last_commit_hash);
    let diff_result = match analyze_git_diff(root.path(), last_commit_hash.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            // 非 Git 仓库：无法做增量，回退全量生成
            tracing::warn!("Git diff 分析失败，回退全量生成: {}", e);
            let (files, modules) = fallback_to_full(insights);
            return Ok((files, modules, EntityChangeSet::default()));
        }
    };

    if diff_result.added.is_empty() && diff_result.modified.is_empty() && diff_result.deleted.is_empty() {
        tracing::info!("无文件变更，跳过更新");
        return Ok((Vec::new(), Vec::new(), EntityChangeSet::default()));
    }

    if diff_result.added_lines + diff_result.deleted_lines > MAX_DIFF_LINES {
        tracing::warn!(
            "diff 行数超过上限 {}（新增 {} 行, 删除 {} 行），回退全量生成",
            MAX_DIFF_LINES,
            diff_result.added_lines,
            diff_result.deleted_lines
        );
        let (files, modules) = fallback_to_full(insights);
        return Ok((files, modules, EntityChangeSet::default()));
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

    // 4. 保存新的状态
    if let Ok(new_state) = GenerationState::from_insights(insights, &diff_result.to_commit) && let Err(e) = new_state.save(state_dir) {
        tracing::warn!("保存生成状态失败: {}", e);
    }

    tracing::info!("增量更新分析完成: {} 个模块受影响", affected_modules.len());
    Ok((all_changed, affected_modules, entity_changes))
}

/// FileWatch 策略的增量更新（变更文件来自外部事件 + 指纹比对）
fn run_file_watch_incremental(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    state_dir: &Path,
    watch_paths: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    // 重新加载状态，比较文件指纹
    let state = GenerationState::load(state_dir).ok();
    let mut changed_files: Vec<PathBuf> = Vec::new();

    for insight in insights {
        if let Ok(true) = state.as_ref().map(|s| s.is_file_changed(&insight.path)).unwrap_or(Ok(true)) {
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
        return Ok((Vec::new(), Vec::new()));
    }

    // BFS 传播影响
    let affected_modules = propagate_impact(&changed_files, graph, config.incremental.max_depth);

    // 保存新状态
    if let Ok(new_state) = GenerationState::from_insights(insights, "file-watch") && let Err(e) = new_state.save(state_dir) {
        tracing::warn!("保存生成状态失败: {}", e);
    }

    tracing::info!("FileWatch 增量分析完成: {} 个模块受影响", affected_modules.len());
    Ok((changed_files, affected_modules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::IncrementalSection;

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

        let (changed, affected, _entity_changes) =
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

        let (changed, affected, _entity_changes) =
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

        let (changed, _affected, _entity_changes) =
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
        let state = GenerationState::from_insights(std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();

        // watch 事件传入已删除路径（磁盘上不存在）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        let config = make_config();

        let (changed, _affected) =
            run_file_watch_incremental(std::slice::from_ref(&insight), &graph, &config, &state_dir, std::slice::from_ref(&deleted)).unwrap();
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
        let state = GenerationState::from_insights(std::slice::from_ref(&insight), "test").unwrap();
        state.save(&state_dir).unwrap();
        std::fs::write(src.join("a.rs"), "v2").unwrap();

        // watch 事件传入另一路径（磁盘上不存在，模拟删除）
        let deleted = src.join("b.rs");
        let graph = KnowledgeGraph::default();
        let config = make_config();

        let (changed, _affected) =
            run_file_watch_incremental(std::slice::from_ref(&insight), &graph, &config, &state_dir, std::slice::from_ref(&deleted)).unwrap();
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
}
