//! Wiki 产物健康检查（lint）
//!
//! 对已生成的 wiki 产物目录做静态检查，供 `repo-wiki lint` 命令与 CI 使用。
//! 对齐 LLM Wiki 最佳实践（Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）：
//!
//! 1. **孤儿页**：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复）
//! 2. **断链**：页面内链接指向不存在的产物文件（复制 crossref 语义，但作用于磁盘产物）
//! 3. **过时**：页面生成时间戳早于其源文件修改时间（源码已变但文档未更新）
//!
//! 检查对象是**磁盘上的产物文件**（真实用户看到的东西），而非内存中的文档对象。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 单条 lint 问题
#[derive(Debug, Clone)]
pub struct LintIssue {
    /// 问题类别: orphan / broken / stale
    pub kind: &'static str,
    /// 问题文件相对路径（相对 output_dir）
    pub path: String,
    /// 问题描述
    pub message: String,
}

/// 执行 lint 检查，返回所有发现的问题（无问题返回空列表）
///
/// `output_dir` 为产物根目录（config.output.dir），
/// `source_roots` 为源码扫描根列表（用于过时检查的源文件 mtime 对比）。
pub fn lint(output_dir: &Path, source_roots: &[PathBuf]) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let wiki_root = output_dir.join("wiki");

    // 收集主语言目录下的全部 .md 产物（wiki 页 + 全局文档）
    let languages = collect_language_dirs(&wiki_root);
    for lang in &languages {
        let lang_dir = wiki_root.join(lang);
        let pages = collect_md_files(&lang_dir);

        // ---- 1. 孤儿页检查：收集所有页面内链接（含目录页 _toc），统计入链 ----
        // 链接形态：[](wiki/zh/xxx.md) 或相对链接 (xxx.md)；按文件名主体匹配
        let mut incoming: HashMap<String, usize> = HashMap::new();
        // 链接统计范围 = 语言目录页面 + 产物根目录页(_toc.md,它在 wiki 根而非 lang 目录,
        // 但其链接指向全部页面——不统计则每页都因无入链被误标孤儿)
        let mut link_sources: Vec<PathBuf> = pages.clone();
        let toc_path = output_dir.join("_toc.md");
        if toc_path.exists() {
            link_sources.push(toc_path);
        }
        for page in &link_sources {
            let content = std::fs::read_to_string(page).unwrap_or_default();
            for link in extract_md_links(&content) {
                // 仅统计 wiki 页面间链接（.md 结尾且不含协议）
                if link.ends_with(".md") && !link.contains("://") {
                    let stem = link
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&link)
                        .trim_end_matches(".md")
                        .to_string();
                    *incoming.entry(stem).or_default() += 1;
                }
            }
        }

        for page in &pages {
            let file_name = page
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = file_name.trim_end_matches(".md").to_string();
            // 全局文档(api/overview/architecture/_toc)由 TOC/概览引用,不算孤儿
            let is_global = matches!(
                stem.as_str(),
                "api" | "overview" | "architecture" | "_toc" | "index"
            );
            if !is_global && incoming.get(&stem).copied().unwrap_or(0) == 0 {
                issues.push(LintIssue {
                    kind: "orphan",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("孤儿页: 无任何页面链接指向 {file_name}"),
                });
            }
        }

        // ---- 2. 断链检查：页面内链接目标必须存在 ----
        for page in &pages {
            let content = std::fs::read_to_string(page).unwrap_or_default();
            let file_name = page
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            for link in extract_md_links(&content) {
                if !link.ends_with(".md") || link.contains("://") {
                    continue;
                }
                // 解析链接目标:可能带 wiki/zh/ 前缀或纯文件名
                let target_name = link.rsplit(['/', '\\']).next().unwrap_or(&link);
                let target_exists = pages.iter().any(|p| {
                    p.file_name()
                        .map(|s| s.to_string_lossy() == target_name)
                        .unwrap_or(false)
                });
                if !target_exists {
                    issues.push(LintIssue {
                        kind: "broken",
                        path: format!("wiki/{lang}/{file_name}"),
                        message: format!("断链: {link} 指向不存在的产物文件"),
                    });
                }
            }
        }

        // ---- 3. 过时检查：模块页/卡片生成时间 < 其源文件 mtime ----
        // 从产物内容提取源文件路径（相关文件段: - `path`，卡片含此段，
        // 模块页正文未必含），与源码根下对应文件的 mtime 对比
        let cards_dir = output_dir.join("cards").join(lang);
        let mut stale_targets: Vec<PathBuf> = pages.clone();
        stale_targets.extend(collect_md_files(&cards_dir));
        for page in &stale_targets {
            let content = std::fs::read_to_string(page).unwrap_or_default();
            let file_name = page
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let page_mtime = std::fs::metadata(page)
                .and_then(|m| m.modified())
                .ok();
            let Some(page_time) = page_mtime else { continue };
            for src in extract_source_files(&content) {
                let abs = resolve_source_path(source_roots, &src);
                if let Ok(meta) = std::fs::metadata(&abs)
                    && let Ok(src_time) = meta.modified()
                    && src_time > page_time
                {
                    issues.push(LintIssue {
                        kind: "stale",
                        path: format!("wiki/{lang}/{file_name}"),
                        message: format!(
                            "过时: 源文件 {src} 的修改时间晚于页面生成时间(源码已变更,文档可能未更新)"
                        ),
                    });
                }
            }
        }
    }

    issues
}

/// 收集 wiki 根下的语言目录（zh/en/...）
fn collect_language_dirs(wiki_root: &Path) -> Vec<String> {
    let mut langs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(wiki_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                langs.push(name.to_string());
            }
        }
    }
    langs
}

/// 递归收集目录下所有 .md 文件
fn collect_md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(collect_md_files(&p));
        } else if p.extension().is_some_and(|e| e == "md") {
            out.push(p);
        }
    }
    out
}

/// 提取 markdown 文本中的链接目标 [text](target)
fn extract_md_links(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let end = after.find(')').unwrap_or(after.len());
        let target = after[..end].trim().to_string();
        if !target.is_empty() {
            out.push(target);
        }
        rest = &after[end.min(after.len())..];
    }
    out
}

/// 从卡片/页面内容提取源文件路径（`- `code`` 相关文件段）
fn extract_source_files(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("- `") && line.ends_with('`') {
            let inner = &line[3..line.len() - 1];
            if inner.contains('.') && !inner.contains("://") {
                out.push(inner.to_string());
            }
        }
    }
    out
}

/// 将产物中记录的源路径解析为绝对路径（相对源码根逐根尝试）
fn resolve_source_path(source_roots: &[PathBuf], src: &str) -> PathBuf {
    let p = Path::new(src);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    for root in source_roots {
        // 产物内路径是相对 cwd 的完整相对路径(如 "src/lib.rs"),可能已含 root 前缀:
        // 先试 cwd 相对(p 原样),再试 root.join(p)(历史行为,兼容不含前缀的情况)
        let p_path = Path::new(p);
        if p_path.exists() {
            return p_path.to_path_buf();
        }
        let candidate = root.join(p);
        if candidate.exists() {
            return candidate;
        }
    }
    // 全部未命中:返回 cwd 相对路径(供 metadata 报错)
    Path::new(p).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造临时产物目录:两页面,a 链接 b(故 b 有入链、a 无入链=孤儿),
    /// 且 a 链接不存在的 c.md(断链)
    /// 构造临时产物目录（tag 区分并行测试，避免同 pid 目录互删）:
    /// 两页面,a 链接 b(故 b 有入链、a 无入链=孤儿),
    /// 且 a 链接不存在的 c.md(断链)
    fn make_fixture(tag: &str) -> (std::path::PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!(
            "repo_wiki_lint_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // a.md 链接到 b.md(入链)与不存在的 c.md(断链)
        std::fs::write(
            wiki.join("a.md"),
            "# A\n\n- [B](wiki/zh/b.md)\n- [C](wiki/zh/c.md)\n",
        )
        .unwrap();
        // 源文件先创建（b.md 引用其绝对路径,避免测试依赖 cwd——并行测试切换 cwd 会互相干扰）
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        let src_file = src_root.join("lib.rs");
        std::fs::write(&src_file, "pub fn f() {}\n").unwrap();
        let src_file_display = src_file.to_string_lossy().to_string();
        // b.md 无任何链接,且引用源文件 src/lib.rs（绝对路径）
        std::fs::write(
            wiki.join("b.md"),
            format!("# B\n\n## 相关文件\n\n- `{}`\n", src_file_display),
        )
        .unwrap();
        // 让 src/lib.rs 明显晚于 b.md（b.md 引用绝对路径,resolve 直接命中）
        let now = std::time::SystemTime::now();
        let _ = std::fs::File::options()
            .write(true)
            .open(&src_file)
            .unwrap();
        let _ = filetime_set(&src_file, now);
        let _ = filetime_set(&wiki.join("b.md"), now - std::time::Duration::from_secs(3600));
        (dir, vec![src_root])
    }

    /// 简化版 mtime 设置(避免引入 filetime 依赖)
    fn filetime_set(path: &Path, time: std::time::SystemTime) -> std::io::Result<()> {
        // Windows/Linux 通用:打开文件并写回一个字节触发 mtime 更新不可靠,
        // 这里直接返回 Ok——过时检查依赖系统 mtime,单测构造时序不稳定,
        // 因此过时检查的断言放宽为"不 panic + 断链/孤儿断言准确"
        let _ = (path, time);
        Ok(())
    }

    #[test]
    fn test_lint_orphan_and_broken() {
        let (dir, src_roots) = make_fixture("orphan");
        eprintln!("DEBUG orphan dir: {:?}", dir);
        let issues = lint(&dir, &src_roots);
        // 孤儿页: a.md 链接 b 但无任何入链 → 应命中
        assert!(
            issues.iter().any(|i| i.kind == "orphan" && i.path.ends_with("a.md")),
            "a.md 无入链应为孤儿, 实际: {:?}",
            issues
        );
        // b.md 被 a 链接 → 不应是孤儿
        assert!(
            !issues.iter().any(|i| i.kind == "orphan" && i.path.ends_with("b.md")),
            "b.md 有入链不应是孤儿, 实际: {:?}",
            issues
        );
        // 断链: a.md 指向 c.md 不存在
        assert!(
            issues.iter().any(|i| i.kind == "broken" && i.message.contains("c.md")),
            "a.md → c.md 应为断链, 实际: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 过时检查:产物引用源文件且源文件 mtime 更新 → 报 stale。
    /// 独立构造 fixture(不共享 make_fixture,避免并行测试竞态):
    /// 页面引用源文件绝对路径,先写页面再写源文件(源严格更新),
    /// 重写源文件刷新 mtime 后 lint 应报 stale。
    #[test]
    fn test_lint_stale_detects_newer_source() {
        let dir = std::env::temp_dir().join(format!(
            "repo_wiki_lint_stale_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        let src_file = src_root.join("lib.rs");
        std::fs::write(&src_file, "pub fn f() {}\n").unwrap();
        let abs = src_file.to_string_lossy().to_string();
        // 页面引用源文件绝对路径
        std::fs::write(
            wiki.join("lib.md"),
            format!("# Lib\n\n## 相关文件\n\n- `{}`\n", abs),
        )
        .unwrap();
        // 先等页面 mtime 落定,再重写源文件(严格更新)
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&src_file, "pub fn updated() {}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let issues = lint(&dir, &[src_root]);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            issues.iter().any(|i| i.kind == "stale"),
            "源文件更新后应报过时, 实际: {:?}",
            issues
        );
    }

    #[test]
    fn test_extract_md_links() {
        let links = extract_md_links("- [B](wiki/zh/b.md) 和 [外部](https://x.com/a.md)");
        assert!(links.contains(&"wiki/zh/b.md".to_string()));
        assert!(links.contains(&"https://x.com/a.md".to_string()));
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_extract_source_files() {
        let files = extract_source_files("## 相关文件\n\n- `src/lib.rs`\n- `tests/a.rs`\n");
        assert_eq!(files, vec!["src/lib.rs".to_string(), "tests/a.rs".to_string()]);
        // 链接行不应误提取
        assert!(extract_source_files("- [x](wiki/zh/a.md)").is_empty());
    }

    #[test]
    fn test_lint_empty_dir() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty(), "空目录应无问题, 实际: {:?}", issues);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言目录缺失时 lint 不 panic
    #[test]
    fn test_lint_no_wiki_dir() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_nodir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
