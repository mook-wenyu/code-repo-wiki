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

use crate::config::schema::WikiConfig;

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
/// 监听根 = root 拼接 scope.include[0] 的目录部分（与扫描生成范围一致，
/// include 是 glob 模式，取通配符之前的目录）。
///
/// 边界：删除事件的路径已不存在于磁盘，回调内不能读取文件内容；
/// 事件类型（ChangeKind）已显式携带，下游不再以 exists() 推断删除。
pub fn run_watch_loop(
    root: &Path,
    config: &WikiConfig,
    on_change: impl Fn(Vec<WatchEvent>) + Send + 'static,
) -> Result<()> {
    let include_exts = collect_include_exts(config);

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )
    .with_context(|| "创建文件防抖监听器失败")?;

    // U03/D4：监听全部 include 前缀目录（默认 src/** 与 lib/** 都要监听到；
    // 单目录场景行为不变）。监听根不存在时跳过该根并告警——include 里的
    // 目录可以尚未创建（如 lib/ 在纯 src 项目里），整体失败会让 watch 无法启动。
    let watch_roots = watch_roots_from_scope(root, config);
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

    for result in rx {
        match result {
            Ok(events) => {
                // 聚合 + 折叠（同路径跨 kind 按最终态合并），
                // 事件类型显式传递给下游，删除不再依赖 exists() 推断
                let watch_events = process_batch(&events, &include_exts);
                if !watch_events.is_empty() {
                    on_change(watch_events);
                }
            }
            Err(errors) => {
                for e in &errors {
                    tracing::warn!("文件监听错误: {:?}", e);
                }
            }
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

/// 从 scope.include 派生全部监听根目录（U03/D4：不再只取 include[0]——
/// 默认配置（src/** 与 lib/**）下 lib/ 的变更此前永不触发事件）
///
/// 每个 include 模式取通配符前的目录部分（如 "src/**" → "src"；
/// 文件路径模式原样保留），去重后逐一注册监听。与 scanner 语义一致
/// （scanner.rs 中 `include.is_empty()` 分支）：include 为空或纯通配
/// （如 "**/*.rs"）时监听项目根——空 include 匹配全部文件，纯通配模式
/// 的最长目录前缀为空，二者都意味着全项目范围。
fn watch_roots_from_scope(root: &Path, config: &WikiConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for include in &config.scope.include {
        // str::split 至少返回一段（空串返回空段）
        let dir_part = include.split('*').next().unwrap_or_default().trim_end_matches('/');
        let candidate = if dir_part.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir_part)
        };
        if !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    if roots.is_empty() {
        roots.push(root.to_path_buf());
    }
    roots
}

/// 从 scope.include 提取扩展名列表（含 '*' 通配的模式无法确定扩展名，跳过）
fn collect_include_exts(config: &WikiConfig) -> Vec<String> {
    config
        .scope
        .include
        .iter()
        .filter_map(|p| {
            if let Some(ext) = p.split('.').next_back() {
                if ext.contains('*') { None } else { Some(ext.to_string()) }
            } else {
                None
            }
        })
        .collect()
}

/// 路径是否应上报（未被忽略且扩展名在 include 列表内）
fn should_report(path: &Path, include_exts: &[String]) -> bool {
    !should_ignore(path) && matches_include(path, include_exts)
}

/// 判断路径是否应被忽略
fn should_ignore(path: &Path) -> bool {
    let ignore_dirs = ["target", ".git", "node_modules", "vendor"];
    path.components().any(|c| {
        if let Some(s) = c.as_os_str().to_str() {
            ignore_dirs.contains(&s)
        } else {
            false
        }
    })
}

/// 检查路径的扩展名是否在 include 列表中
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

    fn make_config(include: Vec<&str>) -> WikiConfig {
        let mut config = WikiConfig::default();
        config.scope.include = include.into_iter().map(String::from).collect();
        config
    }

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
    fn test_should_not_ignore_src_dir() {
        let p = Path::new("/repo/src/main.rs");
        assert!(!should_ignore(p));
    }

    #[test]
    fn test_matches_include_with_matching_ext() {
        let exts = vec!["rs".to_string(), "toml".to_string()];
        assert!(matches_include(Path::new("main.rs"), &exts));
        assert!(matches_include(Path::new("Cargo.toml"), &exts));
    }

    #[test]
    fn test_matches_include_with_mismatch_ext() {
        let exts = vec!["rs".to_string()];
        assert!(!matches_include(Path::new("main.js"), &exts));
        assert!(!matches_include(Path::new("no_ext"), &exts));
    }

    #[test]
    fn test_matches_include_empty_list_allows_all() {
        let exts: Vec<String> = vec![];
        assert!(matches_include(Path::new("anything.txt"), &exts));
        assert!(matches_include(Path::new("no_ext"), &exts));
    }

    /// 扩展名提取：glob 通配模式跳过，普通扩展名保留
    #[test]
    fn test_collect_include_exts_skips_glob_patterns() {
        let config = make_config(vec!["src/**", "lib/**", "**/*.toml"]);
        assert_eq!(collect_include_exts(&config), vec!["toml"]);
    }

    /// 监听根（U03/D4）：全部 include 的 glob 通配前缀被剥离、去重，
    /// 文件路径模式原样保留——默认配置（src/** 与 lib/**）产出两个监听根
    #[test]
    fn test_watch_roots_from_scope_strips_glob() {
        let config = make_config(vec!["src/**", "lib/**"]);
        assert_eq!(
            watch_roots_from_scope(Path::new("/repo"), &config),
            vec![PathBuf::from("/repo/src"), PathBuf::from("/repo/lib")]
        );
        let config = make_config(vec!["Cargo.toml"]);
        assert_eq!(
            watch_roots_from_scope(Path::new("/repo"), &config),
            vec![PathBuf::from("/repo/Cargo.toml")]
        );
        // 去重：同一目录的多个模式只监听一次
        let config = make_config(vec!["src/**", "src/**/*.rs"]);
        assert_eq!(
            watch_roots_from_scope(Path::new("/repo"), &config),
            vec![PathBuf::from("/repo/src")]
        );
    }

    /// 监听根：include 为空或纯通配（"**/*.rs"）时监听项目根——
    /// 与 scanner 空 include = 全部匹配的语义一致（scanner.rs include.is_empty() 分支）
    #[test]
    fn test_watch_roots_from_scope_empty_or_pure_glob_falls_back_to_root() {
        let config = make_config(vec![]);
        assert_eq!(
            watch_roots_from_scope(Path::new("/repo"), &config),
            vec![PathBuf::from("/repo")]
        );
        let config = make_config(vec!["**/*.rs"]);
        assert_eq!(
            watch_roots_from_scope(Path::new("/repo"), &config),
            vec![PathBuf::from("/repo")]
        );
    }

    /// 上报判定：忽略目录与扩展名不匹配的路径不上报
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
}
