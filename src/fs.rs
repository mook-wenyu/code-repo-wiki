//! 文件原子写入辅助（状态/快照/缓存/卡片统一落盘入口）
//!
//! 背景：项目内四处产物写入（generation_state.json / export_snapshot.json /
//! insights_cache.json / 知识卡片）此前各自 `std::fs::write` 直写或
//! remove+rename 折衷。直写非原子（截断写，崩溃/断电可留下半截文件，
//! 损坏即数据风险）；remove+rename 是旧 Windows 折衷（旧版 rename
//! 不覆盖已存在目标），存在"目标被删、临时文件未就位"的中间窗口。
//!
//! 本模块统一为"同目录临时文件写入 + rename 原子覆盖"：
//! - rustc 1.84+ 的 `std::fs::rename` 在 Windows 10 1709+ 使用
//!   FileRenameInfoEx + FILE_RENAME_POSIX_SEMANTICS（POSIX 语义），
//!   原子覆盖已存在目标，因此无需再先删目标（删 remove_file 前置即
//!   消除中间窗口）；本仓库 rustc 1.97.1，前提满足（2026-08-02 实测）。
//! - 同目录临时文件保证 rename 不跨文件系统（跨 mount 会失败）。
//! - 调用方（state/快照/缓存/卡片）各自决定失败语义（fail-loud 或
//!   warn+降级），本函数只负责原子落盘。

use std::path::Path;

use anyhow::{Context, Result};
/// 原子写入：内容写入 `path` 同目录的临时文件后 rename 覆盖
///
/// - 父目录不存在时自动创建（与各调用点现有一致）
/// - 临时文件名 = `{文件名}.tmp`（与历史 write_card_atomic 的约定一致，
///   崩溃残留的 .tmp 会被下次写入覆盖，无需清理逻辑）
/// - rename 覆盖目标为原子操作（POSIX 语义，见模块注释）
pub fn write_file_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    // 落盘屏障：rename 的原子性只保证「命名」原子替换，不保证数据已持久化。
    // 断电/崩溃可能留下「已 rename 但内容截断」的文件（salt 9c18c27 实证），
    // 因此在 rename 前显式 flush + fsync 数据。用写句柄打开以确保 Windows
    // FlushFileBuffers 语义（读句柄在不同平台上行为不一）。
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .with_context(|| format!("打开临时文件刷新失败: {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("刷新临时文件失败: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("原子替换失败: {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ==================== 单实例运行锁（v36 D4）====================
//
// 并发 generate/update/watch 会把状态/索引/产物互相覆盖（最后写入者
// 胜）。生成是「进程内串行、进程间互斥」的操作：本锁在
// run_pipeline_with_progress 入口以 create_new 原子获取，作用域=单次
// 生成全程，Drop 时释放（正常退出/错误传播都会走 Drop）。
//
// 崩溃残留（进程被杀，锁文件遗留）：报错信息明确指引人工删除——
// 不自动清理：自动清会把「另一实例正在生成中」误判为残留，反而引入
// 真并发窗口。

/// 运行锁：持有期间其他实例的生成入口被拒绝；Drop 释放
#[derive(Debug)]
pub struct RunLock {
    path: std::path::PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // 释放失败可忽略：锁文件残留会由下一次获取报错指引人工删除，
        // 此处报错无调用方（Drop 语义），静默符合预期
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 原子获取运行锁：.state/run.lock 不存在则创建（写入当前进程 PID 供
/// 排查），存在则报错——报错信息包含锁路径与处理指引。
pub fn acquire_run_lock(config: &crate::config::schema::WikiConfig) -> Result<RunLock> {
    let state_dir = config.output_dir().join(".state");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
    let path = state_dir.join("run.lock");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            // 写入 PID 供「锁是谁留下的」排查；失败仅告警（锁本身已建立）
            if let Err(e) = writeln!(f, "{}", std::process::id()) {
                eprintln!("code-repo-wiki: 运行锁 PID 写入失败（不影响锁）: {e}");
            }
            Ok(RunLock { path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
            "另一 code-repo-wiki 实例正在运行（锁文件: {}）。确认无残留实例后可删除该文件重试",
            path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("获取运行锁失败: {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_fs_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn lock_config(dir: &std::path::Path) -> crate::config::schema::WikiConfig {
        crate::config::schema::WikiConfig {
            output_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    /// 锁可获取；Drop 后释放（可再次获取）
    #[test]
    fn test_run_lock_acquire_and_release() {
        let dir = temp_path("lock_roundtrip", "");
        let config = lock_config(&dir);
        let lock = acquire_run_lock(&config).unwrap();
        assert!(dir.join(".state/run.lock").exists());
        drop(lock);
        assert!(!dir.join(".state/run.lock").exists(), "Drop 应释放锁");
        // 释放后可再次获取（幂等循环）
        let lock2 = acquire_run_lock(&config).unwrap();
        drop(lock2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁已存在时拒绝第二次获取，报错含路径与指引
    #[test]
    fn test_run_lock_rejects_second() {
        let dir = temp_path("lock_reject", "");
        let config = lock_config(&dir);
        let _lock = acquire_run_lock(&config).unwrap();
        let err = acquire_run_lock(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("正在运行"), "应报并发错误: {msg}");
        assert!(msg.contains("run.lock"), "报错应含锁路径: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新文件写入成功且内容正确
    #[test]
    fn test_write_new_file() {
        let path = temp_path("new", "a.json");
        write_file_atomic(&path, "{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\":1}");
        // 临时文件不应残留
        assert!(!path.with_extension("tmp").exists(), "rename 后不应残留临时文件");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 覆盖已存在文件（原子替换语义）
    #[test]
    fn test_overwrite_existing() {
        let path = temp_path("overwrite", "b.json");
        write_file_atomic(&path, "old").unwrap();
        write_file_atomic(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 父目录不存在时自动创建
    #[test]
    fn test_creates_parent_dir() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_fs_nested_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("deep").join("nested").join("c.json");
        write_file_atomic(&path, "x").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
// v17 F 组增量闭环验证
