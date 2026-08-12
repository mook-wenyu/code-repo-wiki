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
// 崩溃残留（进程被杀，锁文件遗留）：自动清理——读取锁内 PID 做进程
// 活性检测，进程不存在则视为残留锁删除重试。取舍：自动清理存在把
// 「另一实例正在生成」误判为残留的风险，但真实故障场景（外部强杀
// TerminateProcess/任务管理器结束，Drop 不执行）会留下永久锁导致
// watch/git hook 持续失败（实测 67 次锁冲突），活性检测把风险窗口
// 收窄到「PID 恰好被复用且同目录并发」的极小概率，收益大于风险。

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
/// 排查），存在则做残留判定——锁内 PID 无对应进程（或 PID 缺失/不可
/// 解析）视为残留锁删除重试一次；PID 存活视为真并发报错。
pub fn acquire_run_lock(config: &crate::config::schema::WikiConfig) -> Result<RunLock> {
    let state_dir = config.output_dir().join(".state");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
    let path = state_dir.join("run.lock");
    // 自愈重试一次：首次 create_new 失败（残留）→ 清理 → 二次创建；
    // 二次仍失败则报真并发（活 PID 场景）
    acquire_run_lock_inner(&path, &process_alive)
}

/// 锁获取核心：is_alive 注入使测试不依赖真实进程表
fn acquire_run_lock_inner(
    path: &std::path::Path,
    is_alive: &dyn Fn(u32) -> bool,
) -> Result<RunLock> {
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            use std::io::Write;
            // PID 写入失败改显式错误：无 PID 的锁无法做残留判定（下次
            // 冲突要么误删真并发锁要么永久卡死），故移除刚创建的锁并报错
            if let Err(e) = writeln!(f, "{}", std::process::id()) {
                let _ = std::fs::remove_file(path);
                anyhow::bail!("运行锁 PID 写入失败（锁已移除，可重试）: {e}");
            }
            Ok(RunLock { path: path.to_path_buf() })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 冲突：读取锁内容做残留判定
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                // 读失败（竞态删除/权限）：保守报错指引人工处理，不自愈
                Err(e) => anyhow::bail!(
                    "运行锁读取失败（可能另一实例正在释放），锁文件: {}。错误: {e}",
                    path.display()
                ),
            };
            let pid: Option<u32> = content.trim().parse().ok();
            match pid {
                // PID 存活 → 真并发
                Some(pid) if is_alive(pid) => anyhow::bail!(
                    "另一 code-repo-wiki 实例正在运行（PID {pid}，锁文件: {}）。该进程结束后可重试",
                    path.display()
                ),
                // PID 无进程或不可解析（空/半写锁）→ 疑似残留，原子改名认领
                //
                // 双持防护（reviewer REJECTED 修复）：「重读校验 → remove → create」
                // 中 remove 与 create 之间无原子屏障——P1 remove→create 新锁后，
                // P2 的 remove 会误删 P1 的新锁 → 双持。rename 是原子操作：
                // 只有一方能把 run.lock 改名为私有 stale 名（其余方 ENOENT），
                // 赢家随后删除自己改名的文件并 create_new——此时它是唯一
                // 持有者，输家永远无法删除赢家的新锁。
                _ => {
                    // 原子认领：run.lock → run.lock.stale.<pid>（本进程 pid，
                    // 唯一私有名；rename 成功仅一方）
                    let stale = path.with_file_name(format!(
                        "run.lock.stale.{}",
                        std::process::id()
                    ));
                    match std::fs::rename(path, &stale) {
                        Ok(()) => {
                            // 赢家：重读改名后文件，内容仍为陈旧（死 PID/不可
                            // 解析）→ 删除私有文件 + create_new 新锁（此刻唯一
                            // 持有者，无竞争）；内容已变（理论不可达——rename
                            // 唯一性保证无第二赢家）→ rename 回原位并报接管
                            let still_stale = match std::fs::read_to_string(&stale) {
                                Ok(c) => c.trim() == content.trim(),
                                Err(_) => false,
                            };
                            if still_stale {
                                let _ = std::fs::remove_file(&stale);
                                match std::fs::OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .open(path)
                                {
                                    Ok(mut f) => {
                                        use std::io::Write;
                                        if let Err(e) = writeln!(f, "{}", std::process::id()) {
                                            let _ = std::fs::remove_file(path);
                                            anyhow::bail!(
                                                "运行锁 PID 写入失败（锁已移除，可重试）: {e}"
                                            );
                                        }
                                        Ok(RunLock { path: path.to_path_buf() })
                                    }
                                    Err(e) => {
                                        let holder = std::fs::read_to_string(path)
                                            .ok()
                                            .and_then(|c| c.trim().parse::<u32>().ok())
                                            .map(|pid| format!("，当前持锁者 PID {pid}"))
                                            .unwrap_or_default();
                                        Err(e).with_context(|| {
                                            format!(
                                                "获取运行锁失败（自愈重试后仍被占用{holder}）: {}",
                                                path.display()
                                            )
                                        })
                                    }
                                }
                            } else {
                                // 内容已变（超理论防御——rename 唯一性
                                // 保证无第二赢家）：不归位直接报接管（stale 残留
                                // 无害，与 remove 失败允许残留一致；归位 rename 在
                                // Windows 会覆盖已重建目标，反而引入理论覆盖风险）
                                anyhow::bail!(
                                    "运行锁内容在自愈认领后已变化（他进程可能已接管），放弃自愈，锁文件: {}",
                                    path.display()
                                );
                            }
                        }
                        // 输家：锁已被他人认领/删除 → 递归重试一次
                        // （锁已消失则 create_new 成功；新锁已建则走活 PID/残留判定）
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            acquire_run_lock_inner(path, is_alive)
                        }
                        Err(e) => anyhow::bail!(
                            "运行锁自愈认领失败（{}），锁文件: {}",
                            e,
                            path.display()
                        ),
                    }
                }
            }
        }
        Err(e) => Err(e).with_context(|| format!("获取运行锁失败: {}", path.display())),
    }
}

/// 进程活性检测：pid 对应进程存在返回 true
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) 不发送信号仅探活：0 → 存在；-1 且 errno=ESRCH → 不存在；
    // EPERM → 进程存在但无权限（视为存活）
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || (r == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

/// 进程活性检测：pid 对应进程存在返回 true
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !h.is_null() {
            CloseHandle(h);
            true
        } else {
            // ERROR_INVALID_PARAMETER → PID 不存在；其他错误（权限等）
            // 保守视为存在（无法确认死亡就不自愈，避免真并发窗口）
            GetLastError() != ERROR_INVALID_PARAMETER
        }
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

    /// 残留锁（死 PID）自愈：锁内 PID 无进程 → 删除重试成功
    #[test]
    fn test_run_lock_stale_dead_pid_selfheals() {
        let dir = temp_path("lock_stale", "");
        let _config = lock_config(&dir);
        let path = dir.join(".state/run.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "99999999\n").unwrap(); // 极大 PID 必然无进程
        let lock = acquire_run_lock_inner(&path, &|_| false).unwrap();
        assert!(path.exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真并发（活 PID）报错：注入 is_alive=true
    #[test]
    fn test_run_lock_live_pid_reports_concurrency() {
        let dir = temp_path("lock_live", "");
        let _config = lock_config(&dir);
        let path = dir.join(".state/run.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "1\n").unwrap();
        let err = acquire_run_lock_inner(&path, &|_| true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("正在运行"), "应报并发错误: {msg}");
        assert!(msg.contains("PID 1"), "报错应含 PID: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空锁文件（PID 缺失）自愈删除重试
    #[test]
    fn test_run_lock_empty_file_selfheals() {
        let dir = temp_path("lock_empty", "");
        let _config = lock_config(&dir);
        let path = dir.join(".state/run.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let lock = acquire_run_lock_inner(&path, &|_| true).unwrap();
        assert!(path.exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 半写锁文件（PID 不可解析）自愈删除重试
    #[test]
    fn test_run_lock_half_written_selfheals() {
        let dir = temp_path("lock_half", "");
        let _config = lock_config(&dir);
        let path = dir.join(".state/run.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "12x").unwrap(); // 半写：PID 前缀不可解析
        let lock = acquire_run_lock_inner(&path, &|_| true).unwrap();
        assert!(path.exists());
        drop(lock);
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
