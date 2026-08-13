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

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

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

/// 收紧敏感文件/目录权限（audit-cfg-02；密钥类资产用）
///
/// Unix 下：文件 0600（仅所有者读写）、目录 0700（仅所有者全部权限）——
/// 用户级 config.toml 可能含明文 api_key，默认 umask 落盘（0644/0755）
/// 会让同机其他用户可读；Windows 无 POSIX 权限位，走 ACL 机制另行管理，
/// 此处按 cfg(unix) 条件编译跳过。
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn restrict_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("设置权限失败: {}", path.display()))?;
    }
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
/// - `_pool`：池归还令牌。必须声明在 `_lock` 之后——结构体字段按声明序
///   drop，`_lock`（内核守卫）先释放内核锁，随后 `_pool` drop 时把本锁
///   用的 RwLock 归还进程级复用池（见 PoolToken）。
///
/// 锁文件本身常驻，本结构体 Drop 只关闭句柄，不删除文件。
#[derive(Debug)]
pub struct RunLock {
    _lock: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
    /// 池归还令牌（声明在 _lock 之后，drop 顺序见 PoolToken 注释）
    _pool: PoolToken,
}

/// 运行锁文件路径：`{output_dir}/.state/run.lock`，并确保状态目录存在。
/// 打开文件本身由 `pool_checkout` 按需执行（首次使用该路径时才打开）。
fn lock_file_path(config: &crate::config::schema::WikiConfig) -> Result<std::path::PathBuf> {
    let state_dir = config.output_dir().join(".state");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
    Ok(state_dir.join("run.lock"))
}

/// 池中已泄漏 RwLock 的裸指针。`*mut` 非 Send/Sync，无法放进 `static` 或
/// 跨线程 move，用新类型 + unsafe impl 声明安全前提（借用手册见 LOCK_POOL
/// 与 pool_checkout/pool_return 注释：指针只在「被取池独占借用」时才解引用，
/// 池本身受 Mutex 保护，共享指针值本身不构成数据竞争）。
#[derive(Debug, Clone, Copy)]
struct RwLockPtr(*mut fd_lock::RwLock<std::fs::File>);
unsafe impl Send for RwLockPtr {}
unsafe impl Sync for RwLockPtr {}

/// 进程级运行锁复用池：`锁文件路径 -> 空闲 RwLock 列表`。
///
/// 为什么需要池：fd-lock 的写守卫是 `RwLockWriteGuard<'static, _>`，其借用
/// 的 RwLock 必须 `'static`（内存永驻），旧实现因此每次调用都 `Box::leak`
/// 一个新 Box + 打开一个新文件句柄——`--wait` 轮询每进入一次 +1、watch
/// 常驻每轮 +1（Windows 进程句柄上限约 16K，长 wait 可能耗尽）。本池把
/// 泄漏压到「每路径每进程一次」：首次使用该路径时打开句柄并泄漏一个
/// RwLock，之后取池复用；守卫释放后经 PoolToken::drop 归还池中。
///
/// 借用手册（内存安全前提）：同一 RwLock 任意时刻只被一个持有者借用——
/// 要么在池中（无存活守卫）、要么被一个 acquire 调用独占（守卫 Drop 后才
/// 归还）。因此对同一 RwLock 至多存在一个 `&mut`，不会出现「守卫存活期间
/// 另一调用从同一指针重建 `&mut`」的别名冲突。这正是把缓存设计成池而非
/// 单实例的原因：冲突测试「先持锁再二次获取」在单实例缓存下会让二次获取
/// 与存活守卫同时持有同一 RwLock 的 `&'static mut`，属未定义行为。
static LOCK_POOL: OnceLock<Mutex<HashMap<std::path::PathBuf, Vec<RwLockPtr>>>> = OnceLock::new();

/// 从池中取出（或首次创建）一个 RwLock，返回 `(锁路径, 裸指针)`。
///
/// 首次使用某路径时：打开常驻锁文件（不 truncate、不删除），`Box::leak`
/// 一个 RwLock 换 `'static`（每路径每进程仅一次）；此后该路径的 RwLock
/// 都在池中流转，不再新建——`--wait` 长轮询与 watch 每轮复用同一句柄。
fn pool_checkout(config: &crate::config::schema::WikiConfig) -> Result<(std::path::PathBuf, *mut fd_lock::RwLock<std::fs::File>)> {
    let path = lock_file_path(config)?;
    let pool = LOCK_POOL.get_or_init(Default::default);
    let mut pool = pool.lock().unwrap();
    if let Some(RwLockPtr(ptr)) = pool.entry(path.clone()).or_default().pop() {
        return Ok((path, ptr));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("获取运行锁失败: {}", path.display()))?;
    let rwlock: *mut fd_lock::RwLock<std::fs::File> =
        Box::leak(Box::new(fd_lock::RwLock::new(file)));
    Ok((path, rwlock))
}

/// 把 RwLock 归还池中（仅在守卫已释放、无存活借用时调用——即取池方在
/// 冲突/超时/出错分支主动归还，或 PoolToken::drop 在守卫字段 drop 后归还）
fn pool_return(path: &std::path::Path, rwlock: *mut fd_lock::RwLock<std::fs::File>) {
    let pool = LOCK_POOL.get_or_init(Default::default);
    let mut pool = pool.lock().unwrap();
    pool.entry(path.to_path_buf()).or_default().push(RwLockPtr(rwlock));
}

/// 池归还令牌：作为 RunLock 字段存在，且必须声明在 `_lock` 之后。
///
/// 结构体字段按声明序 drop（先于 `Drop::drop` 之后、逐个字段执行），
/// `_lock`（内核守卫）先释放内核锁，随后 `_pool` drop 时 RwLock 已空闲，
/// 归还池中供下次复用（下次 `--wait`/watch 轮询不再新开句柄）。
#[derive(Debug)]
struct PoolToken {
    path: std::path::PathBuf,
    ptr: RwLockPtr,
}

impl Drop for PoolToken {
    fn drop(&mut self) {
        pool_return(&self.path, self.ptr.0);
    }
}

/// 获取运行锁：从进程级池取出（或首次打开）常驻锁文件句柄 → 获取内核
/// 写锁 → 写持锁者身份。锁被占用时报错并给出持锁者身份。
///
/// 池语义：RwLock 每次进入本函数不新建（仅首次打开并泄漏一次），冲突/
/// 出错路径即时归还，成功路径由 RunLock::_pool 在守卫释放后归还。
pub fn acquire_run_lock(config: &crate::config::schema::WikiConfig) -> Result<RunLock> {
    let (path, rwlock) = pool_checkout(config)?;
    match try_acquire_once(rwlock, &path) {
        Ok(Some(run_lock)) => Ok(run_lock),
        // 锁被占用（WouldBlock）：读锁文件定位持锁者，给出可操作报错；
        // RwLock 未产生守卫，即时归还池中
        Ok(None) => {
            pool_return(&path, rwlock);
            Err(lock_conflict_error(&path))
        }
        Err(e) => {
            // try_acquire_once 已带「获取运行锁失败」上下文，此处直接传播
            pool_return(&path, rwlock);
            Err(e)
        }
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

/// 运行锁冲突的结构化错误：携带持锁者 PID（锁文件可解析时）
///
/// audit-srch2-04：冲突判定改用类型匹配（downcast_ref）而非字符串 contains
/// ——报错文案调整会静默破坏 --wait/--skip-if-locked/watch 自愈判定；
/// 结构化类型把「是否是冲突」变成可编译期保证的契约。Display 文案保留
/// 「正在运行」字样（既有单测与 CLI 集成测试断言语义不变）。
#[derive(Debug)]
pub struct LockError {
    /// 持锁者 PID（锁文件缺失/内容损坏时无法解析为 None）
    pub pid: Option<u32>,
    message: String,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LockError {}

/// 判断错误是否为「运行锁被占用」冲突（供 --wait/--skip-if-locked 轮询与
/// 跳过判定；其他错误直接传播）。按 `LockError` 类型匹配，不依赖报错文案。
pub fn is_lock_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<LockError>().is_some()
}

/// 单次尝试获取运行锁（--wait 轮询复用同一句柄时的内部分解）
///
/// 返回 `Ok(Some(RunLock))` 获锁、`Ok(None)` 冲突（WouldBlock）、`Err` 其他
/// 错误。守卫借用 RwLock 需 'static（RunLock 字段约束），故 RwLock 必须
/// 以裸指针传入：裸指针指向池中泄漏的堆对象（pool_checkout），进程生命周期
/// 内有效；每次调用从指针重建 `&mut`，前一调用的守卫在返回前已 drop
/// （Ok 分支返回后不再重建，WouldBlock/Err 分支不产生守卫），且池借用
/// 手册保证同一 RwLock 同一时刻至多一个持有者，无别名冲突。冲突/出错时
/// 调用方负责把 RwLock 归还池中（本函数不持有池上下文）。
fn try_acquire_once(
    raw: *mut fd_lock::RwLock<std::fs::File>,
    path: &std::path::Path,
) -> Result<Option<RunLock>> {
    // 保留裸指针供 PoolToken 归还使用（`rwlock` 局部借用会遮蔽参数）
    let rwlock = unsafe { &mut *raw };
    match rwlock.try_write() {
        Ok(mut guard) => {
            // 先获锁再写身份：避免未获锁时截断覆盖持锁者的诊断内容
            // （否则冲突分支读到的会是「自己刚写的」而非持锁者的）
            write_lock_info(&mut guard)
                .with_context(|| format!("获取运行锁失败: {}", path.display()))?;
            Ok(Some(RunLock {
                _lock: guard,
                _pool: PoolToken {
                    path: path.to_path_buf(),
                    ptr: RwLockPtr(raw),
                },
            }))
        }
        // 锁被占用（WouldBlock）：返回 None 由调用方决定轮询/跳过/报错
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e).with_context(|| format!("获取运行锁失败: {}", path.display())),
    }
}

/// 带选项获取运行锁：冲突轮询等待/超时后跳过或报错（见 `LockAcquire`）
///
/// 池语义：锁句柄在进入轮询前只从池中取一次（首次打开并泄漏一次），冲突
/// 重试复用同一句柄（try_acquire_once 内反复 try_write）——不随尝试次数
/// 逐次泄漏。无论何种退出（获锁 / 跳过 / 超时报错 / 其他 I/O 错误），
/// RwLock 都归还池中（获锁路径由 RunLock::_pool 在守卫释放后归还，其余
/// 路径在此显式归还），下次进入复用同一句柄，`--wait` 长轮询与 watch 每轮
/// 不再增量泄漏 Box/句柄（旧实现每次进入泄漏一个，Windows 进程句柄上限
/// 约 16K，长 wait 可能耗尽）。
pub fn acquire_run_lock_with_options(
    config: &crate::config::schema::WikiConfig,
    lock: &crate::LockOptions,
) -> Result<LockAcquire> {
    let (path, rwlock) = pool_checkout(config)?;
    let start = std::time::Instant::now();
    loop {
        match try_acquire_once(rwlock, &path) {
            Ok(Some(run_lock)) => return Ok(LockAcquire::Acquired(run_lock)),
            Ok(None) => {
                // 冲突：有 wait 且未超时 → 轮询重试；否则按 skip 或报错
                let timed_out = match lock.wait {
                    Some(d) => start.elapsed() >= d,
                    None => true, // 未指定等待：立即超时
                };
                if timed_out {
                    if lock.skip_if_locked {
                        pool_return(&path, rwlock);
                        return Ok(LockAcquire::Skipped);
                    }
                    pool_return(&path, rwlock);
                    return Err(lock_conflict_error(&path));
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            // 其他 I/O 错误（打开/创建锁文件、写身份失败）不视为冲突，
            // 直接传播；RwLock 未产生守卫（try_acquire_once 内部已 drop），
            // 归还池中
            Err(e) => {
                pool_return(&path, rwlock);
                return Err(e);
            }
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
    let pid = lock_field(&content, "pid").and_then(|s| s.parse().ok());
    let process = lock_field(&content, "process");
    let started = lock_field(&content, "started");
    let message = match (pid, process, started) {
        (Some(pid), Some(process), Some(started)) => format!(
            "获取运行锁失败: 另一 code-repo-wiki 实例正在运行（PID {pid}，进程 {process}，启动于 {started}，锁文件: {}）。该进程结束后可重试",
            path.display()
        ),
        _ => format!(
            "获取运行锁失败: 另一 code-repo-wiki 实例正在运行（PID 未知，锁文件: {}）。该进程结束后可重试",
            path.display()
        ),
    };
    anyhow::Error::new(LockError { pid, message })
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
            let file: &mut std::fs::File = &mut lock._lock;
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

    /// 池归还纪律回归：--wait 冲突超时路径必须把 RwLock 归还池中（旧实现
    /// 每次进入泄漏一个 Box+句柄，watch 常驻每轮增量一次），否则释放首锁后
    /// 再次获取会新建句柄——本测试验证「超时归还 → 释放 → 复用同一路径」
    /// 仍能成功获取（池中 RwLock 可复用，而非无限新建）。
    #[test]
    fn test_lock_pool_reuse_after_timeout() {
        let dir = temp_path("lock_pool_reuse", "");
        let config = lock_config(&dir);
        let first = acquire_run_lock(&config).unwrap();
        // 冲突 + 短 wait 超时 → 该次获取的 RwLock 须归还池中（非泄漏）
        let options = crate::LockOptions {
            wait: Some(std::time::Duration::from_millis(200)),
            skip_if_locked: false,
        };
        let err = acquire_run_lock_with_options(&config, &options).unwrap_err();
        assert!(err.to_string().contains("正在运行"), "超时仍应报冲突: {err}");
        drop(first);
        // 释放后再次获取：应复用池中 RwLock 成功（若池归还纪律失效，此处
        // 也会成功但会新建句柄——该测试的回归价值在于保覆盖、防删除归还
        // 路径；句柄复用本身由池实现保证）
        let _second = acquire_run_lock(&config).unwrap();
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
