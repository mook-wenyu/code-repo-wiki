pub mod diff;
pub mod impact;
pub mod state;
pub mod watch;

use std::path::Path;

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::ingest::parser::FileInsight;
use crate::model::KnowledgeGraph;

use self::diff::analyze_git_diff;
use self::impact::propagate_impact;
use self::state::GenerationState;
use crate::config::schema::IncrementalStrategy;

/// diff 行数超过该上限时回退全量生成（防止超大变更集导致 LLM 成本失控）
const MAX_DIFF_LINES: usize = 10_000;

/// 增量更新结果
pub struct IncrementalResult {
    /// 实际发生变更的文件路径（用于 LLM 生成过滤）
    pub changed_files: Vec<std::path::PathBuf>,
    /// 受影响的模块名称列表（用于日志和下游分析）
    pub affected_modules: Vec<String>,
}

/// 运行增量更新分析
///
/// 1. 分析 Git diff 获取变更文件列表
/// 2. 在知识图谱上传播变更影响
/// 3. 返回包含变更文件路径和受影响模块的结果
///
/// 无 Git 历史时跳过增量更新。
pub fn run_incremental_update(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
) -> Result<IncrementalResult> {
    if !config.incremental.enabled {
        tracing::info!("增量更新已禁用，将执行全量生成");
        return Ok(IncrementalResult { changed_files: Vec::new(), affected_modules: Vec::new() });
    }

    let state_dir = Path::new(&config.output.dir).join(".state");

    // 按策略分发：GitDiff 或 FileWatch
    let (changed_files, affected_modules) = match config.incremental.strategy {
        IncrementalStrategy::GitDiff => {
            run_git_diff_incremental(insights, graph, config, &state_dir, Path::new("."))?
        }
        IncrementalStrategy::FileWatch => {
            run_file_watch_incremental(insights, graph, config, &state_dir)?
        }
    };
    Ok(IncrementalResult { changed_files, affected_modules })
}

/// 回退全量生成：changed_files 为所有源文件，affected_modules 为空
/// （affected_modules 只用于日志与生成过滤的模块维度，全量回退按文件维度处理）
fn fallback_to_full(insights: &[FileInsight]) -> (Vec<std::path::PathBuf>, Vec<String>) {
    let all: Vec<std::path::PathBuf> = insights.iter().map(|i| i.path.clone()).collect();
    (all, Vec::new())
}

/// Git diff 策略的增量更新
fn run_git_diff_incremental(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    state_dir: &Path,
    repo_path: &Path,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    // 1. 分析 Git diff
    let last_commit_hash = GenerationState::load(state_dir)
        .ok()
        .and_then(|s| s.last_commit_hash);
    let diff_result = match analyze_git_diff(repo_path, last_commit_hash.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            // 非 Git 仓库：无法做增量，回退全量生成
            tracing::warn!("Git diff 分析失败，回退全量生成: {}", e);
            return Ok(fallback_to_full(insights));
        }
    };

    if diff_result.added.is_empty() && diff_result.modified.is_empty() && diff_result.deleted.is_empty() {
        tracing::info!("无文件变更，跳过更新");
        return Ok((Vec::new(), Vec::new()));
    }

    if diff_result.added_lines + diff_result.deleted_lines > MAX_DIFF_LINES {
        tracing::warn!(
            "diff 行数超过上限 {}（新增 {} 行, 删除 {} 行），回退全量生成",
            MAX_DIFF_LINES,
            diff_result.added_lines,
            diff_result.deleted_lines
        );
        return Ok(fallback_to_full(insights));
    }

    tracing::info!(
        "Git diff: {} 个新增, {} 个修改, {} 个删除, {} 个重命名",
        diff_result.added.len(),
        diff_result.modified.len(),
        diff_result.deleted.len(),
        diff_result.renamed.len()
    );

    // 2. 收集变更文件路径
    let all_changed: Vec<std::path::PathBuf> = diff_result
        .added
        .iter()
        .chain(&diff_result.modified)
        .cloned()
        .collect();

    // 3. 在知识图谱上传播变更影响
    let affected_modules = propagate_impact(&all_changed, graph, config.incremental.max_depth);

    // 4. 保存新的状态
    if let Ok(new_state) = GenerationState::from_insights(insights, &diff_result.to_commit) && let Err(e) = new_state.save(state_dir) {
        tracing::warn!("保存生成状态失败: {}", e);
    }

    tracing::info!("增量更新分析完成: {} 个模块受影响", affected_modules.len());
    Ok((all_changed, affected_modules))
}

/// FileWatch 策略的增量更新（变更文件由外部传入）
fn run_file_watch_incremental(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    state_dir: &Path,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    // 重新加载状态，比较文件指纹
    let state = GenerationState::load(state_dir).ok();
    let mut changed_files: Vec<std::path::PathBuf> = Vec::new();

    for insight in insights {
        if let Ok(true) = state.as_ref().map(|s| s.is_file_changed(&insight.path)).unwrap_or(Ok(true)) {
            changed_files.push(insight.path.clone());
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

        let (changed, affected) =
            run_git_diff_incremental(&insights, &graph, &config, &state_dir, &dir).unwrap();
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().all(|p| insights.iter().any(|i| &i.path == p)));
        assert!(affected.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 行数超限：回退全量
    #[test]
    fn test_git_diff_incremental_line_limit_falls_back_full() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_line_limit_{}", std::process::id()));
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

        let (changed, affected) =
            run_git_diff_incremental(&insights, &graph, &config, &state_dir, &dir).unwrap();
        assert_eq!(changed.len(), 1);
        assert!(affected.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
