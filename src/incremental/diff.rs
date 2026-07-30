use std::path::PathBuf;

use anyhow::Result;

/// Git diff 分析结果
#[derive(Debug, Clone, Default)]
pub struct GitDiffResult {
    /// 新增文件列表
    pub added: Vec<PathBuf>,
    /// 修改文件列表
    pub modified: Vec<PathBuf>,
    /// 删除文件列表
    pub deleted: Vec<PathBuf>,
    /// 重命名文件列表（旧路径，新路径）
    pub renamed: Vec<(PathBuf, PathBuf)>,
    /// 起始 commit hash
    pub from_commit: String,
    /// 目标 commit hash
    pub to_commit: String,
}

/// 分析 Git diff，返回变更的文件列表
///
/// 使用 git2 库分析 last_commit_hash（如有）与 HEAD 的差异。
/// 无 Git 历史或 last_commit_hash 为 None 时退化为 HEAD^。
/// 无父 commit 时返回空结果。
pub fn analyze_git_diff(repo_path: &std::path::Path, last_commit_hash: Option<&str>) -> Result<GitDiffResult> {
    let repo = match git2::Repository::open(repo_path) {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!("无法打开 Git 仓库: {}", repo_path.display());
            return Ok(GitDiffResult::default());
        }
    };

    // 获取 HEAD commit
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => {
            tracing::info!("Git 仓库无 HEAD（空仓库或未提交）");
            return Ok(GitDiffResult::default());
        }
    };

    let head_commit = head.peel_to_commit()?;
    let head_tree = head_commit.tree()?;
    let head_oid = head_commit.id().to_string();

    // 确定 from_commit：优先使用 last_commit_hash，退化为 HEAD^
    let from_commit = if let Some(prev_hash) = last_commit_hash {
        prev_hash.to_string()
    } else if head_commit.parents().count() > 0 {
        let parent = head_commit.parent(0)?;
        parent.id().to_string()
    } else {
        tracing::info!("首次提交或无上次生成记录，无法做增量 diff");
        return Ok(GitDiffResult::default());
    };

    // 解析 from_commit 对应的 tree
    let from_obj = match repo.revparse_single(&from_commit) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("无法解析 commit {}: {}，退化为空 diff", from_commit, e);
            return Ok(GitDiffResult::default());
        }
    };
    let from_tree = from_obj.peel_to_tree()?;

    let diff = repo.diff_tree_to_tree(Some(&from_tree), Some(&head_tree), None)?;

    let mut result = GitDiffResult {
        from_commit,
        to_commit: head_oid,
        ..Default::default()
    };

    diff.foreach(
        &mut |delta, _| {
            let new_file = delta.new_file();
            let old_file = delta.old_file();

            match delta.status() {
                git2::Delta::Added => {
                    if let Some(path) = new_file.path() {
                        result.added.push(path.to_path_buf());
                    }
                }
                git2::Delta::Deleted => {
                    if let Some(path) = old_file.path() {
                        result.deleted.push(path.to_path_buf());
                    }
                }
                git2::Delta::Modified => {
                    if let Some(path) = new_file.path() {
                        result.modified.push(path.to_path_buf());
                    }
                }
                git2::Delta::Renamed => {
                    let old = old_file.path().map(|p| p.to_path_buf());
                    let new = new_file.path().map(|p| p.to_path_buf());
                    if let (Some(old), Some(new)) = (old, new) {
                        result.renamed.push((old, new));
                    }
                }
                _ => {}
            }
            true
        },
        None,
        None,
        None,
    )?;

    Ok(result)
}

/// 获取当前 HEAD commit hash
pub fn get_head_commit_hash() -> Result<String> {
    let repo = git2::Repository::open(".")?;
    let head = repo.head()?;
    let oid = head.target().ok_or_else(|| anyhow::anyhow!("HEAD 没有目标"))?;
    Ok(oid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_diff_result_default() {
        let result = GitDiffResult::default();
        assert!(result.added.is_empty());
        assert!(result.modified.is_empty());
        assert!(result.deleted.is_empty());
        assert!(result.renamed.is_empty());
        assert!(result.from_commit.is_empty());
        assert!(result.to_commit.is_empty());
    }

    #[test]
    fn test_analyze_git_diff_non_repo() {
        let tmp = std::env::temp_dir().join("repo-wiki-test-diff-nonexistent");
        let result = analyze_git_diff(&tmp, None);

        // 非 Git 仓库应返回空结果
        if let Ok(r) = result {
            assert!(r.added.is_empty());
        }
    }
}
