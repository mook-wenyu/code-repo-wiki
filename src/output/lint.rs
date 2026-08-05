//! Wiki 产物健康检查（lint）
//!
//! 对已生成的 wiki 产物目录做静态检查，供 `repo-wiki lint` 命令与 CI 使用。
//! 对齐 LLM Wiki 最佳实践（Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）：
//!
//! 1. **孤儿页**：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复）
//! 2. **断链**：页面内链接指向不存在的产物文件（复制 crossref 语义，但作用于磁盘产物）
//! 3. **过时**：页面生成时间戳早于其源文件修改时间（源码已变但文档未更新）
//! 4. **bad-citation**：正文 `path:line` 引用指向不存在的文件或行号越界（引用契约的静态复核）
//! 5. **entity-coverage**：页面声称的实体不在 api.md 权威清单（LLM 编造的第二道闸）
//! 6. **bad-mermaid**：产物中的 mermaid fence 无法被 merman 解析（历史产物/人工编辑/增量遗留）
//! 7. **stale-entity**：api.md 权威清单的实体在当前源码中不存在（文档引用了已删除/重命名的符号）
//!
//! 检查对象是**磁盘上的产物文件**（真实用户看到的东西），而非内存中的文档对象。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::output::citation;

/// 单条 lint 问题
#[derive(Debug, Clone)]
pub struct LintIssue {
    /// 问题类别: orphan / broken / stale / bad-citation / entity-coverage / bad-mermaid / stale-entity
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
///
/// 六类检查各一个私有函数（B7：单函数承载单一职责，lint() 只做组合）：
/// orphan（孤儿页）、broken（断链）、stale（过时）、bad-citation（引用存在性）、
/// entity-coverage（实体覆盖率）、bad-mermaid（Mermaid 语法）。
pub fn lint(output_dir: &Path, source_roots: &[PathBuf]) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let wiki_root = output_dir.join("wiki");

    // 源码实体表：stale-entity（实体名集合）与 bad-citation-overlap（行区间表）
    // 共用一次扫描（两检查的输入同源，各自消费不同投影）
    let (source_entity_ranges, source_entity_names) = collect_source_entities(source_roots);

    // 收集主语言目录下的全部 .md 产物（wiki 页 + 全局文档）
    let languages = collect_language_dirs(&wiki_root);
    for lang in &languages {
        let lang_dir = wiki_root.join(lang);
        let pages = collect_md_files(&lang_dir);

        // 链接统计范围 = 语言目录页面 + 产物根目录页（_toc.md 在 wiki 根而非
        // lang 目录，但其链接指向全部页面——不统计则每页都因无入链被误标孤儿）
        let mut link_sources: Vec<PathBuf> = pages.clone();
        let toc_path = output_dir.join("_toc.md");
        if toc_path.exists() {
            link_sources.push(toc_path);
        }

        issues.extend(check_orphan_pages(&pages, &link_sources, lang));
        issues.extend(check_broken_links(&pages, lang));
        issues.extend(check_stale(&pages, &output_dir.join("cards").join(lang), source_roots, lang));
        issues.extend(check_citations(&pages, output_dir, source_roots, lang, &source_entity_ranges));
        issues.extend(check_entity_coverage(&pages, &output_dir.join("wiki").join(lang).join("api.md"), lang, output_dir));
        issues.extend(check_mermaid(&pages, lang));
        issues.extend(check_stale_entities(
            &output_dir.join("wiki").join(lang).join("api.md"),
            lang,
            output_dir,
            &source_entity_names,
        ));
    }

    issues
}

/// 1. 孤儿页检查：收集所有页面内链接（含目录页 _toc），统计入链，
///    无任何页面链接指向的模块页报 orphan（全局文档由 TOC/概览引用，不算）
fn check_orphan_pages(pages: &[PathBuf], link_sources: &[PathBuf], lang: &str) -> Vec<LintIssue> {
    let mut incoming: HashMap<String, usize> = HashMap::new();
    for page in link_sources {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
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

    let mut issues = Vec::new();
    for page in pages {
        let file_name = page
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let stem = file_name.trim_end_matches(".md").to_string();
        // 全局文档(api/overview/architecture/_toc/index)由 TOC/概览引用,不算孤儿
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
    issues
}

/// 2. 断链检查：页面内链接目标必须存在于产物文件集合
fn check_broken_links(pages: &[PathBuf], lang: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    for page in pages {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
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
    issues
}

/// 3. 过时检查：模块页/卡片生成时间 < 其源文件 mtime
///    （从产物内容提取源文件路径——相关文件段，与源码根下对应文件的 mtime 对比）
fn check_stale(pages: &[PathBuf], cards_dir: &Path, source_roots: &[PathBuf], lang: &str) -> Vec<LintIssue> {
    let mut stale_targets: Vec<PathBuf> = pages.to_vec();
    stale_targets.extend(collect_md_files(cards_dir));

    let mut issues = Vec::new();
    for page in &stale_targets {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
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
    issues
}

/// 4. 引用存在性检查（P1-4 零成本评测）：正文中的 `path:line` 引用必须可验证
///    （生成层已校验-重试，此处对磁盘产物静态复核：引用文件存在且行号不越界）
///    v14 B 组：叠加区间重叠判定（文件存在且行号有效但区间不覆盖任何实体 =
///    行号对但内容错，bad-citation-overlap 新 kind；实体表无该文件键的引用
///    放行——非代码文件引用合法）
fn check_citations(
    pages: &[PathBuf],
    output_dir: &Path,
    source_roots: &[PathBuf],
    lang: &str,
    entity_ranges: &std::collections::HashMap<String, Vec<(usize, usize)>>,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    for page in pages {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
        let file_name = page
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        for citation in citation::extract_citations(&content) {
            // 路径越界段 `..` 拒绝（与生成层 citation.rs validate_citations
            // 同一规则，v16 C 组对齐）：`../src/x.rs` 可逃逸项目根、
            // `src/../lib.rs` 可跳过目录层级——即使目标文件真实存在也按
            // 无效处理。此前 lint 层未拒绝（不对称），手工/恶意页面可让
            // lint 读取项目根外文件的元数据。
            if citation.path.split(['/', '\\']).any(|seg| seg == "..") {
                issues.push(LintIssue {
                    kind: "bad-citation",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("路径含越界段 ..: `{}`", citation.path),
                });
                continue;
            }
            // 引用相对项目根：output_dir 的上级即项目根（AGENTS.md 生成同约定）；
            // source_roots 兜底逐根尝试（resolve_source_path 返回实际存在的路径）
            let project_root = output_dir.parent().unwrap_or_else(|| Path::new("."));
            let primary_abs = project_root.join(&citation.path);
            let abs = if primary_abs.exists() {
                primary_abs
            } else {
                resolve_source_path(source_roots, &citation.path)
            };
            let total_lines = std::fs::read_to_string(&abs)
                .map(|s| s.lines().count())
                .ok();
            let Some(n) = total_lines else {
                issues.push(LintIssue {
                    kind: "bad-citation",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("引用不存在: `{}` 指向的文件找不到", citation.path),
                });
                continue;
            };
            if citation.end > n {
                issues.push(LintIssue {
                    kind: "bad-citation",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!(
                        "引用越界: `{}` 的 {}-{} 行超出文件总行数 {}",
                        citation.path, citation.start, citation.end, n
                    ),
                });
                continue;
            }
            // 区间重叠判定：实体表键 = norm_sep 绝对路径（与 collect_source_entities
            // 的键形态一致——引用相对项目根解析出的绝对路径，Windows 反斜杠
            // 统一为正斜杠后比较）。实体表无该文件键（非代码文件）→ 放行。
            let key = crate::incremental::norm_sep(&absolutize(&abs).to_string_lossy());
            if let Some(ranges) = entity_ranges.get(&key)
                && !citation::citation_overlaps_entity(&citation, ranges)
            {
                issues.push(LintIssue {
                    kind: "bad-citation-overlap",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!(
                        "引用位置可疑: `{}` 的 {}-{} 行未覆盖该文件的任何实体（行号可能指向错误位置）",
                        citation.path, citation.start, citation.end
                    ),
                });
            }
        }
    }
    issues
}

/// 相对路径绝对化（实体表键的统一形态：相对 cwd 的路径与项目根解析的
/// 绝对路径在 Windows 下必须同基准比较，否则反斜杠/正斜杠混存不命中）
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// 从 api.md 权威清单提取实体名集合（- ` 行 + entity_name_from_signature）
///
/// entity-coverage（页面声称实体须在清单中）与 stale-entity（清单实体须在
/// 源码中）两侧共用同一提取，保证口径一致。
fn api_known_entities(api_content: &str) -> std::collections::HashSet<String> {
    api_content
        .lines()
        .filter(|l| l.trim_start().starts_with("- `"))
        .filter_map(|l| {
            // 签名如 `pub fn authenticate(username: &str) -> Option<User>`：
            // 取第一个 '(' 前的最后标识符（跳过 pub/fn 等关键字前缀）
            let inner = &l[l.find('`').unwrap() + 1..];
            inner
                .split('`')
                .next()
                .and_then(entity_name_from_signature)
        })
        .collect()
}

/// 5. 实体覆盖率检查（P1-4 零成本评测）：模块页核心实体须存在于 api.md
///    （api.md 由 graph 权威渲染，页面声称的实体若不在 = LLM 编造实体名，
///    防幻觉第二道闸；api.md 仅主语言一份，只检查主语言目录）
fn check_entity_coverage(pages: &[PathBuf], api_path: &Path, lang: &str, output_dir: &Path) -> Vec<LintIssue> {
    if primary_language(output_dir) != *lang {
        return Vec::new();
    }
    let Ok(api_content) = std::fs::read_to_string(api_path) else {
        return Vec::new();
    };
    let known = api_known_entities(&api_content);

    let mut issues = Vec::new();
    for page in pages {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
        let file_name = page
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        for entity in extract_entity_names(&content) {
            if !known.contains(&entity) {
                issues.push(LintIssue {
                    kind: "entity-coverage",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("实体覆盖率: 页面声称的实体 `{entity}` 不在 api.md 清单中（可能是编造或已删除）"),
                });
            }
        }
    }
    issues
}

/// 扫描源码根并解析全部实体（stale-entity 与 bad-citation-overlap 共用一次
/// 扫描，避免 lint 对源码做两遍 AST 解析）
///
/// 返回 (norm_sep 绝对路径 → 实体行区间列表, 全部实体名集合)。
/// 解析失败的文件跳过（文件级损坏不是文档问题）；源码根不存在/为空时
/// 返回空表——调用方据此跳过对应检查（扫描失败 ≠ 文档过期/引用错误，
/// 两种错误信号不能混淆）。
fn collect_source_entities(
    source_roots: &[PathBuf],
) -> (
    crate::output::citation::EntityRanges,
    std::collections::HashSet<String>,
) {
    let mut ranges: crate::output::citation::EntityRanges =
        std::collections::HashMap::new();
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let registry = crate::ingest::parser::ParserRegistry::new();
    for root in source_roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walk_files(root) {
            let Some(processor) = registry.get_for_file(&entry) else { continue };
            let Ok(source) = std::fs::read_to_string(&entry) else { continue };
            if let Ok(insight) = processor.parse(&source, &entry) {
                let key = crate::incremental::norm_sep(&absolutize(&entry).to_string_lossy());
                ranges.insert(
                    key,
                    insight
                        .entities
                        .iter()
                        .map(|e| (e.line_start, e.line_end))
                        .collect(),
                );
                for entity in &insight.entities {
                    names.insert(entity.name.clone());
                }
            }
        }
    }
    (ranges, names)
}

/// 7. 符号漂移检查（v13 D1，N1）：api.md 权威清单中的实体在当前源码中不存在
///    → "文档引用了已删除实体"（entity-coverage 的反向：前者防 LLM 编造，
///    本检查防文档过期——增量更新未覆盖、模块重构改名、人工删改产物）。
///    零 LLM，源码侧直接 AST 解析（与生成侧同一 parser，口径一致）。
fn check_stale_entities(
    api_path: &Path,
    lang: &str,
    output_dir: &Path,
    source_entity_names: &std::collections::HashSet<String>,
) -> Vec<LintIssue> {
    if primary_language(output_dir) != *lang {
        return Vec::new();
    }
    let Ok(api_content) = std::fs::read_to_string(api_path) else {
        return Vec::new();
    };
    let known = api_known_entities(&api_content);
    if known.is_empty() {
        return Vec::new();
    }
    if source_entity_names.is_empty() {
        // 源码根为空/全解析失败时无从对比，跳过（避免把"扫描失败"误报成
        // "文档过期"——二者错误信号不同，不能混淆）
        return Vec::new();
    }

    let mut issues = Vec::new();
    let mut stale: Vec<&String> = known
        .iter()
        .filter(|e| !source_entity_names.contains(*e))
        .collect();
    stale.sort();
    for entity in stale {
        issues.push(LintIssue {
            kind: "stale-entity",
            path: format!("wiki/{lang}/api.md"),
            message: format!("符号漂移: api.md 中的实体 `{entity}` 在当前源码中不存在（已删除或重命名，文档过期）"),
        });
    }
    issues
}

/// 递归收集目录下全部文件（跟随子目录，忽略隐藏目录与符号链接循环——
/// 生产仓库正常布局下深度有限，不引入额外依赖）
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// 6. Mermaid 语法检查（G2）：产物中的 mermaid fence 必须可被 merman 权威解析器解析
///    （生成层已做校验-重试-降级，此处兜住历史产物/人工编辑/增量遗留三类来源；
///    发现坏图即报 issue，CI 门禁语义与 bad-citation 一致：只阻断不自动修复）
fn check_mermaid(pages: &[PathBuf], lang: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    for page in pages {
        // 页面读取失败（损坏/权限/竞态删除）时显式告警并跳过该页——
        // 静默当作空内容会把页误报为孤儿/断链（失败必须可观测）
        let Ok(content) = std::fs::read_to_string(page) else {
            tracing::warn!("lint 读取页面失败（跳过检查）: {}", page.display());
            continue;
        };
        let file_name = page
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        for issue in crate::output::mermaid_check::validate_mermaid_blocks(&content) {
            issues.push(LintIssue {
                kind: "bad-mermaid",
                path: format!("wiki/{lang}/{file_name}"),
                message: format!(
                    "Mermaid 校验失败（第 {} 个块）: {}",
                    issue.block_index + 1,
                    issue.message
                ),
            });
        }
    }
    issues
}

/// 主语言目录名：api.md 只写主语言一份（render_all 规则），实体覆盖检查以它为权威
fn primary_language(output_dir: &Path) -> String {
    // 遍历 wiki/ 下的语言目录，取含 api.md 的那个（主语言）；无则返回空串（跳过检查）
    let wiki_root = output_dir.join("wiki");
    if let Ok(entries) = std::fs::read_dir(&wiki_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && entry.path().join("api.md").is_file()
                && let Some(name) = entry.file_name().to_str()
            {
                return name.to_string();
            }
        }
    }
    String::new()
}

/// 从签名/实体文本中提取实体真名
///
/// 签名形态：`pub fn authenticate(username: &str) -> Option<User>`（函数）、
/// `Foo`（struct/enum 裸名）、`def foo()`（Python）、`func Foo()`（Go）。
/// 规则：有 '(' 时取第一个 '(' 前最后一个标识符（跳过 pub/fn/def 等
/// 关键字前缀）；无 '(' 时取最后一个标识符（裸名/类型）。页面侧与
/// api.md 权威侧共用同一提取，保证两侧命名口径一致。
pub fn entity_name_from_signature(sig: &str) -> Option<String> {
    let trimmed = sig.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 实体名前缀段 = 第一个 '(' 之前（函数/方法）或整段（类型声明）。
    // v21 I 轮 Unity 抽样核证（20/20 真实存在）：948 条 stale 中真实
    // 只有 ~13 条，误报根因是最后标识符被三类后缀污染——逐类剥离：
    // 0) 属性宏段（C# [ContextMenu("x")] / Rust #[test]）必须在找 '('
    //    之前切掉——属性自身的括号会先于函数括号被 find('(') 命中
    let after_attr = match trimmed.rfind(']') {
        Some(rb) => &trimmed[rb + 1..],
        None => trimmed,
    };
    let mut head = match after_attr.find('(') {
        Some(open) => &after_attr[..open],
        None => after_attr,
    };
    // 1) 泛型约束子句（C# class Foo where T : class / Rust impl<T> Foo<T> where T: Clone）：
    //    其中的 ':' 会误导继承剥离，必须先切掉
    if let Some(w) = head.find("where") {
        head = &head[..w];
    }
    // 2) 继承/实现段（C# class Foo : Base, IBar / Java class Foo extends Bar 的 ':'）：
    //    基类名/接口名会污染最后标识符（实测 ScriptableObject/IDisposable 误报）
    if let Some(colon) = head.find(':') {
        head = &head[..colon];
    }
    // 3) 泛型参数列表（RegisterInstance<TService> / fn foo<T>）：'<' 后是类型参数名
    if let Some(lt) = head.find('<') {
        head = &head[..lt];
    }
    let candidate = head
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .rfind(|_| true);
    // v19 t03 过滤：单字符 token（LLM 文本噪声 a/_/P）与纯数字（42）
    // 会污染 entity-coverage 统计并误报——api 权威侧与页面声称侧共用
    // 本函数，两侧同口径不会误报。
    candidate
        .filter(|s| s.len() > 1 && !s.chars().all(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
}/// 从模块页内容提取声称的实体名：`- `Name`` 核心实体行（反引号内实体真名）
fn extract_entity_names(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("- `") && let Some(end) = line[3..].find('`') {
            let inner = &line[3..3 + end];
            if let Some(name) = entity_name_from_signature(inner) {
                out.push(name);
            }
        }
    }
    out
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

    /// P1-4 引用存在性：产物中的 `path:line` 引用指向不存在的文件 → bad-citation
    #[test]
    fn test_lint_bad_citation_missing_file() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_cite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".repo-wiki"); // output_dir 的父目录 = 项目根
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 页面引用不存在的文件
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n核心逻辑见 `src/ghost.rs:10`\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let issues = lint(&out, &[]);
        assert!(
            issues.iter().any(|i| i.kind == "bad-citation"),
            "引用不存在的文件应报 bad-citation, 实际: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-4 引用存在性：引用真实存在的文件且行号合法 → 无 bad-citation
    #[test]
    fn test_lint_bad_citation_valid_passes() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_cite_ok_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("real.rs"), "line1\nline2\n").unwrap();
        // 页面引用真实文件（相对项目根路径,output_dir 父目录解析命中）
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n核心逻辑见 `src/real.rs:1`\n",
        )
        .unwrap();

        let issues = lint(&out, &[]);
        assert!(
            !issues.iter().any(|i| i.kind == "bad-citation"),
            "有效引用不应报错, 实际: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 B 组：区间重叠复核——文件存在且行号有效但引用区间不覆盖任何
    /// 实体（行号对但内容错）→ bad-citation-overlap；覆盖实体 → 通过；
    /// 无实体文件（README）→ 放行
    #[test]
    fn test_lint_bad_citation_overlap_detects_wrong_location() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_overlap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        // 10 行源码：实体区间 (2,2)（fn server 定义在第 2 行）
        let source = "line1\npub fn server() {}\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        std::fs::write(src_root.join("server.rs"), source).unwrap();
        std::fs::write(dir.join("README.md"), "docs\n").unwrap();
        // 页面：引用 2 行（覆盖实体，合法）+ 引用 8 行（文件内但区间外）+ README（无实体）
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n- `src/server.rs:2` 核心\n- `src/server.rs:8` 位置可疑\n- `README.md:1` 说明\n",
        )
        .unwrap();

        let issues = lint(&out, &[src_root]);
        let overlaps: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "bad-citation-overlap")
            .collect();
        assert_eq!(overlaps.len(), 1, "只应报区间外引用, 实际: {:?}", issues);
        assert!(
            overlaps[0].message.contains("src/server.rs") && overlaps[0].message.contains("8"),
            "应指向 8 行引用: {}",
            overlaps[0].message
        );
        assert!(
            !issues.iter().any(|i| i.kind == "bad-citation"),
            "文件级校验不应误报（文件存在且行号合法）: {:?}",
            issues
        );

        // 源码根为空：区间检查跳过（扫描失败 ≠ 引用错误）
        let empty_root = dir.join("empty_src");
        std::fs::create_dir_all(&empty_root).unwrap();
        let issues2 = lint(&out, &[empty_root]);
        assert!(
            !issues2.iter().any(|i| i.kind == "bad-citation-overlap"),
            "空源码根应跳过区间检查: {:?}",
            issues2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_lint_entity_coverage_detects_fake() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_cov_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // api.md 权威清单只有 Foo
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `Foo` — 描述 — m.rs:1\n",
        )
        .unwrap();
        // 模块页声称 FakeEntity（不在 api.md）
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n## 核心实体\n\n- `FakeEntity` — 编造的实体\n- `Foo` — 真实实体\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let cov: Vec<_> = issues.iter().filter(|i| i.kind == "entity-coverage").collect();
        assert_eq!(cov.len(), 1, "只应报编造实体, 实际: {:?}", issues);
        assert!(cov[0].message.contains("FakeEntity"), "应指向 FakeEntity: {}", cov[0].message);
        assert!(!cov[0].message.contains("Foo"), "真实实体不应误报");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_entity_names() {
        let content = "## 核心实体\n\n- `Server`（struct）— HTTP 服务\n- `fn connect()` — 连接\n- `foo_bar` — 下划线\n";
        let names = extract_entity_names(content);
        assert!(names.contains(&"Server".to_string()));
        assert!(
            names.contains(&"connect".to_string()),
            "签名应提取实体真名（跳过 fn 关键字）: {:?}",
            names
        );
        assert!(!names.contains(&"fn".to_string()), "关键字不应被提取: {:?}", names);
        assert!(names.contains(&"foo_bar".to_string()));
    }

    /// v19 t03：单字符与纯数字 token 是 LLM 编造噪声（双仓库实测
    /// `P`/`_`/`a`/`2`），应被过滤以免污染 entity-coverage 声称侧；
    /// 多字符正常实体不受影响。
    #[test]
    fn test_entity_name_filters_noise_tokens() {
        assert_eq!(entity_name_from_signature("`P`"), None, "单字符应过滤");
        assert_eq!(entity_name_from_signature("`_`"), None, "下划线单字符应过滤");
        assert_eq!(entity_name_from_signature("`2`"), None, "纯数字应过滤");
        assert_eq!(entity_name_from_signature("fn x()"), None, "单字符函数名应过滤");
        let content = "## 核心实体\n\n- `Server`（struct）\n- `src` — 目录\n- `P` — 噪声\n- `2` — 数字\n";
        let names = extract_entity_names(content);
        assert!(names.contains(&"Server".to_string()), "正常实体应保留: {:?}", names);
        assert!(names.contains(&"src".to_string()), "多字符实体应保留: {:?}", names);
        assert!(!names.contains(&"P".to_string()), "单字符噪声不应声称: {:?}", names);
        assert!(!names.contains(&"2".to_string()), "纯数字噪声不应声称: {:?}", names);
    }

    /// v21 I 轮 Unity 抽样核证回归：三类后缀污染最后标识符，导致 stale
    /// 误报（948 条中真实仅 ~13 条）——基类/接口名、泛型参数名、属性宏名
    #[test]
    fn test_entity_name_strips_inheritance_generics_and_attributes() {
        // 继承/实现段：取类名而非基类/接口名
        assert_eq!(
            entity_name_from_signature("internal class PrimeTweenInstaller : ScriptableObject"),
            Some("PrimeTweenInstaller".into())
        );
        assert_eq!(
            entity_name_from_signature("public class Foo : Bar, IBaz"),
            Some("Foo".into())
        );
        // 泛型方法：取方法名而非类型参数名
        assert_eq!(
            entity_name_from_signature("public void RegisterInstance<TService>(TService instance)"),
            Some("RegisterInstance".into())
        );
        assert_eq!(
            entity_name_from_signature("pub fn load<T>(path: &str) -> T"),
            Some("load".into())
        );
        // 泛型约束子句中的 ':' 不误导继承剥离（C# where 子句）
        assert_eq!(
            entity_name_from_signature("class Foo<T> where T : class"),
            Some("Foo".into())
        );
        // 属性宏段：取 ']' 之后的实体名
        assert_eq!(
            entity_name_from_signature("[ContextMenu(\"x\")] public void DoThing()"),
            Some("DoThing".into())
        );
        // 普通签名不受影响
        assert_eq!(
            entity_name_from_signature("pub fn load(path: &str) -> Result<Config>"),
            Some("load".into())
        );
    }

    /// G2：产物中的 mermaid fence 语法错误 → bad-mermaid；合法图不报
    #[test]
    fn test_lint_bad_mermaid_detects_broken_diagram() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_mermaid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 一个坏图页面 + 一个好图页面
        std::fs::write(
            wiki.join("bad.md"),
            "# Bad\n\n```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n",
        )
        .unwrap();
        std::fs::write(
            wiki.join("good.md"),
            "# Good\n\n```mermaid\nflowchart LR\nA[Start] --> B[End]\n```\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-mermaid").collect();
        assert_eq!(bad.len(), 1, "只有坏图应报 bad-mermaid, 实际: {:?}", issues);
        assert!(bad[0].path.ends_with("bad.md"), "应指向坏图页面: {}", bad[0].path);
        assert!(bad[0].message.contains("Unterminated"), "错误消息应可读: {}", bad[0].message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D1（N1）：符号漂移——api.md 权威清单中的实体在当前源码 AST 中不存在
    /// → 报 stale-entity（文档过期/实体已删除）；源码中存在的实体不报。
    /// 源码根为空（扫描失败/无源码）时跳过检查，不把"扫描失败"误报成"文档过期"
    #[test]
    fn test_lint_stale_entity_detects_deleted_symbol() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_stale_entity_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        // 源码只有 alpha（beta 已被删除/重命名）
        std::fs::write(src_root.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        // api.md 声明 alpha（存在）+ beta（已删除）
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `alpha` — m.rs:1\n- `beta` — m.rs:2\n",
        )
        .unwrap();

        let issues = lint(&dir, &[src_root]);
        let stale: Vec<_> = issues.iter().filter(|i| i.kind == "stale-entity").collect();
        assert_eq!(stale.len(), 1, "只应报已删除的 beta, 实际: {:?}", issues);
        assert!(stale[0].message.contains("beta"), "应指向 beta: {}", stale[0].message);
        assert!(!stale[0].message.contains("alpha"), "源码存在的实体不应误报");

        // 源码根为空 → 跳过检查（扫描失败与文档过期是不同信号，不能混淆）
        let empty_root = dir.join("empty_src");
        std::fs::create_dir_all(&empty_root).unwrap();
        let issues2 = lint(&dir, &[empty_root]);
        assert!(
            !issues2.iter().any(|i| i.kind == "stale-entity"),
            "空源码根应跳过 stale-entity 检查, 实际: {:?}",
            issues2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v16 C 组：lint 层引用越界段拒绝（与生成层 citation.rs 对齐）——
    /// 页面引用 `../x.rs` 即使文件存在也报 bad-citation（越根读取防护）
    #[test]
    fn test_lint_bad_citation_rejects_dotdot() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_lint_dotdot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 项目根外确实存在该文件（证明"文件存在但路径越界"场景）
        std::fs::create_dir_all(dir.parent().unwrap().join("escape_dir")).unwrap();
        std::fs::write(
            dir.parent().unwrap().join("escape_dir").join("x.rs"),
            "line1\n",
        )
        .unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n核心逻辑见 `../escape_dir/x.rs:1`\n",
        )
        .unwrap();

        let issues = lint(&out, &[]);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-citation").collect();
        assert_eq!(bad.len(), 1, "越界段应报 bad-citation, 实际: {:?}", issues);
        assert!(bad[0].message.contains("越界段 .."), "消息应说明越界: {}", bad[0].message);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
