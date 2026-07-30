use std::path::{Path, PathBuf};
use anyhow::Result;
use glob::Pattern;
use ignore::WalkBuilder;

use crate::config::schema::ScopeSection;

const BINARY_EXTENSIONS: &[&str] = &[
    ".exe", ".dll", ".bin", ".png", ".jpg", ".jpeg", ".gif", ".ico", ".svg",
    ".pdf", ".ttf", ".woff", ".woff2", ".eot", ".zip", ".tar", ".gz", ".7z",
    ".rar", ".mp3", ".mp4", ".avi", ".mov", ".wasm", ".o", ".obj", ".lib",
    ".a", ".so", ".dylib", ".pyc", ".class",
];

fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = format!(".{}", ext.to_lowercase());
            BINARY_EXTENSIONS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

/// 文件系统遍历器，支持 .gitignore 和 glob 模式过滤
pub struct Scanner {
    root: PathBuf,
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl Scanner {
    /// 创建 Scanner
    ///
    /// * `root` - 项目根目录
    /// * `scope` - 扫描范围配置（include/exclude glob 模式）
    pub fn new(root: &Path, scope: &ScopeSection) -> Self {
        let include = scope.include.iter().filter_map(|p| Pattern::new(p).ok()).collect();
        let exclude = scope.exclude.iter().filter_map(|p| Pattern::new(p).ok()).collect();
        Self { root: root.to_path_buf(), include, exclude }
    }

    /// 遍历目录树，返回匹配的文件列表
    ///
    /// - 使用 `ignore::WalkBuilder` 处理 .gitignore
    /// - 按 scope.include 和 scope.exclude 的 glob 模式过滤
    pub fn scan(&self) -> Result<Vec<PathBuf>> {
        let walker = WalkBuilder::new(&self.root).standard_filters(true).build();
        let mut files = Vec::new();

        for result in walker {
            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("遍历目录出错: {}", err);
                    continue;
                }
            };
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }

            let path = entry.path();
            let rel = path.strip_prefix(&self.root).unwrap_or(path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            let included = self.include.is_empty() || self.include.iter().any(|p| p.matches(&rel_str));
            let excluded = self.exclude.iter().any(|p| p.matches(&rel_str));
            if included && !excluded && !is_binary_extension(path) {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scanner_compiles_and_filters() {
        let scope = ScopeSection {
            include: vec!["**/*.rs".to_string()],
            exclude: vec!["**/test/**".to_string()],
        };
        let scanner = Scanner::new(Path::new("."), &scope);
        assert_eq!(scanner.include.len(), 1);
        assert_eq!(scanner.exclude.len(), 1);
        // smoke: scan current project dir, at least src/lib.rs should be found
        let files = scanner.scan().unwrap();
        assert!(files.iter().any(|p| p.to_string_lossy().contains("lib.rs")));
    }
}
