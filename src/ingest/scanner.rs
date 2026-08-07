use std::path::{Path, PathBuf};
use anyhow::{Result, bail};
use ignore::WalkBuilder;

use crate::ingest::parser::SUPPORTED_EXTENSIONS;

/// 默认扫描文件数上限（超过即报错，避免海量文件拖垮整条管线）
const MAX_FILES: usize = 100_000;

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

/// 内置噪音目录：第三方依赖与构建产物（全量扫描时的边界——不依赖 .gitignore，
/// 许多项目未规范编写。命中目录整棵跳过，防止 node_modules/target 等爆炸）。
/// pub：文件监听（watch.rs）共用同一清单，扫与听保持一致边界。
pub const NOISE_DIRS: &[&str] = &[
    "node_modules", ".venv", "venv", "vendor", "Pods", "Library",
    "target", "dist", "build", "out", ".next", ".nuxt", ".output",
    "coverage", ".cache", "__pycache__", ".pytest_cache", ".mypy_cache",
    "bower_components", ".git", "obj", "bin",
];

fn is_noise_dir(name: &str) -> bool {
    NOISE_DIRS.contains(&name)
}

/// 文件系统遍历器：全量遍历 + 内置过滤（v30+：无 include/exclude 配置，
/// 扫描范围由「可解析语言 + 噪音目录 + 二进制 + 文件数上限」四个内置边界决定——
/// 不同项目目录结构不同，路径模式无法通用，语言才是 repo-wiki 的能力边界）
pub struct Scanner {
    root: PathBuf,
}

impl Scanner {
    /// 创建 Scanner，根为项目根目录
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }

    /// 遍历目录树，返回可解析的源文件列表
    ///
    /// - 使用 `ignore::WalkBuilder` 处理 .gitignore 与隐藏目录
    /// - 跳过内置噪音目录（依赖/构建产物，见 [NOISE_DIRS]）
    /// - 只保留 [SUPPORTED_EXTENSIONS] 内的源文件
    /// - 超过 MAX_FILES 个文件时返回错误
    pub fn scan(&self) -> Result<Vec<PathBuf>> {
        self.scan_with_limit(MAX_FILES)
    }

    /// 带文件数上限的扫描（上限可配置，供测试覆盖超限分支）
    fn scan_with_limit(&self, limit: usize) -> Result<Vec<PathBuf>> {
        let walker = WalkBuilder::new(&self.root)
            .standard_filters(true)
            .filter_entry(|entry| {
                // 目录名命中噪音清单时整棵剪枝（标准过滤器已跳 .git/隐藏目录，
                // 这里补充显式清单：node_modules/target/dist 等常见依赖与构建产物）
                !entry
                    .file_type()
                    .is_some_and(|ft| ft.is_dir() && is_noise_dir(&entry.file_name().to_string_lossy()))
            })
            .build();
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
            if is_binary_extension(path) {
                continue;
            }
            // 只保留可解析语言的源文件（v30+：按语言而非路径模式过滤——
            // 各项目目录结构不同，但「支持的语言」是确定的通用边界）
            let is_source = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| {
                    let ext = format!(".{}", ext.to_lowercase());
                    SUPPORTED_EXTENSIONS.contains(&ext.as_str())
                })
                .unwrap_or(false);
            if !is_source {
                continue;
            }

            if files.len() >= limit {
                bail!("源文件数超过上限 {limit}（噪音目录已自动跳过；若项目确需更多请精简或忽略多余内容）");
            }
            files.push(path.to_path_buf());
        }

        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn scratch(_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "repo_wiki_test_scanner_{}_{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// v30+：无任何配置时全量扫描，只收可解析语言的源文件
    #[test]
    fn test_scanner_default_scans_sources_only() {
        let dir = scratch("default");
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "pub fn a() {}").unwrap();
        std::fs::write(dir.join("src/sub/b.ts"), "export const b = 1;").unwrap();
        // 非支持语言与二进制不进入结果
        std::fs::write(dir.join("README.md"), "docs").unwrap();
        std::fs::write(dir.join("pic.png"), b"\x89PNG").unwrap();
        std::fs::write(dir.join("data.json"), "{}").unwrap();

        let scanner = Scanner::new(&dir);
        let files = scanner.scan().unwrap();
        let names: Vec<String> = files.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        assert!(names.iter().any(|n| n.ends_with("src/a.rs")));
        assert!(names.iter().any(|n| n.ends_with("src/sub/b.ts")));
        assert_eq!(files.len(), 2, "非支持语言与二进制应被过滤: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内置噪音目录整棵跳过（node_modules/target 等），不依赖 .gitignore
    #[test]
    fn test_scanner_skips_noise_dirs() {
        let dir = scratch("noise");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "module.exports = 1;").unwrap();
        std::fs::write(dir.join("target/debug/lib.rs"), "fn x() {}").unwrap();

        let scanner = Scanner::new(&dir);
        let files = scanner.scan().unwrap();
        let names: Vec<String> = files.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(names.len(), 1, "噪音目录应被跳过: {names:?}");
        assert!(names[0].ends_with("src/main.rs"), "唯一产物应为主源码: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件数超限时应返回错误
    #[test]
    fn test_scan_with_limit_exceeds() {
        let dir = scratch("limit");
        for i in 0..4 {
            std::fs::write(dir.join(format!("f{i}.rs")), "fn x() {}").unwrap();
        }

        let scanner = Scanner::new(&dir);
        assert!(scanner.scan_with_limit(3).is_err());

        // 上限之内正常返回
        let files = scanner.scan_with_limit(10).unwrap();
        assert_eq!(files.len(), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
