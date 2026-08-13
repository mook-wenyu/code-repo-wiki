//! 测试基建：git2 提交 helper（libgit2 Windows 竞态有界重试）
//!
//! 供 lib 单元测试与 tests/ 集成测试共用（integration tests 无法访问
//! `pub(crate)`，故为 pub 模块）。封装「add_all + write + write_tree +
//! commit」完整提交序列，并对 libgit2 在 Windows 上的两个瞬时环境竞态
//! 做有界重试：
//!
//! 1. `file changed before we could read it`（class Filesystem=30，
//!    `diff_file.c:346` 的尺寸复查）——测试写文件后立即 add_all，文件在
//!    workdir 迭代器 stat 与内容读取之间被文件系统缓存落盘/安全软件触碰，
//!    尺寸变化触发。瞬态，重读即恢复。
//! 2. `GIT_ELOCKED`（`.git/index.lock` 残留，`git_indexwriter_init` 经
//!    `git_filebuf_open` 的 O_CREAT|O_EXCL 创建）——测试临时仓库独占，
//!    残留锁必为上次崩溃/竞态遗留。
//!
//! 判定原则：上述两者均为测试环境固有竞态（产品代码路径不调用
//! index.add_all，见 src/incremental/ 生产实现），重试幂等安全
//! （add_all 每次重算工作树→索引 diff，重复应用结果一致）。其余错误
//! 立即 panic——失败即报错，不吞错、不掩盖非瞬时问题。
use std::path::Path;

/// 提交当前工作区全部文件，返回 commit id
///
/// 有界重试：命中瞬时竞态时清理残留 `.git/index.lock` 后重试，最多 4 次
/// （200ms 固定退避，避开文件缓存落盘/安全软件触碰窗口）。非瞬时错误
/// 立即 panic。
pub fn commit_all(repo_path: &Path, message: &str) -> String {
    const MAX_ATTEMPTS: usize = 4;
    for attempt in 0..MAX_ATTEMPTS {
        match try_commit_all(repo_path, message) {
            Ok(id) => return id,
            Err(e) if is_transient_git_error(&e) && attempt + 1 < MAX_ATTEMPTS => {
                // 测试临时仓库由本测试独占，残留 index.lock 必为上次失败遗留，
                // 删除安全（git CLI 对崩溃残留锁同样要求人工清理）
                if let Ok(repo) = git2::Repository::open(repo_path) {
                    let _ = std::fs::remove_file(repo.path().join("index.lock"));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => panic!("git 提交失败（第 {} 次尝试后）: {e}", attempt + 1),
        }
    }
    unreachable!("有界重试循环必然返回或 panic")
}

fn try_commit_all(repo_path: &Path, message: &str) -> Result<String, git2::Error> {
    let repo = git2::Repository::open(repo_path)?;
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = git2::Signature::now("test", "test@test.com")?;
    let commit_id = match repo.head().ok() {
        Some(head) => {
            let parent = head.peel_to_commit()?;
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?
        }
        None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?,
    };
    Ok(commit_id.to_string())
}

/// 判定 libgit2 的瞬时环境竞态（Windows 特有）
///
/// - Filesystem 类：文件在 diff 内容读取时尺寸变化（`diff_file.c` 用
///   GIT_ERROR_FILESYSTEM 设置"file changed before we could read it"）——
///   用错误类而非消息文本判定，跨 libgit2 版本稳定
/// - Locked 码：`.git/index.lock` 残留冲突（`GIT_ELOCKED`）
fn is_transient_git_error(e: &git2::Error) -> bool {
    e.class() == git2::ErrorClass::Filesystem || e.code() == git2::ErrorCode::Locked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预置残留 `.git/index.lock`：commit_all 应清理后重试成功（不再 panic）
    #[test]
    fn commit_all_cleans_stale_index_lock() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_git_commit_lock_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo.path().join("index.lock"), "stale").unwrap();

        let id = commit_all(&dir, "init");
        assert!(!id.is_empty(), "残留 index.lock 时应清理重试后成功");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 防回归：提交后 HEAD 可解析且第二次提交含父提交（真实 diff 语义保持）
    #[test]
    fn commit_all_produces_valid_commits() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_git_commit_valid_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();

        let id = commit_all(&dir, "init");
        let repo = git2::Repository::open(&dir).unwrap();
        assert_eq!(
            repo.head().unwrap().peel_to_commit().unwrap().id().to_string(),
            id,
            "首次提交 HEAD 应指向返回的 commit id"
        );

        // 第二次提交（含父提交路径）
        std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
        let id2 = commit_all(&dir, "second");
        let repo = git2::Repository::open(&dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.id().to_string(), id2, "第二次提交 HEAD 应推进");
        assert_eq!(head.parent_count(), 1, "第二次提交应含父提交");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
