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

    let watch_root = watch_root_from_scope(root, config);

    // notify-debouncer-full 0.4：Debouncer 自身实现了 Watcher trait，
    // 可以直接调用 .watch()，无需 .watcher()
    debouncer
        .watch(watch_root.as_path(), RecursiveMode::Recursive)
        .with_context(|| format!("监听目录失败: {}", watch_root.display()))?;

    tracing::info!("文件监听已启动（阻塞模式）: {}", watch_root.display());

    for result in rx {
        match result {
            Ok(events) => {
                // 按 kind 分组聚合防抖窗口内的事件（同 kind 路径去重合并），
                // 事件类型显式传递给下游，删除不再依赖 exists() 推断
                let watch_events = aggregate_events(&events, &include_exts);
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

/// 将防抖窗口内的事件按 kind 分组聚合：每种 kind 至多产出一个
/// `WatchEvent`，路径过滤后去重合并（保持输入顺序）。
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

/// 从 scope.include[0] 派生监听根目录（glob 模式取通配符前的目录部分，
/// 如 "src/**" → "src"；文件路径模式原样保留）。
///
/// 与 scanner 语义一致（scanner.rs 中 `include.is_empty()` 分支）：
/// include 为空或纯通配（如 "**/*.rs"）时监听项目根——空 include 匹配全部文件，
/// 纯通配模式的最长目录前缀为空，二者都意味着全项目范围。
fn watch_root_from_scope(root: &Path, config: &WikiConfig) -> PathBuf {
    let include0 = config
        .scope
        .include
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    // str::split 至少返回一段，unwrap_or_default 仅满足 Option 类型要求（空串返回空段）
    let dir_part = include0
        .split('*')
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    root.join(dir_part)
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

    /// 监听根：include[0] 的 glob 通配前缀被剥离（"src/**" → "src"），
    /// 文件路径模式原样保留
    #[test]
    fn test_watch_root_from_scope_strips_glob() {
        let config = make_config(vec!["src/**", "lib/**"]);
        assert_eq!(
            watch_root_from_scope(Path::new("/repo"), &config),
            PathBuf::from("/repo/src")
        );
        let config = make_config(vec!["Cargo.toml"]);
        assert_eq!(
            watch_root_from_scope(Path::new("/repo"), &config),
            PathBuf::from("/repo/Cargo.toml")
        );
    }

    /// 监听根：include 为空或纯通配（"**/*.rs"）时监听项目根——
    /// 与 scanner 空 include = 全部匹配的语义一致（scanner.rs include.is_empty() 分支）
    #[test]
    fn test_watch_root_from_scope_empty_or_pure_glob_falls_back_to_root() {
        let config = make_config(vec![]);
        assert_eq!(
            watch_root_from_scope(Path::new("/repo"), &config),
            PathBuf::from("/repo")
        );
        let config = make_config(vec!["**/*.rs"]);
        assert_eq!(
            watch_root_from_scope(Path::new("/repo"), &config),
            PathBuf::from("/repo")
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
}
