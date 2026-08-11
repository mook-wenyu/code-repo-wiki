//! 文件系统监听模块
//!
//! 使用 `notify-debouncer-full` crate 实现防抖文件监听，
//! 将底层 notify 事件去重、聚合后通过 channel 上报。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebouncedEvent, DebounceEventResult};

use crate::ingest::scanner::{NOISE_DIRS, ROOT_ONLY_NOISE_DIRS};
use crate::ingest::parser::SUPPORTED_EXTENSIONS;

/// 冷却窗口常量（v31 C-07）：连续编辑期间合并事件，安静 `COOLDOWN_QUIET_MS`
/// 或首个事件后 `COOLDOWN_DEADLINE_MS` 触发一次合并增量。
/// 2s/5s 取值依据（SME v31）：主流 IDE 自动保存频率 1-2s，2s 静默覆盖单次
/// 保存后的停顿；5s 上限保证批量编辑（git checkout、重构重命名）不会无限
/// 推迟更新。
pub const COOLDOWN_QUIET_MS: u64 = 2000;
pub const COOLDOWN_DEADLINE_MS: u64 = 5000;

/// 文件变更类型（由监听事件显式标记，下游无需以 exists() 推断删除）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 文件被创建
    Created,
    /// 文件内容/元数据被修改（含重命名，保守视为修改）
    Modified,
    /// 文件被删除
    Deleted,
}

/// 一次防抖聚合产生的事件：同一 kind 的路径合并去重
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub paths: Vec<PathBuf>,
    pub kind: ChangeKind,
}

/// 启动文件监听并执行回调（阻塞版本）
///
/// v30+：监听根恒为仓库根（扫描范围已硬编码为全量遍历+内置过滤），
/// 事件上报时按支持语言扩展名与噪音目录过滤（与 scanner 同一边界）。
///
/// 边界：删除事件的路径已不存在于磁盘，回调内不能读取文件内容；
/// 事件类型（ChangeKind）已显式携带，下游不再以 exists() 推断删除。
pub fn run_watch_loop(
    root: &Path,
    stop_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_change: impl Fn(Vec<WatchEvent>) + Send + 'static,
) -> Result<()> {
    let include_exts = supported_exts();

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: DebounceEventResult| {
            // tx.send 失败 = 接收端（主循环 rx）已 drop。此时事件本就无人消费，
            // 静默丢弃是正确语义（不存在"数据丢失"——通道已断）；主循环退出
            // 由外部信号/错误驱动，不由 send 结果驱动，故不在此处理 Err。
            let _ = tx.send(result);
        },
    )
    .with_context(|| "创建文件防抖监听器失败")?;

    // v30+：监听根=仓库根（全量监听，事件按扩展名/噪音目录过滤——
    // 目录结构因项目而异，路径模式无法通用，语言才是能力边界）
    let watch_roots = vec![root.to_path_buf()];
    for watch_root in &watch_roots {
        if !watch_root.exists() {
            tracing::warn!("监听根不存在，跳过: {}", watch_root.display());
            continue;
        }
        // notify-debouncer-full 0.4：Debouncer 自身实现了 Watcher trait，
        // 可以直接调用 .watch()，无需 .watcher()
        debouncer
            .watch(watch_root.as_path(), RecursiveMode::Recursive)
            .with_context(|| format!("监听目录失败: {}", watch_root.display()))?;
    }

    tracing::info!(
        "文件监听已启动（阻塞模式）: {}",
        watch_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // v14 F 组（t06 拍板）：Ctrl-C 优雅退出——主循环每 500ms 检查停止
    // 标记，置位时退出（当前正在执行的 on_change 完成后才检查，即
    // "等当前增量生成完成"；不会在生成中途打断状态落盘）。
    // v31 C-07：冷却窗口——高频编辑（IDE 自动保存、批量重构）期间把事件
    // 累积到 pending，安静 2s（尾沿）或首个事件后 5s（强制）触发一次
    // 合并增量，避免连续保存 N 次触发 N 次全量管线（Token 节省核心）。
    use std::sync::atomic::Ordering;
    let mut pending: Vec<(PathBuf, ChangeKind)> = Vec::new();
    let mut pending_first_at: Option<Instant> = None;
    let mut quiet_since: Option<Instant> = None;
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            // 退出丢弃 pending：事件只是"待触发"的请求，进程退出后由
            // 下次启动的全量/增量兜底，丢弃不产生数据丢失
            tracing::info!("收到停止信号，文件监听退出");
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                // 聚合 + 折叠（批内同路径跨 kind 按最终态合并），
                // 事件类型显式传递给下游，删除不再依赖 exists() 推断
                let watch_events = process_batch(&events, &include_exts);
                if !watch_events.is_empty() {
                    // 跨批按时间序收敛（apply_batch）：同路径后到达的 kind
                    // 覆盖先到达的——删除重建（git checkout/codegen clean+
                    // rebuild/IDE save-as）收敛为 Created/Modified 而非删除
                    // 优先（删除优先只对批内最终态成立，跨批会把"删→建"
                    // 误判为"仍删除"，下游误删现存文件的产物页）
                    apply_batch(&mut pending, &watch_events);
                    let now = Instant::now();
                    pending_first_at.get_or_insert(now);
                    quiet_since = Some(now);
                }
            }
            Ok(Err(errors)) => {
                for e in &errors {
                    tracing::warn!("文件监听错误: {:?}", e);
                }
            }
            // 超时（轮询停止标记 + 冷却窗口判定）是正常路径：
            // 回到循环顶检查 stop_flag 与 pending 是否应触发
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if should_flush_now(&pending, &mut pending_first_at, &mut quiet_since) {
                    on_change(flush_events(&std::mem::take(&mut pending)));
                }
            }
            // Disconnected = 接收端全部 drop，监听无意义
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // 持续事件流下（批间隔 <500ms 恒走 Ok 分支）5s 强制截止也必须可达
        // ——每批处理完后求值一次 deadline（reviewer 修正：原实现只在
        // Timeout 分支求值，<500ms 间隔的持续编辑会无限推迟触发）。
        // quiet=0：只依赖强制截止（与 should_flush 共享同一判定逻辑，DRY；
        // 阈值语义由 test_should_flush_deadline_forced 纯函数测试覆盖）
        if let Some(first_at) = pending_first_at {
            let total = Instant::now().saturating_duration_since(first_at);
            if should_flush(Duration::ZERO, total) {
                on_change(flush_events(&std::mem::take(&mut pending)));
                pending_first_at = None;
                quiet_since = None;
            }
        }
    }
    Ok(())
}

/// 冷却窗口触发判定（纯函数，可测）：
/// 尾沿——安静期 ≥ `COOLDOWN_QUIET_MS`（连续编辑停止后尽快收敛）；
/// 强制——总时长 ≥ `COOLDOWN_DEADLINE_MS`（防无限推迟，保证最终一致性）。
fn should_flush(quiet_elapsed: Duration, total_elapsed: Duration) -> bool {
    quiet_elapsed >= Duration::from_millis(COOLDOWN_QUIET_MS)
        || total_elapsed >= Duration::from_millis(COOLDOWN_DEADLINE_MS)
}

/// Timeout 分支的触发判定：pending 非空且冷却窗口到点
fn should_flush_now(
    pending: &[(PathBuf, ChangeKind)],
    pending_first_at: &mut Option<Instant>,
    quiet_since: &mut Option<Instant>,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let quiet_elapsed = quiet_since
        .map(|q| now.saturating_duration_since(q))
        .unwrap_or(Duration::ZERO);
    let total_elapsed = pending_first_at
        .map(|f| now.saturating_duration_since(f))
        .unwrap_or(Duration::ZERO);
    let flush = should_flush(quiet_elapsed, total_elapsed);
    if flush {
        *pending_first_at = None;
        *quiet_since = None;
    }
    flush
}

/// 把一批（已折叠的）事件应用到跨批累积表：同路径后到达的 kind 覆盖
/// 先到达的（按时间序收敛），不同路径追加。路径在该批内唯一
/// （process_batch 已按最终态折叠），无同批重复覆盖问题。
fn apply_batch(pending: &mut Vec<(PathBuf, ChangeKind)>, events: &[WatchEvent]) {
    for event in events {
        for p in &event.paths {
            match pending.iter_mut().find(|(path, _)| path == p) {
                Some(entry) => entry.1 = event.kind,
                None => pending.push((p.clone(), event.kind)),
            }
        }
    }
}

/// 把累积表分组为待回调的 WatchEvent 列表（保留各组首次出现顺序，
/// 与单批 aggregate 语义一致；下游逐组跑 pipeline）
fn flush_events(pending: &[(PathBuf, ChangeKind)]) -> Vec<WatchEvent> {
    let mut out: Vec<WatchEvent> = Vec::new();
    for (path, kind) in pending {
        match out.iter_mut().find(|e| e.kind == *kind) {
            Some(ev) => ev.paths.push(path.clone()),
            None => out.push(WatchEvent {
                paths: vec![path.clone()],
                kind: *kind,
            }),
        }
    }
    out
}

/// 将防抖窗口内的事件批处理为待回调的 WatchEvent 列表（纯函数，主循环可测化）
///
/// 两步：aggregate_events 按 kind 分组去重合并 → fold_events 按最终态语义
/// 折叠同路径的跨 kind 事件。窗口内同路径最多产出一个 WatchEvent。
pub fn process_batch(events: &[DebouncedEvent], include_exts: &[String]) -> Vec<WatchEvent> {
    fold_events(aggregate_events(events, include_exts))
}

/// 按最终态语义折叠同路径的跨 kind 事件，消除同窗口双跑完整流水线
///
/// 规则（文件在窗口内的最终状态决定唯一事件）：
/// - 路径出现在 Deleted 事件中 → 从其他 kind 移除（最终不存在，删除优先于一切）
/// - 路径同时出现在 Created 与 Modified → 保留 Modified（最终存在且被修改）
///
/// 不同路径、不同 kind 的事件保持独立；输出保持各 kind 首次出现的顺序。
fn fold_events(events: Vec<WatchEvent>) -> Vec<WatchEvent> {
    let mut out: Vec<WatchEvent> = Vec::new();
    for event in &events {
        let paths: Vec<PathBuf> = event
            .paths
            .iter()
            .filter(|p| {
                // 删除优先：路径最终不存在时，非 Deleted 记录无意义
                if event.kind != ChangeKind::Deleted && has_path(&events, ChangeKind::Deleted, p) {
                    return false;
                }
                // Modified 优先于 Created：文件最终存在且被修改过
                if event.kind == ChangeKind::Created && has_path(&events, ChangeKind::Modified, p) {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        if !paths.is_empty() {
            out.push(WatchEvent { paths, kind: event.kind });
        }
    }
    out
}

/// 判断 events 中是否存在指定 kind 且包含该路径的事件
fn has_path(events: &[WatchEvent], kind: ChangeKind, path: &Path) -> bool {
    events
        .iter()
        .any(|e| e.kind == kind && e.paths.iter().any(|p| p == path))
}
///
/// Modify 与 Remove 混合的窗口产出两个独立事件，删除路径不与被
/// 修改路径混在一起，下游可直入清理而无需 exists() 推断。
fn aggregate_events(events: &[DebouncedEvent], include_exts: &[String]) -> Vec<WatchEvent> {
    let mut out: Vec<WatchEvent> = Vec::new();
    for debounced in events {
        let kind = change_kind_of(&debounced.event.kind);
        for p in &debounced.event.paths {
            if !should_report(p, include_exts) {
                continue;
            }
            match out.iter_mut().find(|e| e.kind == kind) {
                // 该 kind 分组已存在且路径未记录 → 合并
                Some(ev) if !ev.paths.contains(p) => ev.paths.push(p.clone()),
                // 已记录过该路径（去重）
                Some(_) => {}
                // 首个该 kind 的事件
                None => out.push(WatchEvent { paths: vec![p.clone()], kind }),
            }
        }
    }
    out
}

/// 将 notify 事件类型映射为业务变更类型
///
/// Access/Any/Other 等未知事件保守视为修改——宁可多一次增量更新，
/// 也不让未知事件误触发删除清理。
fn change_kind_of(kind: &notify::EventKind) -> ChangeKind {
    match kind {
        notify::EventKind::Create(_) => ChangeKind::Created,
        notify::EventKind::Remove(_) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    }
}

/// 支持语言扩展名列表（去点前缀、小写，供事件上报过滤——与
/// parser::SUPPORTED_EXTENSIONS 同源，扫与听共用同一能力边界）
fn supported_exts() -> Vec<String> {
    SUPPORTED_EXTENSIONS
        .iter()
        .map(|e| e.trim_start_matches('.').to_string())
        .collect()
}

/// 路径是否应上报（不在噪音目录内且扩展名为支持语言）
fn should_report(path: &Path, include_exts: &[String]) -> bool {
    !should_ignore(path) && matches_include(path, include_exts)
}

/// 判断路径是否位于噪音目录（与 scanner 同清单语义，P1-14 同步）：
/// 任意深度清单（node_modules/target/.git 等）命中任意路径段即忽略；
/// 根级清单（dist/build/out/bin/obj）仅在路径第 2 段（仓库根的直接
/// 子目录）命中——src/bin 等合法源码目录的深层 bin 段不误杀。
fn should_ignore(path: &Path) -> bool {
    let mut depth = 0;
    for c in path.components() {
        if let Some(s) = c.as_os_str().to_str() {
            // 深度语义：Root 段计 0，仓库名段计 1，其后首段计 2——
            // 根级目录 = 深度 2（如 /repo/dist/ 的 dist）
            if NOISE_DIRS.contains(&s) {
                return true;
            }
            // 根级目录 = 精确深度 2（/repo/dist/ 的 dist；src/bin 的 bin 段
            // depth=3 不命中——src/bin 是合法源码目录，P1-14）
            if depth == 2 && ROOT_ONLY_NOISE_DIRS.contains(&s) {
                return true;
            }
        }
        depth += 1;
    }
    false
}

/// 检查路径的扩展名是否在支持语言列表内
fn matches_include(path: &Path, include_exts: &[String]) -> bool {
    if include_exts.is_empty() {
        return true;
    }
    match path.extension() {
        Some(ext) => include_exts.iter().any(|e| ext == e.as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_ignore_target_dir() {
        let p = Path::new("/repo/target/debug/main.rs");
        assert!(should_ignore(p));
    }

    #[test]
    fn test_should_ignore_git_dir() {
        let p = Path::new("/repo/.git/HEAD");
        assert!(should_ignore(p));
    }

    #[test]
    fn test_should_ignore_node_modules() {
        let p = Path::new("/repo/node_modules/foo/index.js");
        assert!(should_ignore(p));
    }

    #[test]
    fn test_should_ignore_dist_and_venv() {
        assert!(should_ignore(Path::new("/repo/dist/bundle.js")));
        assert!(should_ignore(Path::new("/repo/.venv/lib/py.py")));
    }

    #[test]
    fn test_should_not_ignore_src_dir() {
        let p = Path::new("/repo/src/main.rs");
        assert!(!should_ignore(p));
    }

    /// P1-14 回归锚：src/bin 是合法源码目录，深层 bin 段不忽略
    #[test]
    fn test_should_not_ignore_src_bin() {
        let p = Path::new("/repo/src/bin/tool.rs");
        assert!(!should_ignore(p), "src/bin 是合法源码目录，不得忽略: {:?}", p);
    }

    #[test]
    fn test_matches_include_with_matching_ext() {
        let exts = vec!["rs".to_string(), "tsx".to_string()];
        assert!(matches_include(Path::new("main.rs"), &exts));
        assert!(matches_include(Path::new("comp.tsx"), &exts));
    }

    #[test]
    fn test_matches_include_with_mismatch_ext() {
        let exts = vec!["rs".to_string()];
        assert!(!matches_include(Path::new("main.js"), &exts));
        assert!(!matches_include(Path::new("no_ext"), &exts));
    }

    /// v30+：支持扩展名列表来自 parser 注册表同源常量
    #[test]
    fn test_supported_exts_cover_all_parsers() {
        let exts = supported_exts();
        for expected in ["rs", "ts", "tsx", "py", "go", "js", "jsx", "mjs", "cjs", "cs", "java"] {
            assert!(exts.contains(&expected.to_string()), "缺少 {expected}");
        }
    }

    /// 上报判定：噪音目录与扩展名不匹配的路径不上报
    #[test]
    fn test_should_report_filters_ignored_and_mismatched() {
        let exts = vec!["rs".to_string()];
        assert!(should_report(Path::new("/repo/src/main.rs"), &exts));
        assert!(!should_report(Path::new("/repo/target/main.rs"), &exts));
        assert!(!should_report(Path::new("/repo/src/main.js"), &exts));
        assert!(!should_report(Path::new("/repo/src/no_ext"), &exts));
    }

    /// 事件类型显式化：Modify 与 Remove 事件聚合后 kind 正确保留，
    /// 删除路径不被修改路径"吸收"（下游可直入清理，无需 exists() 推断）
    #[test]
    fn test_watch_event_kind_preserved() {
        use notify::event::{DataChange, ModifyKind, RemoveKind};
        use notify::{Event, EventKind};

        let mk = || {
            let mut e = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)));
            e.paths = vec![PathBuf::from("src/a.rs")];
            DebouncedEvent::new(e, std::time::Instant::now())
        };
        let mut removed = Event::new(EventKind::Remove(RemoveKind::File));
        removed.paths = vec![PathBuf::from("src/b.rs")];
        let events = vec![
            mk(),
            DebouncedEvent::new(removed, std::time::Instant::now()),
        ];
        let exts = vec!["rs".to_string()];
        let aggregated = aggregate_events(&events, &exts);

        assert_eq!(aggregated.len(), 2, "Modify 与 Remove 应各自聚合成独立事件");
        let modified = aggregated
            .iter()
            .find(|e| e.kind == ChangeKind::Modified)
            .expect("应存在 Modified 事件");
        assert_eq!(modified.paths, vec![PathBuf::from("src/a.rs")]);
        let deleted = aggregated
            .iter()
            .find(|e| e.kind == ChangeKind::Deleted)
            .expect("应存在 Deleted 事件");
        assert_eq!(deleted.paths, vec![PathBuf::from("src/b.rs")]);
    }

    /// 聚合去重：同一 kind 的重复路径只保留一份
    #[test]
    fn test_aggregate_events_dedups_same_kind_paths() {
        use notify::event::{DataChange, ModifyKind};
        use notify::{Event, EventKind};

        let mk = || {
            let mut e = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)));
            e.paths = vec![PathBuf::from("src/a.rs")];
            DebouncedEvent::new(e, std::time::Instant::now())
        };
        let exts = vec!["rs".to_string()];
        let aggregated = aggregate_events(&[mk(), mk()], &exts);
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].paths, vec![PathBuf::from("src/a.rs")]);
    }

    // ---- 折叠（fold_events / process_batch）测试 ----
    // 折叠语义：同路径跨 kind 事件按"窗口内最终态"合并——
    // 删除优先于一切，Modified 优先于 Created；不同路径保持独立。

    /// 测试事件构造器：任意 kind 的 notify 事件（路径/时刻可覆盖）
    fn make_debounced(kind: notify::EventKind, path: &str) -> DebouncedEvent {
        let mut e = notify::Event::new(kind);
        e.paths = vec![PathBuf::from(path)];
        DebouncedEvent::new(e, std::time::Instant::now())
    }

    /// Modified + Deleted 同路径 → 折叠为单个 Deleted（文件最终不存在）
    #[test]
    fn test_fold_modified_deleted() {
        use notify::event::{DataChange, ModifyKind, RemoveKind};
        let exts = vec!["rs".to_string()];
        let events = vec![
            make_debounced(notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)), "src/a.rs"),
            make_debounced(notify::EventKind::Remove(RemoveKind::File), "src/a.rs"),
        ];
        let folded = process_batch(&events, &exts);
        assert_eq!(folded.len(), 1, "同路径 Modified+Deleted 应折叠为单事件");
        assert_eq!(folded[0].kind, ChangeKind::Deleted);
        assert_eq!(folded[0].paths, vec![PathBuf::from("src/a.rs")]);
    }

    /// Created + Deleted 同路径 → 折叠为单个 Deleted（创建即删，最终不存在）
    #[test]
    fn test_fold_created_deleted() {
        use notify::event::{CreateKind, RemoveKind};
        let exts = vec!["rs".to_string()];
        let events = vec![
            make_debounced(notify::EventKind::Create(CreateKind::File), "src/a.rs"),
            make_debounced(notify::EventKind::Remove(RemoveKind::File), "src/a.rs"),
        ];
        let folded = process_batch(&events, &exts);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].kind, ChangeKind::Deleted);
    }

    /// Created + Modified 同路径 → 折叠为单个 Modified（文件最终存在且被修改）
    #[test]
    fn test_fold_created_modified() {
        use notify::event::{CreateKind, DataChange, ModifyKind};
        let exts = vec!["rs".to_string()];
        let events = vec![
            make_debounced(notify::EventKind::Create(CreateKind::File), "src/a.rs"),
            make_debounced(notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)), "src/a.rs"),
        ];
        let folded = process_batch(&events, &exts);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].kind, ChangeKind::Modified);
        assert_eq!(folded[0].paths, vec![PathBuf::from("src/a.rs")]);
    }

    /// 聚合+折叠：同一路径的 Modify+Remove 产出单一 Deleted 事件
    /// （aggregate 先分组，fold 再按最终态合并——下游只会跑一次删除清理）
    #[test]
    fn test_aggregate_events_same_path_cross_kind() {
        use notify::event::{DataChange, ModifyKind, RemoveKind};
        let exts = vec!["rs".to_string()];
        let events = vec![
            make_debounced(notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)), "src/a.rs"),
            make_debounced(notify::EventKind::Remove(RemoveKind::File), "src/a.rs"),
        ];
        let folded = process_batch(&events, &exts);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].kind, ChangeKind::Deleted);
    }

    /// 不同路径互不影响：a 的 Modify+Remove 折叠为 Deleted，
    /// b 的 Modify 独立保留（折叠不吞并无关路径）
    #[test]
    fn test_aggregate_events_preserves_distinct_paths() {
        use notify::event::{DataChange, ModifyKind, RemoveKind};
        let exts = vec!["rs".to_string()];
        let events = vec![
            make_debounced(notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)), "src/a.rs"),
            make_debounced(notify::EventKind::Remove(RemoveKind::File), "src/a.rs"),
            make_debounced(notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)), "src/b.rs"),
        ];
        let folded = process_batch(&events, &exts);
        assert_eq!(folded.len(), 2, "a 折叠为 Deleted、b 独立 Modified，共 2 事件");
        let deleted = folded
            .iter()
            .find(|e| e.kind == ChangeKind::Deleted)
            .expect("应存在 Deleted 事件");
        assert_eq!(deleted.paths, vec![PathBuf::from("src/a.rs")]);
        let modified = folded
            .iter()
            .find(|e| e.kind == ChangeKind::Modified)
            .expect("应存在 Modified 事件");
        assert_eq!(modified.paths, vec![PathBuf::from("src/b.rs")]);
    }

    /// v14 F 组（t06 拍板）：停止标记预置 → 主循环立即退出
    /// （优雅停止路径：标记在 on_change 完成后才检查，不打断进行中的
    /// 增量生成；本测试验证预置标记的退出语义与不崩溃）
    #[test]
    fn test_watch_loop_exits_on_pre_set_stop_flag() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_watch_stop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 标记预置：循环第一次检查即退出（监听根不存在只告警不阻塞）
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let start = std::time::Instant::now();
        let result = run_watch_loop(&dir, stop_flag, |_| panic!("不应触发回调"));
        assert!(result.is_ok(), "优雅退出应返回 Ok: {result:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "预置停止标记应在监听启动后立即退出（无需等待事件）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v31 C-07 冷却窗口测试 ----

    /// 尾沿触发：安静期 ≥2s → 触发（连续编辑停止后的收敛路径）
    #[test]
    fn test_should_flush_quiet_elapsed_reaches_threshold() {
        assert!(
            should_flush(
                Duration::from_millis(COOLDOWN_QUIET_MS),
                Duration::from_millis(500)
            ),
            "安静 2s 应触发（尾沿）"
        );
        assert!(
            should_flush(
                Duration::from_millis(3000),
                Duration::from_millis(3000)
            ),
            "安静 3s 应触发"
        );
    }

    /// 强制触发：总时长 ≥5s → 触发（即使一直在编辑，最终一致性保证）
    #[test]
    fn test_should_flush_deadline_forced() {
        assert!(
            should_flush(
                Duration::from_millis(300),
                Duration::from_millis(COOLDOWN_DEADLINE_MS)
            ),
            "总时长 5s 应强制触发（编辑未停也触发）"
        );
    }

    /// 冷却期内不触发：安静 <2s 且总时长 <5s
    #[test]
    fn test_should_flush_within_cooldown_does_not_trigger() {
        assert!(
            !should_flush(
                Duration::from_millis(1500),
                Duration::from_millis(1500)
            ),
            "编辑未停且未到 5s 上限不应触发"
        );
        assert!(
            !should_flush(Duration::ZERO, Duration::ZERO),
            "刚收到事件不应触发"
        );
    }

    /// 累积表：同 kind 路径合并、不同 kind 独立保留
    #[test]
    fn test_apply_batch_dedups_and_combines() {
        let mut pending: Vec<(PathBuf, ChangeKind)> = Vec::new();
        let batch1 = vec![WatchEvent {
            paths: vec![PathBuf::from("src/a.rs")],
            kind: ChangeKind::Modified,
        }];
        let batch2 = vec![
            WatchEvent {
                paths: vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
                kind: ChangeKind::Modified,
            },
            WatchEvent {
                paths: vec![PathBuf::from("src/c.rs")],
                kind: ChangeKind::Deleted,
            },
        ];
        apply_batch(&mut pending, &batch1);
        apply_batch(&mut pending, &batch2);
        let flushed = flush_events(&pending);
        assert_eq!(flushed.len(), 2, "同 kind 合并为 1 组 + Deleted 1 组");
        let modified = flushed
            .iter()
            .find(|e| e.kind == ChangeKind::Modified)
            .expect("应存在 Modified 组");
        assert_eq!(
            modified.paths,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
            "a 去重、b 追加"
        );
        assert!(flushed.iter().any(|e| e.kind == ChangeKind::Deleted));
    }

    /// 跨批时间序收敛：批 1 Modified a.rs、批 2 Deleted a.rs →
    /// 后到达的 Deleted 覆盖 Modified（文件最终被删，下游只跑一次删除清理）
    #[test]
    fn test_apply_batch_later_kind_overwrites() {
        let mut pending: Vec<(PathBuf, ChangeKind)> = Vec::new();
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/a.rs")],
                kind: ChangeKind::Modified,
            }],
        );
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
                kind: ChangeKind::Deleted,
            }],
        );
        let flushed = flush_events(&pending);
        assert_eq!(flushed.len(), 1, "跨批 Modified+Deleted 收敛为单个事件");
        assert_eq!(flushed[0].kind, ChangeKind::Deleted);
        assert_eq!(
            flushed[0].paths,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
    }

    /// 跨批删除重建：批 1 Deleted a.rs、批 2 Created a.rs →
    /// 后到达的 Created 覆盖 Deleted（文件最终存在，不能误跑删除清理——
    /// reviewer HIGH 缺陷的回归测试：删除优先只对批内最终态成立）
    #[test]
    fn test_apply_batch_delete_then_recreate_keeps_created() {
        let mut pending: Vec<(PathBuf, ChangeKind)> = Vec::new();
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/a.rs")],
                kind: ChangeKind::Deleted,
            }],
        );
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/a.rs")],
                kind: ChangeKind::Created,
            }],
        );
        let flushed = flush_events(&pending);
        assert_eq!(flushed.len(), 1);
        assert_eq!(
            flushed[0].kind,
            ChangeKind::Created,
            "删除重建必须收敛为 Created（文件最终存在），否则下游误删产物页"
        );
        assert_eq!(flushed[0].paths, vec![PathBuf::from("src/a.rs")]);
    }

    /// 连续编辑不丢路径：三次批的路径全部累积（同 kind）
    #[test]
    fn test_apply_batch_accumulates_across_batches() {
        let mut pending: Vec<(PathBuf, ChangeKind)> = Vec::new();
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/a.rs")],
                kind: ChangeKind::Modified,
            }],
        );
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/b.rs")],
                kind: ChangeKind::Modified,
            }],
        );
        apply_batch(
            &mut pending,
            &[WatchEvent {
                paths: vec![PathBuf::from("src/c.rs")],
                kind: ChangeKind::Modified,
            }],
        );
        let flushed = flush_events(&pending);
        assert_eq!(flushed.len(), 1);
        assert_eq!(
            flushed[0].paths,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/b.rs"),
                PathBuf::from("src/c.rs")
            ],
            "三批同 kind 路径应全部累积"
        );
    }
}
