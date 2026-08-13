use anyhow::{Result, bail};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

use crate::ingest::parser::SUPPORTED_EXTENSIONS;

/// 默认扫描文件数上限（超过即报错，避免海量文件拖垮整条管线）
const MAX_FILES: usize = 100_000;

/// 单文件字节上限（audit-gen-07）：超大源码文件（如生成的 bundle/长测试
/// fixture）按字节读入会浪费内存且对解析/嵌入无增益，超限跳过并告警，
/// 计入 ScanOutput.files_failed（与解析失败同口径，失败可观测）。
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

const BINARY_EXTENSIONS: &[&str] = &[
    ".exe", ".dll", ".bin", ".png", ".jpg", ".jpeg", ".gif", ".ico", ".svg", ".pdf", ".ttf",
    ".woff", ".woff2", ".eot", ".zip", ".tar", ".gz", ".7z", ".rar", ".mp3", ".mp4", ".avi",
    ".mov", ".wasm", ".o", ".obj", ".lib", ".a", ".so", ".dylib", ".pyc", ".class",
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

/// 任意深度剪枝的噪音目录（依赖/缓存/构建产物，嵌套出现也剪）：
/// node_modules/target/.git 等项目普遍依赖目录，出现在任意层级都应跳过。
pub const NOISE_DIRS: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "vendor",
    "Pods",
    "Library",
    "target",
    ".next",
    ".nuxt",
    ".output",
    "coverage",
    ".cache",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    "bower_components",
    ".git",
];

/// 仅根级剪枝的构建输出目录（P1-14 修复）：
///
/// dist/build/out/bin/obj 是**常见合法源码目录名**——Rust 标准布局
/// src/bin/*.rs 会被任意深度匹配的 bin 整棵跳过（入口文件全丢）、
/// C/C++ 项目的 out/build 目录也可能含手写源码。这些只在仓库根
/// （深度 1）出现时按构建产物处理，嵌套出现视为普通源码目录。
///
/// Unity 项目同理：Packages/（UPM 第三方包目录，常含数百个 .cs）、
/// Temp/（Unity 编译缓存）与 Logs/（编辑器日志）均为根级生成物，
/// 全量入扫会放大图构建/分析成本；嵌套出现（如 Unity 内嵌子项目
/// 的 src/Packages/）仍按普通源码目录保留，语义与其余根级清单一致。
pub const ROOT_ONLY_NOISE_DIRS: &[&str] = &[
    "dist", "build", "out", "bin", "obj", "Packages", "Temp", "Logs",
];

/// 噪音目录判定：任意深度清单直接命中；根级清单仅 entry.depth()==1 时命中
/// （depth 0=仓库根自身，1=根的直接子目录——构建产物通常在根级出现）
fn is_noise_dir(name: &str, at_root_level: bool) -> bool {
    NOISE_DIRS.contains(&name) || (at_root_level && ROOT_ONLY_NOISE_DIRS.contains(&name))
}

/// 文件系统遍历器：全量遍历 + 内置过滤（v30+：无 include/exclude 配置，
/// 扫描范围由「可解析语言 + 噪音目录 + 二进制 + 文件数上限」四个内置边界决定——
/// 不同项目目录结构不同，路径模式无法通用，语言才是 code-repo-wiki 的能力边界）
pub struct Scanner {
    root: PathBuf,
    /// 本次扫描因超单文件字节上限被跳过的文件数（audit-gen-07）。
    /// 用 Cell 内变：scan 只取 &self，计数在遍历中就地累加。
    skipped_oversized: std::cell::Cell<usize>,
}

impl Scanner {
    /// 创建 Scanner，根为项目根目录
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            skipped_oversized: std::cell::Cell::new(0),
        }
    }

    /// 本次扫描因超过单文件字节上限被跳过的文件数（上游并入
    /// ScanOutput.files_failed，使超大文件跳过可观测、不静默）
    pub fn skipped_oversized(&self) -> usize {
        self.skipped_oversized.get()
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
                // 这里补充显式清单：node_modules/target 等任意深度依赖与构建产物；
                // dist/build/out/bin/obj 等仅根级剪枝——src/bin 等合法源码目录
                // 不被误杀，P1-14）
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                if !is_dir {
                    return true;
                }
                // depth：0=根自身，1=根的直接子目录（构建产物判定锚点）
                let at_root_level = entry.depth() == 1;
                !is_noise_dir(&entry.file_name().to_string_lossy(), at_root_level)
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

            // audit-gen-07：单文件字节上限——超限跳过并告警（计入
            // skipped_oversized，上游并入 files_failed），超大文件对
            // 读取/解析/嵌入均无增益，读入反而拖垮内存与耗时
            if let Ok(meta) = entry.metadata()
                && meta.len() > MAX_FILE_BYTES
            {
                tracing::warn!(
                    "跳过超大文件 {}（{} 字节 > 上限 {} 字节）",
                    path.display(),
                    meta.len(),
                    MAX_FILE_BYTES
                );
                self.skipped_oversized.set(self.skipped_oversized.get() + 1);
                continue;
            }

            if files.len() >= limit {
                bail!(
                    "源文件数超过上限 {limit}（噪音目录已自动跳过；若项目确需更多请精简或忽略多余内容）"
                );
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
            "code_repo_wiki_test_scanner_{}_{}",
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
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
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
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(names.len(), 1, "噪音目录应被跳过: {names:?}");
        assert!(
            names[0].ends_with("src/main.rs"),
            "唯一产物应为主源码: {names:?}"
        );

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

    /// audit-gen-07：单文件超过字节上限时跳过并计入 skipped_oversized，
    /// 小文件正常保留
    #[test]
    fn test_scanner_skips_oversized_file() {
        let dir = scratch("oversize");
        std::fs::write(dir.join("small.rs"), "pub fn small() {}").unwrap();
        // 6MB 超限文件（上限 5MB）：字节超限即跳过，不读入内存
        std::fs::write(dir.join("huge.rs"), vec![b'a'; 6 * 1024 * 1024]).unwrap();

        let scanner = Scanner::new(&dir);
        let files = scanner.scan().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(names.len(), 1, "超大文件应被跳过: {names:?}");
        assert!(names[0].ends_with("small.rs"), "仅小文件应保留: {names:?}");
        assert_eq!(
            scanner.skipped_oversized(),
            1,
            "超限文件应计入 skipped_oversized"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-14 回归锚：src/bin 是合法源码目录，根级 bin 才是噪音——
    /// 扫描 src/bin 下文件不被跳过，根级 bin/ 被剪枝
    #[test]
    fn test_noise_dirs_root_anchored() {
        let dir = scratch("rootnoise");
        std::fs::create_dir_all(dir.join("src/bin")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("src/bin/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("bin/gen.rs"), "fn gen() {}").unwrap();
        std::fs::write(dir.join("dist/bundle.rs"), "fn bundle() {}").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

        let scanner = Scanner::new(&dir);
        let files = scanner.scan().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("src/bin/main.rs")),
            "src/bin 是合法源码目录不应被剪: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("src/main.rs")),
            "src 顶层源码应保留: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("bin/")),
            "根级 bin/ 应被剪枝: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("dist/")),
            "根级 dist/ 应被剪枝: {names:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unity 项目根级生成物剪枝：根级 Packages/Temp/Logs 整棵跳过，
    /// 嵌套同名目录（内嵌子项目）仍按普通源码目录保留
    #[test]
    fn test_noise_dirs_unity_root_anchored() {
        let dir = scratch("unitynoise");
        std::fs::create_dir_all(dir.join("Assets/Scripts")).unwrap();
        std::fs::create_dir_all(dir.join("Packages/com.unity.test-framework")).unwrap();
        std::fs::create_dir_all(dir.join("Temp/Il2Cpp")).unwrap();
        std::fs::create_dir_all(dir.join("Logs")).unwrap();
        std::fs::create_dir_all(dir.join("src/Packages/local")).unwrap();
        std::fs::write(dir.join("Assets/Scripts/Game.cs"), "class Game {}").unwrap();
        std::fs::write(
            dir.join("Packages/com.unity.test-framework/Test.cs"),
            "class T {}",
        )
        .unwrap();
        std::fs::write(dir.join("Temp/Il2Cpp/gen.cs"), "class G {}").unwrap();
        std::fs::write(dir.join("Logs/editor.cs"), "class L {}").unwrap();
        std::fs::write(dir.join("src/Packages/local/Helper.cs"), "class H {}").unwrap();

        let scanner = Scanner::new(&dir);
        let files = scanner.scan().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("Assets/Scripts/Game.cs")),
            "主源码应保留: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.ends_with("src/Packages/local/Helper.cs")),
            "嵌套 Packages 是合法源码目录不应被剪: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("Packages/")),
            "根级 Packages/ 应被剪枝: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("Temp/")),
            "根级 Temp/ 应被剪枝: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("Logs/")),
            "根级 Logs/ 应被剪枝: {names:?}"
        );
        assert_eq!(files.len(), 2, "仅主源码与嵌套目录文件应保留: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
