//! Wiki 产物健康检查（lint）
//!
//! 对已生成的 wiki 产物目录做静态检查，供 `code-repo-wiki lint` 命令与 CI 使用。
//! 对齐 LLM Wiki 最佳实践（Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）：
//!
//! 1. **孤儿页**：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复）
//! 2. **断链**：页面内链接指向不存在的产物文件（复制 crossref 语义，但作用于磁盘产物）
//! 3. **过时**：页面生成时间戳早于其源文件修改时间（源码已变但文档未更新）
//! 4. **bad-citation**：正文 `path:line` 引用指向不存在的文件或行号越界（引用契约的静态复核）
//!
//! 4b. **bad-vctx**：正文 `[[vctx:path#L-a-L-b@hash8]]` 手工标记做 5 步哈希只读校验（vericontext 协议，人工文档护栏：t05 决议不引入生成契约，只识别并校验已有标记）
//!
//! 5. **entity-coverage**：页面声称的实体不在 api.md 权威清单（LLM 编造的第二道闸；api.md 的模块名（## 节标题）属已知名——合成页按模块名引用不是实体声称）
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
    /// 问题类别: orphan / broken / stale / bad-citation / bad-vctx / entity-coverage / bad-mermaid / stale-entity
    pub kind: &'static str,
    /// 问题文件相对路径（相对 output_dir）
    pub path: String,
    /// 问题描述
    pub message: String,
}

/// 执行 lint 检查，返回所有发现的问题（无问题返回空列表）
///
/// `output_dir` 为产物根目录（config.output_dir()），
/// `source_roots` 为源码扫描根列表（用于过时检查的源文件 mtime 对比）。
///
/// 各类检查各一个私有函数（B7：单函数承载单一职责，lint() 只做组合）：
/// orphan（孤儿页）、broken（断链）、stale（过时）、bad-citation（引用存在性）、
/// bad-vctx（vctx 标记哈希）、entity-coverage（实体覆盖率）、bad-mermaid（Mermaid 语法）。
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
        issues.extend(check_vctx_tokens(&pages, output_dir, source_roots, lang));
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
    // 同时检查 wiki 页与 cards 卡片；逐项携带来源目录名（"wiki"/"cards"）,
    // 否则 path 恒标 wiki/ 会把卡片误标成 wiki 路径（真实卡片在 cards/{lang}/ 下）
    let mut stale_targets: Vec<(PathBuf, &'static str)> =
        pages.iter().map(|p| (p.clone(), "wiki")).collect();
    stale_targets.extend(
        collect_md_files(cards_dir)
            .into_iter()
            .map(|p| (p, "cards")),
    );

    let mut issues = Vec::new();
    for (page, dir) in &stale_targets {
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
                    path: format!("{dir}/{lang}/{file_name}"),
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
            // 区间重叠判定：实体表键 = citation_key（绝对路径、过滤 `./` 段、
            // norm_sep 统一分隔符——与 collect_source_entities 的键同形态；
            // 引用相对项目根解析出的绝对路径）。实体表无该文件键（非代码
            // 文件）→ 放行。
            let key = citation_key(&abs);
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

/// vctx 标记（vericontext 协议，t05 源码级核对）：
/// `[[vctx:path#L-<start>-L-<end>@<hash8>]]`，path 相对项目根、不含 `#`/`]`，
/// start/end 为 1-based 包含行区间，hash8 为 SHA-256 前 8 位小写 hex。
#[derive(Debug, Clone, PartialEq, Eq)]
struct VctxToken {
    path: String,
    start: usize,
    end: usize,
    hash: String,
}

/// 从文本中扫描全部 `[[vctx:...]]` 标记并解析（手写扫描，与 citation.rs
/// 同风格，项目无 regex 依赖）。返回逐条解析结果：Err 携带"格式不完整"
/// 原因——`[[vctx:` 出现却无法完整解析 = 手写标记写坏，必须可观测。
fn extract_vctx_tokens(content: &str) -> Vec<Result<VctxToken, String>> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(pos) = rest.find("[[vctx:") {
        // token 尾部 = 第一个 "]]"（语法内 `]` 只出现在收尾，路径不含 `]`）
        let after = &rest[pos + 7..];
        let end = after.find("]]").map(|e| e + 2).unwrap_or(after.len());
        out.push(parse_vctx_token(&after[..end]));
        rest = &after[end..];
    }
    out
}

/// 单条 token 解析（"[[vctx:" 前缀已剥除）：`path#L-<start>-L-<end>@<hash8>]]`
fn parse_vctx_token(s: &str) -> Result<VctxToken, String> {
    let (path, rest) = s
        .split_once('#')
        .ok_or_else(|| "缺少 # 行区间段".to_string())?;
    if path.is_empty() || path.contains(']') {
        return Err("路径为空或含非法字符 ]".to_string());
    }
    let rest = rest
        .strip_prefix("L-")
        .ok_or_else(|| "行区间段应以 L- 开头".to_string())?;
    let (start_str, rest) = rest
        .split_once("-L-")
        .ok_or_else(|| "行区间缺 -L- 分隔".to_string())?;
    let start: usize = start_str
        .parse()
        .map_err(|_| "起始行号非数字".to_string())?;
    let (end_str, rest) = rest
        .split_once('@')
        .ok_or_else(|| "缺 @ 哈希分隔".to_string())?;
    let end: usize = end_str
        .parse()
        .map_err(|_| "结束行号非数字".to_string())?;
    let hash = rest
        .strip_suffix("]]")
        .ok_or_else(|| "哈希段后缺 ]] 收尾".to_string())?;
    if hash.len() != 8 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("哈希必须为 8 位十六进制".to_string());
    }
    Ok(VctxToken {
        path: path.to_string(),
        start,
        end,
        hash: hash.to_ascii_lowercase(),
    })
}

/// vctx 5 步哈希（对齐 vericontext src/core/file.ts readCanonicalText +
/// hashLineSpan，t05 Resolution 第 2 节源码级核对）：
/// 1. 严格 UTF-8 读取（read_to_string 失败即拒绝，不做字节替换）；
/// 2. EOL 归一化：`\r\n` 与裸 `\r` → `\n`（跨平台一致，vericontext 自评
///    "最重要的可移植性决策"；Rust lines() 等价处理 CRLF，但行哈希要求
///    归一化后再取区间，故显式替换）；
/// 3. 取 [start, end] 行区间（1-based 包含）；
/// 4. 行间 join("\n") 且无尾换行；
/// 5. SHA-256 hex 前 8 位小写（32 位截断够检测编辑，非安全边界）。
fn vctx_line_hash(source: &str, start: usize, end: usize) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let span = lines[start - 1..end].join("\n");
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(span.as_bytes());
    hex::encode(hasher.finalize())[..8].to_string()
}

/// 4b. vctx 只读校验（v28 t06）：产物中人工手写的 `[[vctx:path#L-a-L-b@hash8]]`
/// 标记做 5 步哈希校验（vericontext 协议）。t05 决议：vctx 是"写时哈希协议"
/// 而非生成契约，不要求 LLM 产出——本检查只兜住人工/工具写出的标记，与
/// bad-citation（结构校验）互补：存在性+行区间是"行号对"，哈希是"内容对"
/// （防引用内容漂移）。
fn check_vctx_tokens(
    pages: &[PathBuf],
    output_dir: &Path,
    source_roots: &[PathBuf],
    lang: &str,
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
        for token in extract_vctx_tokens(&content) {
            let token = match token {
                Ok(t) => t,
                Err(reason) => {
                    issues.push(LintIssue {
                        kind: "bad-vctx",
                        path: format!("wiki/{lang}/{file_name}"),
                        message: format!("vctx 标记格式不完整: {reason}"),
                    });
                    continue;
                }
            };
            // 路径越界段拒绝（与 check_citations 同一规则）：`..` 段可逃逸
            // 项目根，即使目标真实存在也按无效处理
            if token.path.split(['/', '\\']).any(|seg| seg == "..") {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("vctx 路径含越界段 ..: `{}`", token.path),
                });
                continue;
            }
            // 路径相对项目根解析（output_dir 的父目录），source_roots 兜底
            let project_root = output_dir.parent().unwrap_or_else(|| Path::new("."));
            let primary_abs = project_root.join(&token.path);
            let abs = if primary_abs.exists() {
                primary_abs
            } else {
                resolve_source_path(source_roots, &token.path)
            };
            // 严格 UTF-8 读取：失败 = 文件不存在或非 UTF-8（vericontext 同
            // fail-closed 语义：file_missing / invalid_utf8 均拒绝）
            let Ok(source) = std::fs::read_to_string(&abs) else {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("vctx 目标不存在或非 UTF-8: `{}`", token.path),
                });
                continue;
            };
            let total = source.lines().count();
            // 行区间有效性：1-based 包含区间，0 不是合法行号
            if token.start == 0 || token.start > token.end || token.end > total {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!(
                        "vctx 行区间越界: `{}` 的 {}-{} 行超出文件总行数 {}",
                        token.path, token.start, token.end, total
                    ),
                });
                continue;
            }
            let actual = vctx_line_hash(&source, token.start, token.end);
            if actual != token.hash {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!(
                        "vctx 哈希不匹配: `{}` 的 {}-{} 行内容已变更（现哈希 {actual}，标记为 {}）",
                        token.path, token.start, token.end, token.hash
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

/// 实体表键的统一定型（v23 B 组）：绝对化 + 过滤 `./` 段 + norm_sep。
///
/// include 通配符（`**/*.rs`）派生的源码根可能带 `./` 前缀（walk_files
/// 逐级 join 保留该段），而引用侧从项目根解析的绝对路径无此段——两侧
/// 键若不统一，实体表查询恒不命中，区间重叠检查静默失效（SA2 审计）。
/// `..` 段保留：引用侧已在上游拒绝越界段（check_citations），本函数
/// 只统一形态、不重复拦截。
fn citation_key(p: &Path) -> String {
    let mut cleaned = PathBuf::new();
    for comp in absolutize(p).components() {
        if matches!(comp, std::path::Component::CurDir) {
            continue;
        }
        cleaned.push(comp);
    }
    crate::incremental::norm_sep(&cleaned.to_string_lossy())
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

/// 从 api.md 提取模块名集合（`## ` 节标题 = 模块名，容器名而非叶子实体）。
/// entity-coverage 声称侧命中模块名也属已知名：合成页（architecture.md 等）
/// 按模块名引用模块（如 `src`、`src::storage`），不是叶子实体声称（P3 误报修复）
fn api_module_names(api_content: &str) -> std::collections::HashSet<String> {
    api_content
        .lines()
        .filter(|l| l.starts_with("## "))
        .filter_map(|l| {
            let name = l[3..].trim();
            (!name.is_empty()).then(|| name.to_string())
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
    // 模块名（api.md 的 ## 节标题）也纳入已知名：LLM 合成页（architecture.md
    // 等）会按模块名引用（如 `src`）。模块名是容器而非叶子实体，不在叶子
    // 清单中——修复前一律误报 entity-coverage（P3 已知噪声）
    let modules = api_module_names(&api_content);

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
        for entity in extract_entity_names(&content, &modules) {
            // 声称命中叶子实体清单或模块名（容器名）即非编造；其余报错
            if known.contains(&entity) || modules.contains(&entity) {
                continue;
            }
            issues.push(LintIssue {
                kind: "entity-coverage",
                path: format!("wiki/{lang}/{file_name}"),
                message: format!("实体覆盖率: 页面声称的实体 `{entity}` 不在 api.md 清单中（可能是编造或已删除）"),
            });
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
                let key = citation_key(&entry);
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
    //    之前切掉——属性自身的括号会先于函数括号被 find('(') 命中。
    //    只在前缀确为 '#'（Rust #[..]）或 '['（C# [..]）时剥离，否则不剥：
    //    函数签名里的切片类型 &[i64] 也含 ']'，无条件 rfind(']') 会把
    //    [i64] 的右括号误当属性段结尾（pub fn sum(values: &[i64]) -> i64
    //    被截成 ") -> i64"，末标识符提取出原生类型 i64 → 误报 stale-entity）。
    //    保留 rfind 以兼容叠放多段属性宏
    let after_attr = if trimmed.starts_with('#') || trimmed.starts_with('[') {
        match trimmed.rfind(']') {
            Some(rb) => &trimmed[rb + 1..],
            None => trimmed,
        }
    } else {
        trimmed
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
}

/// 提取 `- \`...\`` 声称行的反引号内文（非声称行返回 None）。
/// extract_entity_names 借它做模块名原文精确匹配（多段名提取后会被截断），
/// 不能只依赖 entity_name_from_signature 的提取结果
fn claimed_backtick_inner(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("- `")
        .and_then(|rest| rest.find('`').map(|end| &rest[..end]))
}

/// 从模块页内容提取声称的实体名：`- `Name`` 核心实体行（反引号内实体真名）。
/// `modules` 为 api.md 的模块名集合：原文精确命中模块名的声称行是模块引用
/// （容器名，如 `src`、`src::storage`）而非实体声称，先行剔除——多段名
/// `src::storage` 经 entity_name_from_signature（`::` 被当作继承段冒号）
/// 会截断为 `src`，必须按原文剔除（P3 误报修复）
fn extract_entity_names(content: &str, modules: &std::collections::HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let Some(inner) = claimed_backtick_inner(line) else { continue };
        if modules.contains(inner) {
            continue;
        }
        if let Some(name) = entity_name_from_signature(inner) {
            out.push(name);
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
            "code_repo_wiki_lint_{}_{}",
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
            "code_repo_wiki_lint_stale_{}",
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

    /// 回归：卡片 stale 的 issue path 必须标 `cards/` 前缀（修复前与页面
    /// 一样恒为 `wiki/`，展示层误标卡片路径；真实卡片在 cards/{lang}/ 下）。
    /// 页面 stale 仍标 `wiki/` 前缀，判定逻辑不受影响。
    #[test]
    fn test_lint_stale_card_path_uses_cards_prefix() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_card_path_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let cards = dir.join("cards").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&cards).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        let src_file = src_root.join("lib.rs");
        std::fs::write(&src_file, "pub fn f() {}\n").unwrap();
        let abs = src_file.to_string_lossy().to_string();
        // 页面与卡片各一,均引用同一源文件绝对路径（卡片文件名与页面不同,
        // 避免按文件名过滤时互相干扰）
        std::fs::write(
            wiki.join("lib.md"),
            format!("# Lib\n\n## 相关文件\n\n- `{}`\n", abs),
        )
        .unwrap();
        std::fs::write(
            cards.join("card.md"),
            format!("# Card\n\n## 相关文件\n\n- `{}`\n", abs),
        )
        .unwrap();
        // 先等页面/卡片 mtime 落定,再重写源文件(严格更新)
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&src_file, "pub fn updated() {}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let issues = lint(&dir, &[src_root]);
        let _ = std::fs::remove_dir_all(&dir);

        let card_stale: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "stale" && i.path.contains("card.md"))
            .collect();
        assert_eq!(
            card_stale.len(),
            1,
            "卡片应报过时且 path 含卡片名, 实际: {:?}",
            issues
        );
        assert!(
            card_stale[0].path.starts_with("cards/"),
            "卡片 stale 的 path 应以 cards/ 开头, 实际: {}",
            card_stale[0].path
        );

        let page_stale: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "stale" && i.path.contains("lib.md"))
            .collect();
        assert_eq!(
            page_stale.len(),
            1,
            "页面应报过时且 path 含页面名, 实际: {:?}",
            issues
        );
        assert!(
            page_stale[0].path.starts_with("wiki/"),
            "页面 stale 的 path 应以 wiki/ 开头, 实际: {}",
            page_stale[0].path
        );
    }

    /// v23 B 组防回归：include 通配（`**/*.rs`）派生的源码根带 `./` 段时，
    /// 区间重叠检查必须仍命中实体区间——修复前两侧键形态不一致（实体表
    /// 键含 `/./` 段、引用侧无），实体表查询恒空，检查静默失效。
    /// 同一 fixture 分别用 `./` 形态与常规形态的源码根，断言结果一致：
    /// 合法引用（落在实体区间）不误报，行号指向实体间隙的引用必报。
    #[test]
    fn test_lint_citation_overlap_survives_dot_slash_source_roots() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_dotslash_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        // 两个实体各占一行：f 在 1 行、g 在 2 行，第 3 行空白
        // （引用 3-3 落在文件内但不覆盖任何实体=间隙；若文件只有 2 行
        // 则 3-3 属越界，会在 overlap 判定前被 bad-citation 拦截）
        std::fs::write(src_root.join("lib.rs"), "pub fn f() {}\npub fn g() {}\n\n").unwrap();
        // 引用写相对项目根（= output_dir.parent()）的路径。注意必须含父
        // 目录前缀（如 code_repo_wiki_lint_dotslash_<pid>/src/lib.rs）——若只写
        // src/lib.rs，resolve_source_path 的 cwd 相对兜底会命中本仓库自己
        // 的 src/lib.rs（cwd 恰好有同名文件），实体表键恒不命中
        let rel = format!(
            "{}/src/lib.rs",
            dir.file_name().unwrap().to_string_lossy()
        );
        // a.md 引用 1-1 行（f 的实体区间内）→ 合法，不应报 overlap
        std::fs::write(wiki.join("a.md"), format!("# A\n\n- 源: {rel}:1-1\n")).unwrap();
        // b.md 引用 3-3 行（无实体覆盖）→ 应报 overlap
        std::fs::write(wiki.join("b.md"), format!("# B\n\n- 源: {rel}:3-3\n")).unwrap();
        // 页面必须引用到源码（过时检查触发实体表构建），源码晚于页面
        let now = std::time::SystemTime::now();
        let _ = std::fs::File::options()
            .write(true)
            .open(src_root.join("lib.rs"))
            .unwrap();
        let _ = filetime_set(&src_root.join("lib.rs"), now);
        let _ = filetime_set(
            &wiki.join("a.md"),
            now - std::time::Duration::from_secs(3600),
        );
        let _ = filetime_set(
            &wiki.join("b.md"),
            now - std::time::Duration::from_secs(3600),
        );
        // `./` 段形态：join(".") 在 Windows 与 Unix 均保留 CurDir 段
        let dot_roots = vec![src_root.join(".").join("lib.rs").parent().unwrap().to_path_buf()];
        let plain_roots = vec![src_root.clone()];
        for (tag, roots) in [("dot", dot_roots), ("plain", plain_roots)] {
            let issues = lint(&dir, &roots);
            assert!(
                !issues
                    .iter()
                    .any(|i| i.kind == "bad-citation-overlap" && i.path.ends_with("a.md")),
                "[{tag}] 引用实体区间内不应报 overlap, 实际: {:?}",
                issues
            );
            assert!(
                issues
                    .iter()
                    .any(|i| i.kind == "bad-citation-overlap" && i.path.ends_with("b.md")),
                "[{tag}] 引用实体间隙应报 overlap, 实际: {:?}",
                issues
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty(), "空目录应无问题, 实际: {:?}", issues);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言目录缺失时 lint 不 panic
    #[test]
    fn test_lint_no_wiki_dir() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_nodir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-4 引用存在性：产物中的 `path:line` 引用指向不存在的文件 → bad-citation
    #[test]
    fn test_lint_bad_citation_missing_file() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_cite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki"); // output_dir 的父目录 = 项目根
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_cite_ok_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_overlap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_cov_{}", std::process::id()));
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

    /// P3 已知噪声防回归：合成页（architecture.md 等）引用模块名（api.md 的
    /// `## ` 节标题，如 `src`、`src::storage`）不应报 entity-coverage——
    /// 模块名是容器而非叶子实体，不在 api_known_entities 清单中；
    /// 编造的实体名仍必须报（防幻觉语义不变）
    #[test]
    fn test_lint_entity_coverage_accepts_module_names() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_cov_mod_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // api.md：单段模块名 src 与多段模块名 src::storage 各一节 + 叶子实体
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## src\n\n- `Foo` — m.rs:1\n\n## src::storage\n\n- `SessionStore` — storage.rs:1\n",
        )
        .unwrap();
        // 合成页：按模块名引用（含多段名）+ 一个编造的实体名
        std::fs::write(
            wiki.join("architecture.md"),
            "# 架构\n\n## 模块\n\n- `src` — 核心模块\n- `src::storage` — 存储模块\n- `GhostEntity` — 编造的实体\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let cov: Vec<_> = issues.iter().filter(|i| i.kind == "entity-coverage").collect();
        assert_eq!(cov.len(), 1, "只应报编造实体, 实际: {:?}", issues);
        assert!(cov[0].message.contains("GhostEntity"), "应指向编造实体: {}", cov[0].message);
        assert!(
            !cov.iter().any(|i| i.message.contains("src")),
            "模块名引用不应误报: {:?}",
            cov
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_entity_names() {
        let content = "## 核心实体\n\n- `Server`（struct）— HTTP 服务\n- `fn connect()` — 连接\n- `foo_bar` — 下划线\n";
        let names = extract_entity_names(content, &std::collections::HashSet::new());
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
        let names = extract_entity_names(content, &std::collections::HashSet::new());
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

    /// 边界回归：切片类型签名 `&[i64]` 的右括号不应被误当属性宏段结尾。
    /// 原实现无条件 rfind(']')，会把 pub fn sum(values: &[i64]) -> i64 截成
    /// ") -> i64"，末标识符提取出原生类型 i64 → 误报 stale-entity（lint 为
    /// CI 门禁，任何含 &[T] 参数的仓库必误报）。属性宏前缀仍应正常剥离。
    #[test]
    fn test_entity_name_slice_type_signature_not_misstripped() {
        // 切片类型参数：rfind(']') 会命中 [i64] 的右括号，修复后应提取 sum
        assert_eq!(
            entity_name_from_signature("pub fn sum(values: &[i64]) -> i64"),
            Some("sum".into())
        );
        assert_eq!(
            entity_name_from_signature("pub fn concat(items: &[String]) -> String"),
            Some("concat".into())
        );
        // Rust 属性宏前缀仍正确剥离
        assert_eq!(
            entity_name_from_signature("#[tokio::main] async fn run() {}"),
            Some("run".into())
        );
        assert_eq!(
            entity_name_from_signature("#[test] fn helper() {}"),
            Some("helper".into())
        );
        // C# 属性宏（既有行为不回退）
        assert_eq!(
            entity_name_from_signature("[ContextMenu(\"x\")] public void DoThing()"),
            Some("DoThing".into())
        );
    }

    /// G2：产物中的 mermaid fence 语法错误 → bad-mermaid；合法图不报
    #[test]
    fn test_lint_bad_mermaid_detects_broken_diagram() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_mermaid_{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_stale_entity_{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_dotdot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
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

    /// v28 t06：vctx 只读校验——合法标记（文件存在/行区间有效/哈希正确）不报错。
    /// 哈希期望值硬编码为独立算法的已知输出（SHA-256("hello") 前 8 位 =
    /// 2cf24dba），防实现自身偏差（自洽计算无法发现"两侧同错"）。
    #[test]
    fn test_lint_vctx_valid_passes() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_vctx_ok_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("real.rs"), "hello\n").unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n核心逻辑见 [[vctx:src/real.rs#L-1-L-1@2cf24dba]]\n",
        )
        .unwrap();

        let issues = lint(&out, &[]);
        assert!(
            !issues.iter().any(|i| i.kind == "bad-vctx"),
            "合法 vctx 标记不应报错, 实际: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 t06：路径不存在 → bad-vctx；顺带覆盖格式不完整标记
    /// （`[[vctx:src/real.rs]]` 缺 # 行区间段）也报 bad-vctx（手写护栏，
    /// 写坏的标记必须可观测）
    #[test]
    fn test_lint_vctx_missing_file_and_malformed() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_vctx_miss_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n- [[vctx:src/ghost.rs#L-1-L-1@2cf24dba]]\n- [[vctx:src/real.rs]]\n",
        )
        .unwrap();

        let issues = lint(&out, &[]);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-vctx").collect();
        assert_eq!(bad.len(), 2, "缺失文件与格式不完整各报一条, 实际: {:?}", issues);
        assert!(
            bad.iter().any(|i| i.message.contains("ghost.rs")),
            "应指向缺失文件: {:?}",
            issues
        );
        assert!(
            bad.iter().any(|i| i.message.contains("格式不完整")),
            "应报格式不完整: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 t06：行区间越界（end > 文件总行数）→ bad-vctx
    #[test]
    fn test_lint_vctx_range_out_of_bounds() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_vctx_range_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("small.rs"), "line1\nline2\n").unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n见 [[vctx:src/small.rs#L-3-L-3@2cf24dba]]\n",
        )
        .unwrap();

        let issues = lint(&out, &[]);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-vctx").collect();
        assert_eq!(bad.len(), 1, "越界引用应报 bad-vctx, 实际: {:?}", issues);
        assert!(bad[0].message.contains("越界"), "消息应说明越界: {}", bad[0].message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 t06：文件内容变更但标记保留旧哈希 → bad-vctx（哈希防内容漂移：
    /// 行号对、内容错也报警，补 bad-citation 结构校验之外的内容维度）
    #[test]
    fn test_lint_vctx_hash_mismatch() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_lint_vctx_hash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // 先按原始内容计算标记哈希（模拟"标记写好后文件被改"）
        let old_hash = vctx_line_hash("line1\nline2\n", 1, 2);
        std::fs::write(dir.join("src").join("lib.rs"), "changed\nline2\n").unwrap();
        std::fs::write(
            wiki.join("m.md"),
            format!("# M\n\n见 [[vctx:src/lib.rs#L-1-L-2@{old_hash}]]\n"),
        )
        .unwrap();

        let issues = lint(&out, &[]);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-vctx").collect();
        assert_eq!(bad.len(), 1, "内容变更后旧哈希应报错, 实际: {:?}", issues);
        assert!(bad[0].message.contains("哈希不匹配"), "消息应说明哈希不一致: {}", bad[0].message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
