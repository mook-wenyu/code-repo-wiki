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

/// 在指定项目根下获取当前 HEAD commit hash
///
/// git 仓库定位基准显式注入：不再依赖进程 cwd（watch 常驻进程的
/// cwd 漂移不再改变仓库解析目标）。
pub fn get_head_commit_hash_at(root: &crate::project::ProjectRoot) -> Result<String> {
    let repo = git2::Repository::open(root.path())?;
    let head = repo.head()?;
    let oid = head
        .target()
        .ok_or_else(|| anyhow::anyhow!("HEAD 没有目标"))?;
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
}
