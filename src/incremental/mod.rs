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
            run_git_diff_incremental(insights, graph, config, &state_dir)?
        }
        IncrementalStrategy::FileWatch => {
            run_file_watch_incremental(insights, graph, config, &state_dir)?
        }
    };
    Ok(IncrementalResult { changed_files, affected_modules })
}

/// Git diff 策略的增量更新
fn run_git_diff_incremental(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    _config: &WikiConfig,
    state_dir: &Path,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    // 1. 分析 Git diff
    let last_commit_hash = GenerationState::load(state_dir)
        .ok()
        .and_then(|s| s.last_commit_hash);
    let diff_result = match analyze_git_diff(Path::new("."), last_commit_hash.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("Git diff 分析失败，跳过增量更新: {}", e);
            return Ok((Vec::new(), Vec::new()));
        }
    };

    if diff_result.added.is_empty() && diff_result.modified.is_empty() && diff_result.deleted.is_empty() {
        tracing::info!("无文件变更，跳过更新");
        return Ok((Vec::new(), Vec::new()));
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
    let affected_modules = propagate_impact(&all_changed, graph);

    // 4. 保存新的状态
    if let Ok(new_state) = GenerationState::from_insights(insights, &diff_result.to_commit) {
        if let Err(e) = new_state.save(&state_dir) {
            tracing::warn!("保存生成状态失败: {}", e);
        }
    }

    tracing::info!("增量更新分析完成: {} 个模块受影响", affected_modules.len());
    Ok((all_changed, affected_modules))
}

/// FileWatch 策略的增量更新（变更文件由外部传入）
fn run_file_watch_incremental(
    insights: &[FileInsight],
    graph: &KnowledgeGraph,
    _config: &WikiConfig,
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
    let affected_modules = propagate_impact(&changed_files, graph);

    // 保存新状态
    if let Ok(new_state) = GenerationState::from_insights(insights, "file-watch") {
        if let Err(e) = new_state.save(state_dir) {
            tracing::warn!("保存生成状态失败: {}", e);
        }
    }

    tracing::info!("FileWatch 增量分析完成: {} 个模块受影响", affected_modules.len());
    Ok((changed_files, affected_modules))
}
