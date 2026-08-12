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

// ==================== 单实例运行锁（Phase 15.1，fd-lock 内核锁）====================
//
// 并发 generate/update/watch 会把状态/索引/产物互相覆盖（最后写入者
// 胜）。生成是「进程内串行、进程间互斥」的操作：本锁在
// run_pipeline_with_progress 入口以内核 advisory 锁获取，作用域=单次
// 生成全程，Drop 时释放（正常退出/错误传播都会走 Drop）。
//
// 协议要点（v36 create_new+PID 活性检测方案废弃，改 fd-lock 内核锁）：
// - 锁文件 {output_dir}/.state/run.lock 常驻：仅首次运行创建，从不
//   删除。互斥性由内核 advisory 锁（Windows LockFileEx / Unix flock
//   族）保证，锁绑定「打开句柄」而非路径——只要所有实例都打开同一
//   路径，冲突检测就有效。
// - 崩溃残留（进程被强杀，句柄未关闭）无需自愈：内核在进程终止时
//   自动释放其持有的锁，新实例直接获锁；锁文件内容被截断重写为
//   新持锁者身份。旧方案的 PID 活性检测 / rename 认领因此整体删除。
// - 锁文件内容保存持锁者身份（pid/process/started 三行），供冲突
//   实例报错时定位「谁在运行」（内核锁只回答有/无，不回答谁持有）。

/// 运行锁：持有期间其他实例的生成入口被拒绝；Drop 释放
///
/// 字段说明：
/// - `_lock`：fd_lock 写守卫，是锁的真正载体。其 Drop 显式调用
///   UnlockFile 释放内核锁并关闭句柄；守卫被 drop 之前锁一直有效
///   （`#[must_use]`，若守卫不保存会在获取处立刻释放）。
/// - `path`：锁文件路径，仅用于诊断报错。
/// 锁文件本身常驻，本结构体 Drop 只关闭句柄，不删除文件。
#[derive(Debug)]
pub struct RunLock {
    _lock: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
    // 锁路径保留用于诊断（Debug 输出可见）；运行期冲突报错在
    // acquire_run_lock 内完成，本字段无运行期读取点
    #[allow(dead_code)]
    path: std::path::PathBuf,
}

/// 获取运行锁：打开（必要时创建）常驻锁文件 → 获取内核写锁 →
/// 写持锁者身份。锁被占用时报错并给出持锁者身份。
pub fn acquire_run_lock(config: &crate::config::schema::WikiConfig) -> Result<RunLock> {
    let state_dir = config.output_dir().join(".state");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
    let path = state_dir.join("run.lock");

    // 打开锁文件：create(true) 首次创建，read/write 供写身份与读冲突
    // 诊断；不 truncate、不删除——文件常驻，锁状态由内核管理。
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("获取运行锁失败: {}", path.display()))?;

    // guard 借用 RwLock 本身（自引用），故 RwLock 需要 'static：
    // 进程级单例锁在此泄漏一个 Box（Windows 上仅含 File 句柄，
    // 约 16 字节），进程退出由 OS 回收，可接受。
    let lock: &'static mut fd_lock::RwLock<std::fs::File> =
        Box::leak(Box::new(fd_lock::RwLock::new(file)));

    match lock.try_write() {
        Ok(mut guard) => {
            // 先获锁再写身份：避免未获锁时截断覆盖持锁者的诊断内容
            // （否则冲突分支读到的会是「自己刚写的」而非持锁者的）
            write_lock_info(&mut guard)
                .with_context(|| format!("获取运行锁失败: {}", path.display()))?;
            Ok(RunLock { _lock: guard, path })
        }
        // 锁被占用（WouldBlock）：读锁文件定位持锁者，给出可操作报错
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(lock_conflict_error(&path)),
        Err(e) => Err(e).with_context(|| format!("获取运行锁失败: {}", path.display())),
    }
}

/// 带选项获取运行锁（Phase 15.2：--wait 轮询重试 / --skip-if-locked 跳过）
///
/// 语义（与 lib.rs::LockOptions 对齐）：
/// - 冲突（内核锁被占用）时若 `wait` 指定且未超时，sleep 100-200ms 后重试；
/// - 最终仍冲突：`skip_if_locked` 为 true 返回 `Skipped`（调用方以退出码 0
///   跳过本次操作，供 hook/CI 非阻塞拿锁），否则传播冲突报错；
/// - 其他 I/O 错误（打开/创建锁文件、写身份失败）不视为冲突，直接传播。
#[derive(Debug)]
pub enum LockAcquire {
    /// 成功获取运行锁（Drop 时释放）
    Acquired(RunLock),
    /// 冲突且 --skip-if-locked 命中：本次操作跳过
    Skipped,
}

/// 判断错误是否为「运行锁被占用」冲突（供 --wait/--skip-if-locked 轮询与
/// 跳过判定；其他错误直接传播）。锚定 lock_conflict_error 的「正在运行」
/// 消息标记（fs.rs 单测与 CLI 集成测试均依赖该文案）。
pub fn is_lock_conflict(err: &anyhow::Error) -> bool {
    err.to_string().contains("正在运行")
}

/// 带选项获取运行锁：冲突轮询等待/超时后跳过或报错（见 `LockAcquire`）
pub fn acquire_run_lock_with_options(
    config: &crate::config::schema::WikiConfig,
    lock: &crate::LockOptions,
) -> Result<LockAcquire> {
    let start = std::time::Instant::now();
    loop {
        match acquire_run_lock(config) {
            Ok(run_lock) => return Ok(LockAcquire::Acquired(run_lock)),
            Err(e) if is_lock_conflict(&e) => {
                // 冲突：有 wait 且未超时 → 轮询重试；否则按 skip 或报错
                let timed_out = match lock.wait {
                    Some(d) => start.elapsed() >= d,
                    None => true, // 未指定等待：立即超时
                };
                if timed_out {
                    if lock.skip_if_locked {
                        return Ok(LockAcquire::Skipped);
                    }
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Err(e) => return Err(e),
        }
    }
}

/// 覆盖写入锁文件持锁者身份（截断重写；仅获锁后调用，不会破坏他人锁文件）
fn write_lock_info(guard: &mut fd_lock::RwLockWriteGuard<'_, std::fs::File>) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let content = format!(
        "pid={}\nprocess={}\nstarted={}\n",
        std::process::id(),
        process_name(),
        chrono::Local::now().to_rfc3339()
    );
    let file: &mut std::fs::File = &mut *guard;
    file.set_len(0).context("截断锁文件失败")?;
    file.seek(SeekFrom::Start(0)).context("定位锁文件失败")?;
    file.write_all(content.as_bytes())
        .context("写入锁文件诊断内容失败")?;
    Ok(())
}

/// 当前可执行文件基名（诊断用）；获取失败退化为空串
fn process_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// 锁被占用时的报错：读取锁文件解析持锁者身份；内容缺失/损坏退化为 PID 未知
fn lock_conflict_error(path: &std::path::Path) -> anyhow::Error {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let pid = lock_field(&content, "pid");
    let process = lock_field(&content, "process");
    let started = lock_field(&content, "started");
    match (pid, process, started) {
        (Some(pid), Some(process), Some(started)) => anyhow::anyhow!(
            "获取运行锁失败: 另一 code-repo-wiki 实例正在运行（PID {pid}，进程 {process}，启动于 {started}，锁文件: {}）。该进程结束后可重试",
            path.display()
        ),
        _ => anyhow::anyhow!(
            "获取运行锁失败: 另一 code-repo-wiki 实例正在运行（PID 未知，锁文件: {}）。该进程结束后可重试",
            path.display()
        ),
    }
}

/// 提取锁文件诊断行 `key=value` 的 value；行不存在/值为空返回 None
fn lock_field(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix('=')?.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
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

    /// 锁可获取；获取后锁文件存在；Drop 后锁文件仍存在（常驻）；可再次获取
    #[test]
    fn test_run_lock_acquire_and_release() {
        let dir = temp_path("lock_roundtrip", "");
        let config = lock_config(&dir);
        let lock = acquire_run_lock(&config).unwrap();
        assert!(dir.join(".state/run.lock").exists());
        drop(lock);
        // 锁文件常驻：Drop 只关闭句柄释放内核锁，不删除文件
        assert!(dir.join(".state/run.lock").exists(), "锁文件应常驻");
        // 释放后可再次获取（幂等循环）
        let lock2 = acquire_run_lock(&config).unwrap();
        drop(lock2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁已存在时拒绝第二次获取，报错含持锁者指引与锁路径
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

    /// 获取锁后锁文件写入持锁者身份（供冲突实例报错定位）
    #[test]
    fn test_run_lock_writes_holder_info() {
        use std::io::{Read, Seek, SeekFrom};
        let dir = temp_path("lock_info", "");
        let config = lock_config(&dir);
        let mut lock = acquire_run_lock(&config).unwrap();
        // Windows 上 LockFileEx 锁定文件 offset 0 区域，其他句柄读该区域
        // 会返回 ERROR_LOCK_VIOLATION，因此经守卫句柄（同一句柄可读写
        // 锁定区域）读取验证内容
        let mut content = String::new();
        {
            let file: &mut std::fs::File = &mut *lock._lock;
            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_string(&mut content).unwrap();
        }
        assert!(content.contains("pid="), "锁文件应含 pid=: {content}");
        assert!(content.contains("process="), "锁文件应含 process=: {content}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== Phase 15.2 LockOptions 组合逻辑 ====================
    // 与集成测试互补：单测覆盖三种组合——wait 超时仍报错、skip 返回跳过、
    // wait 内让锁释放后成功（后台线程按短时序释放持锁）。跨进程冲突
    // 语义（CLI 级）由 tests/test_cli.rs 覆盖。

    /// skip_if_locked：无 wait 冲突时立即返回 Skipped（不等待）
    #[test]
    fn test_lock_options_skip_if_locked() {
        let dir = temp_path("lock_skip", "");
        let config = lock_config(&dir);
        let _first = acquire_run_lock(&config).unwrap();
        let options = crate::LockOptions { wait: None, skip_if_locked: true };
        let outcome = acquire_run_lock_with_options(&config, &options).unwrap();
        assert!(
            matches!(outcome, LockAcquire::Skipped),
            "冲突时应跳过: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// default（无 wait 无 skip）：冲突立即报错（既有行为不变）
    #[test]
    fn test_lock_options_default_conflict_errors() {
        let dir = temp_path("lock_default", "");
        let config = lock_config(&dir);
        let _first = acquire_run_lock(&config).unwrap();
        let err = acquire_run_lock_with_options(&config, &crate::LockOptions::default()).unwrap_err();
        assert!(err.to_string().contains("正在运行"), "应报冲突错误: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// wait 超时仍报错（持锁不放，等 300ms 后仍冲突 → 报错而非跳过/成功）
    #[test]
    fn test_lock_options_wait_timeout_errors() {
        let dir = temp_path("lock_wait_timeout", "");
        let config = lock_config(&dir);
        let _first = acquire_run_lock(&config).unwrap();
        let options = crate::LockOptions {
            wait: Some(std::time::Duration::from_millis(300)),
            skip_if_locked: false,
        };
        let err = acquire_run_lock_with_options(&config, &options).unwrap_err();
        assert!(err.to_string().contains("正在运行"), "超时仍应报冲突错误: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// wait + skip_if_locked：等待超时后仍冲突才跳过（非立即跳过）
    #[test]
    fn test_lock_options_wait_then_skip() {
        let dir = temp_path("lock_wait_skip", "");
        let config = lock_config(&dir);
        let _first = acquire_run_lock(&config).unwrap();
        let options = crate::LockOptions {
            wait: Some(std::time::Duration::from_millis(300)),
            skip_if_locked: true,
        };
        let outcome = acquire_run_lock_with_options(&config, &options).unwrap();
        assert!(
            matches!(outcome, LockAcquire::Skipped),
            "等待超时后仍冲突应跳过: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// wait 内让锁释放后成功：后台线程 300ms 后释放持锁，主线程 5s wait
    /// 内应获锁（非跳过/非报错）。fd-lock 内核锁同进程二次获取也会
    /// WouldBlock（test_run_lock_rejects_second 已证），因此单进程可测
    /// 「等待后释放→获锁」的时序路径。
    #[test]
    fn test_lock_options_wait_succeeds_after_release() {
        let dir = temp_path("lock_wait_success", "");
        let config = lock_config(&dir);
        let first = acquire_run_lock(&config).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(first);
        });
        let options = crate::LockOptions {
            wait: Some(std::time::Duration::from_secs(5)),
            skip_if_locked: false,
        };
        let outcome = acquire_run_lock_with_options(&config, &options);
        release.join().unwrap();
        match outcome {
            Ok(LockAcquire::Acquired(_)) => {}
            Ok(LockAcquire::Skipped) => panic!("wait 未超时应获锁而非跳过"),
            Err(e) => panic!("wait 未超时应获锁: {e}"),
        }
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
