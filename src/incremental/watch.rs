//! 文件系统监听模块
//!
//! 使用 `notify-debouncer-full` crate 实现防抖文件监听，
//! 将底层 notify 事件去重、聚合后通过 channel 上报。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::config::schema::WikiConfig;

/// 文件变更事件
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// 发生变更的文件路径列表（防抖窗口内合并）
    pub paths: Vec<PathBuf>,
    /// 变更类型
    pub kind: ChangeKind,
}

/// 变更类型
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeKind {
    /// 文件被创建
    Created,
    /// 文件被修改
    Modified,
    /// 文件被删除
    Deleted,
}

/// 文件监听器
///
/// 使用 notify-debouncer-full 在后台线程监听文件变更，
/// 通过内部 mpsc channel 向主线程发送聚合后的事件。
pub struct FileWatcher {
    /// 监听根目录
    root: PathBuf,
    /// 事件发送端
    tx: mpsc::Sender<FileChangeEvent>,
    /// 事件接收端
    rx: mpsc::Receiver<FileChangeEvent>,
}

impl FileWatcher {
    /// 创建新的文件监听器
    pub fn new(root: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            root: root.to_path_buf(),
            tx,
            rx,
        }
    }

    /// 开始监听文件变更（阻塞当前线程）
    pub fn watch(&mut self, config: &WikiConfig) -> Result<()> {
        let tx = self.tx.clone();
        let include_exts: Vec<String> = config
            .scope
            .include
            .iter()
            .filter_map(|p| {
                if let Some(ext) = p.split('.').last() {
                    if ext.contains('*') { None } else { Some(ext.to_string()) }
                } else {
                    None
                }
            })
            .collect();

        // 第 2 个参数是 Optional Duration——防抖后的事件超时阈值
        let mut debouncer = new_debouncer(
            Duration::from_millis(300),
            None,
            move |result: DebounceEventResult| {
                let events = match result {
                    Ok(events) => events,
                    Err(errors) => {
                        for e in &errors {
                            tracing::warn!("文件监听错误: {:?}", e);
                        }
                        return;
                    }
                };

                let mut paths = Vec::new();
                let mut kind = ChangeKind::Modified;

                for debounced in events {
                    let ev = &debounced.event;
                    for p in &ev.paths {
                        if should_ignore(p.as_path()) {
                            continue;
                        }
                        if !matches_include(p.as_path(), &include_exts) {
                            continue;
                        }
                        paths.push(p.clone());
                    }
                    // 判断整条 event 的 kind
                    if let Some(ek) = notify_event_to_change_kind(&ev.kind) {
                        if ek != ChangeKind::Modified {
                            kind = ek;
                        }
                    }
                }

                if !paths.is_empty() {
                    let _ = tx.send(FileChangeEvent { paths, kind });
                }
            },
        )
        .with_context(|| "创建文件防抖监听器失败")?;

        // 监听 src/ 目录（递归）
        let watch_root = Path::new(&config.scope.include[0]);
        let watch_path = if watch_root.is_absolute() {
            watch_root.to_path_buf()
        } else {
            self.root.join(watch_root)
        };

        // notify-debouncer-full 0.4：Debouncer 自身实现了 Watcher trait，
        // 可以直接调用 .watch()，无需 .watcher()
        debouncer
            .watch(watch_path.as_path(), RecursiveMode::Recursive)
            .with_context(|| format!("监听目录失败: {}", watch_path.display()))?;

        tracing::info!("文件监听已启动: {}", watch_path.display());

        // 保持监听器存活
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// 非阻塞方式获取已合并的文件变更事件
    pub fn try_recv(&self) -> Option<FileChangeEvent> {
        self.rx.try_recv().ok()
    }
}

/// 启动文件监听并执行回调（阻塞版本）
pub fn run_watch_loop(
    _config: &WikiConfig,
    on_change: impl Fn() + Send + 'static,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )
    .with_context(|| "创建文件防抖监听器失败")?;

    // 监听当前目录
    debouncer
        .watch(Path::new("."), RecursiveMode::Recursive)
        .with_context(|| "监听目录失败")?;

    tracing::info!("文件监听已启动（阻塞模式）");

    for result in rx {
        match result {
            Ok(events) => {
                let has_relevant = events.iter().any(|e| {
                    e.event.paths.iter().any(|p| {
                        !should_ignore(p) && p.extension().is_some()
                    })
                });
                if has_relevant {
                    on_change();
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

/// 将 notify::EventKind 转为内部 ChangeKind
fn notify_event_to_change_kind(kind: &notify::EventKind) -> Option<ChangeKind> {
    use notify::EventKind::*;
    match kind {
        Create(_) => Some(ChangeKind::Created),
        Modify(_) => Some(ChangeKind::Modified),
        Remove(_) => Some(ChangeKind::Deleted),
        _ => None,
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

    #[test]
    fn test_notify_event_to_change_kind_mapping() {
        use notify::EventKind;
        assert_eq!(
            notify_event_to_change_kind(&EventKind::Create(notify::event::CreateKind::File)),
            Some(ChangeKind::Created)
        );
        assert_eq!(
            notify_event_to_change_kind(&EventKind::Modify(notify::event::ModifyKind::Data(notify::event::DataChange::Content))),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            notify_event_to_change_kind(&EventKind::Remove(notify::event::RemoveKind::File)),
            Some(ChangeKind::Deleted)
        );
        assert_eq!(
            notify_event_to_change_kind(&EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
    }
}
