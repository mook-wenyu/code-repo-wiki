//! 文件系统监听模块
//!
//! 使用 `notify-debouncer-full` crate 实现防抖文件监听，
//! 将底层 notify 事件去重、聚合后通过 channel 上报。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebouncedEvent, DebounceEventResult};

use crate::ingest::scanner::NOISE_DIRS;
use crate::ingest::parser::SUPPORTED_EXTENSIONS;

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
    use std::sync::atomic::Ordering;
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            tracing::info!("收到停止信号，文件监听退出");
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                // 聚合 + 折叠（同路径跨 kind 按最终态合并），
                // 事件类型显式传递给下游，删除不再依赖 exists() 推断
                let watch_events = process_batch(&events, &include_exts);
                if !watch_events.is_empty() {
                    on_change(watch_events);
                }
            }
            Ok(Err(errors)) => {
                for e in &errors {
                    tracing::warn!("文件监听错误: {:?}", e);
                }
            }
            // 超时（轮询停止标记）是正常路径：回到循环顶检查 stop_flag；
            // Disconnected = 接收端全部 drop，监听无意义
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
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

/// 判断路径是否位于噪音目录（与 scanner::NOISE_DIRS 同清单：
/// 第三方依赖与构建产物，事件不必上报）
fn should_ignore(path: &Path) -> bool {
    path.components().any(|c| {
        if let Some(s) = c.as_os_str().to_str() {
            NOISE_DIRS.contains(&s)
        } else {
            false
        }
    })
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
        let dir = std::env::temp_dir().join(format!("repo_wiki_watch_stop_{}", std::process::id()));
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
}
