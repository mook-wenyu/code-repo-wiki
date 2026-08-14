//! Wiki 产物健康检查（lint）
//!
//! 对已生成的 wiki 产物目录做静态检查，供 `code-repo-wiki lint` 命令与 CI 使用。
//! 对齐 LLM Wiki 最佳实践（Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）：
//!
//! 1. **孤儿页**：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复）
//! 2. **断链**：页面内链接指向不存在的产物文件（复制 crossref 语义，但作用于磁盘产物）
//! 3. **过时**：页面生成时间戳早于其源文件修改时间（源码已变但文档未更新）
//!    3b. **source-missing**：产物引用的代码源文件不存在（已删除/未生成，KNOWN-06）
//! 4. **bad-citation**：正文 `path:line` 引用指向不存在的文件或行号越界（引用契约的静态复核）
//!    4a. **bad-citation-overlap**：文件存在且行号有效但引用区间不覆盖任何实体（行号对但内容错，v14 B 组）
//!
//! 4b. **bad-vctx**：正文 `[[vctx:path#L-a-L-b@hash8]]` 手工标记做 5 步哈希只读校验（vericontext 协议，人工文档护栏：t05 决议不引入生成契约，只识别并校验已有标记）
//!
//! 5. **entity-coverage**：页面声称的实体不在 api.md 权威清单（LLM 编造的第二道闸；api.md 的模块名（## 节标题）属已知名——合成页按模块名引用不是实体声称；v0.7.1 DEFECT-B 起加源码现实校验——真实目录段名/文件 stem/AST 解析实体同样放行，路径引用（`src/main.rs`）由声称侧剔除，不产生派生 token；R2 起声称提取限定实体节，跳过「依赖关系」/「使用方式」小节——依赖节的外部 crate 名/模块引用不是实体声称）
//!    5a. **entity-ownership**（A8 幻觉缓解，收紧要害）：entity-coverage 只管「名字是否存在于代码库」——编造名恰好撞上真实目录段名/文件 stem（如编造 `Authenticator` 恰有 authenticator.rs）会漏网。归属校验收紧：模块页声称的实体必须归属正确——api 权威实体归属模块 == 页面模块放行；归属其他模块的 api 实体须在页面内有真实 file:line 引用（bad-citation 级），无引用报 entity-ownership（error，R2 起「声称行自带引用」或「实体名过短」两类归属不可靠情况降为告警级）；源码 AST 实体须文件级归属正确——所属文件 ∈ 页面关联文件，模块页无「相关文件」节时退化按「实体文件 stem == 模块短名」判定（R2 修结构性死代码）；仅命中目录段名/文件 stem 的放行但降为告警级（保留 DEFECT-B 宽容）；合成页（无模块归属）只做存在性校验
//! 6. **bad-mermaid**：产物中的 mermaid fence 无法被 merman 解析（历史产物/人工编辑/增量遗留）
//! 7. **stale-entity**：api.md 权威清单的实体在当前源码中不存在（文档引用了已删除/重命名的符号）；A8 起做反向定位——扫描模块页声称实体，对每个 stale 实体报出「页面引用了已删除实体 X」（无人引用的仍挂在 api.md 兜底）
//! 8. **dependency-fabricated**：模块页「## 依赖关系」节的模块声称对照权威集校验（生成期 validate_dependencies 的磁盘级复用，防人工/增量篡改引入虚构依赖）；权威集 = export_snapshot 的模块依赖名 + 模块文件 imports 顶级 crate + std/core——声称的外部 crate 未被导入或声称的模块不在依赖列表即报错（权威集不完整时降为告警，避免误报阻断 CI）
//!
//! 检查对象是**磁盘上的产物文件**（真实用户看到的东西），而非内存中的文档对象。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::output::citation;

/// 问题严重级别（R2 结构化替代 message 内嵌"（告警）"标记，网络权威明确：
/// 严重级别是结构化数据，不应编码进展示文本）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 阻断级：lint/status 退出码非 0（CI 门禁语义）
    Error,
    /// 告警级：仅展示不阻断退出码
    Warning,
}

/// 单条 lint 问题
#[derive(Debug, Clone)]
pub struct LintIssue {
    /// 问题类别（字符串化清单，audit-out-06 固化；新增类别须同步更新此处与
    /// 模块头文档，含完整枚举）:
    /// orphan / broken / stale / source-missing / bad-citation / bad-citation-overlap /
    /// bad-vctx / entity-coverage / entity-ownership / bad-mermaid / stale-entity /
    /// dependency-fabricated
    pub kind: &'static str,
    /// 问题文件相对路径（相对 output_dir）
    pub path: String,
    /// 问题描述（纯展示，不再内嵌严重级别标记）
    pub message: String,
    /// 严重级别：Error 阻断 lint/status 退出码，Warning 仅展示不阻断
    pub severity: Severity,
}

impl LintIssue {
    /// 是否告警级问题（读取结构化 severity 字段，不再依赖 message 文本
    /// 匹配——message 是展示文本，级别判定不应耦合其措辞）。
    pub fn is_warning(&self) -> bool {
        self.severity == Severity::Warning
    }
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

    // A8 stale 指纹化：加载生成状态（{output_dir}/.state/generation_state.json）。
    // 状态不可读（无状态文件/非 git 无状态/损坏）→ None，check_stale 走 mtime
    // 退化路径（保留现状兜底）。项目根 = output_dir 的父目录，作为把产物
    // 源路径与指纹表键（相对项目根）对齐的基准。
    let state = crate::incremental::state::GenerationState::load(&output_dir.join(".state")).ok();
    let project_root = output_dir.parent().unwrap_or_else(|| Path::new("."));

    // 源码实体表：stale-entity（实体名集合）、bad-citation-overlap（行区间表）
    // 与 entity-ownership（实体→源文件归属表）共用一次扫描（三检查的输入
    // 同源，各自消费不同投影）
    let (
        source_entity_ranges,
        source_entity_names,
        source_path_names,
        entity_name_files,
        file_imports,
    ) = collect_source_entities(source_roots);

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
        issues.extend(check_stale(
            &pages,
            &output_dir.join("cards").join(lang),
            source_roots,
            lang,
            state.as_ref(),
            project_root,
        ));
        issues.extend(check_citations(
            &pages,
            output_dir,
            source_roots,
            lang,
            &source_entity_ranges,
        ));
        issues.extend(check_vctx_tokens(&pages, output_dir, source_roots, lang));
        // A8 归属校验收紧：合成页（api/overview/architecture/_toc 等无模块
        // 归属）走存在性检查（check_entity_coverage，现状）；模块页走归属
        // 校验（check_entity_ownership，新判定）。两检查都内置主语言守卫。
        let api_path = output_dir.join("wiki").join(lang).join("api.md");
        let (synthetic_pages, module_pages): (Vec<PathBuf>, Vec<PathBuf>) =
            pages.iter().cloned().partition(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| is_global_page(n.trim_end_matches(".md")))
                    .unwrap_or(false)
            });
        issues.extend(check_entity_coverage(
            &synthetic_pages,
            &api_path,
            lang,
            output_dir,
            &source_entity_names,
            &source_path_names,
        ));
        issues.extend(check_entity_ownership(
            &module_pages,
            &api_path,
            output_dir,
            lang,
            &source_entity_names,
            &source_path_names,
            &entity_name_files,
        ));
        issues.extend(check_mermaid(&pages, lang));
        issues.extend(check_stale_entities(
            &pages,
            &api_path,
            lang,
            output_dir,
            &source_entity_names,
        ));
        issues.extend(check_dependency_fabricated(
            &module_pages,
            output_dir,
            lang,
            source_roots,
            &file_imports,
        ));
    }

    issues
}

/// 全局/合成页判定（stem 命中受管文件名）：这些页面无模块归属，entity
/// 归属校验对它们只做存在性（复用 is_global，与 check_orphan_pages 的
/// 全局豁免同口径——api/overview/architecture/_toc/index/_log/architecture-map）。
fn is_global_page(stem: &str) -> bool {
    matches!(
        stem,
        "api" | "overview" | "architecture" | "architecture-map" | "_toc" | "index" | "_log"
    )
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
        // 全局文档(api/overview/architecture/_toc/index)由 TOC/概览引用,不算孤儿；
        // _log（note 命令追加的知识日志，P1-2）无任何入链但属受管文件,同样豁免
        let is_global = is_global_page(&stem);
        if !is_global && incoming.get(&stem).copied().unwrap_or(0) == 0 {
            issues.push(LintIssue {
                kind: "orphan",
                path: format!("wiki/{lang}/{file_name}"),
                message: format!("孤儿页: 无任何页面链接指向 {file_name}"),
                severity: Severity::Warning,
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
            // 边界（audit-out-05）：目标存在性只与**当前语言**页面集比对——
            // target 按 basename 匹配，跨语言链接（如 zh 页指向 wiki/en/foo.md）
            // 且本语言无同名 basename 时会误报断链。产物当前只生成主语言
            // （v30 后无扩展语言），属可接受的已知边界；跨语言目标存在性
            // 不在本检查范围，如需覆盖应改为跨语言页面全集比对。
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
                    severity: Severity::Error,
                });
            }
        }
    }
    issues
}

/// 3. 过时检查：模块页/卡片与其源文件的新鲜度对比
///    （从产物内容提取源文件路径——相关文件段，与源码根下对应文件对比；
///    相关文件段含 `..` 越界段时跳过该源路径，防 root 外 metadata 探测）
///
/// A8 指纹化：生成状态可用（state 非 None）且源文件在指纹表时，用内容
/// SHA256 对比替代 mtime——touch 不改内容不再误报 stale（CI checkout
/// 假 stale / touch 假 stale 的根因：mtime 变了但内容没变，文档不需要
/// 重生成）。判定矩阵（fp = 指纹表值，exists = 磁盘文件存在）：
///   (Some(fp), true) 且 fp≠当前 → stale（内容已变更）
///   (Some(fp), true) 且 fp==当前 → 通过（touch 不触发）
///   (_, false) → source-missing（保留现状）
/// 退化路径（保守，设计明确要求）：状态不可读/文件不在指纹表 → 回退
/// mtime 对比（保留现状逻辑兜底）——无指纹数据时宁可用旧信号也不静默
/// 放行或误报。
fn check_stale(
    pages: &[PathBuf],
    cards_dir: &Path,
    source_roots: &[PathBuf],
    lang: &str,
    state: Option<&crate::incremental::state::GenerationState>,
    project_root: &Path,
) -> Vec<LintIssue> {
    // 同时检查 wiki 页与 cards 卡片；逐项携带来源目录名（"wiki"/"cards"）,
    // 否则 path 恒标 wiki/ 会把卡片误标成 wiki 路径（真实卡片在 cards/{lang}/ 下）
    let mut stale_targets: Vec<(PathBuf, &'static str)> =
        pages.iter().map(|p| (p.clone(), "wiki")).collect();
    stale_targets.extend(
        collect_md_files(cards_dir)
            .into_iter()
            .map(|p| (p, "cards")),
    );

    // 指纹表键归一化（norm_sep 正斜杠）：from_insights 的键是相对项目根的
    // insight.path，Windows 上是反斜杠——与页面相关文件（正斜杠相对路径）
    // 归一后同基准才能命中。一次构建供全部源文件查询。
    let norm_fps: std::collections::HashMap<String, &str> = state
        .map(|s| {
            s.file_fingerprints
                .iter()
                .map(|(k, v)| (crate::incremental::norm_sep(k), v.as_str()))
                .collect()
        })
        .unwrap_or_default();

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
        let page_mtime = std::fs::metadata(page).and_then(|m| m.modified()).ok();
        let Some(page_time) = page_mtime else {
            continue;
        };
        for src in extract_source_files(&content) {
            // 统一路径逃逸过滤（与 check_citations/check_vctx_tokens 同一
            // detect_path_escape）：`..` 越界段 / Windows 根相对与盘符相对
            // 形态 / 绝对路径越出源码根——任一命中即跳过该源路径，不解析、
            // 不 metadata 探测 root 外文件（P1 不对称消除）。此处无对应
            // issue 类别，沿用本函数"读取失败跳过并 warn"的既有失败处理风格。
            if let Some(reason) = detect_path_escape(&src, source_roots) {
                tracing::warn!(
                    "lint stale 跳过源路径 `{src}` (page: {}): {reason}",
                    page.display()
                );
                continue;
            }
            let abs = resolve_source_path(source_roots, &src);

            // 指纹路径：状态可用且源文件在指纹表 → 内容对比
            if let Some(fp) = lookup_generation_fingerprint(&norm_fps, &src, &abs, project_root) {
                match std::fs::metadata(&abs) {
                    Ok(_) => {
                        match crate::incremental::state::GenerationState::compute_file_fingerprint(
                            &abs,
                        ) {
                            Ok(current) => {
                                // (Some(fp), true) 且 fp==当前 → 通过（touch 不改
                                // 内容的核心回归）；fp≠当前 → 内容已变更 → stale
                                if current != *fp {
                                    issues.push(LintIssue {
                                        kind: "stale",
                                        path: format!("{dir}/{lang}/{file_name}"),
                                        message: format!(
                                            "过时: 源文件 {src} 内容与生成时指纹不一致(源码已变更,文档可能未更新)"
                                        ),
                                        severity: Severity::Error,
                                    });
                                }
                            }
                            // 当前指纹读取失败（权限/IO 竞态）：无法确认内容，
                            // 保守回退 mtime（不因读取失败静默放行）
                            Err(_) => {
                                if let Some(issue) = stale_issue_by_mtime(
                                    &src, &abs, page_time, dir, lang, &file_name, page,
                                ) {
                                    issues.push(issue);
                                }
                            }
                        }
                    }
                    // (_, false) → source-missing（保留现状）：指纹表记录过该
                    // 文件但磁盘已删除，与 mtime 路径的缺失判定一致
                    Err(_) => {
                        if abs.as_os_str().is_empty() {
                            tracing::warn!(
                                "lint stale 无法解析源路径（无源码根基准）: `{src}` (page: {})",
                                page.display()
                            );
                            continue;
                        }
                        issues.push(LintIssue {
                            kind: "source-missing",
                            path: format!("{dir}/{lang}/{file_name}"),
                            message: format!(
                                "源文件缺失: 产物引用的源文件 `{src}` 不存在（{}）",
                                abs.display()
                            ),
                            severity: Severity::Error,
                        });
                    }
                }
                continue;
            }

            // mtime 退化路径（状态不可读 / 文件不在指纹表）：保留现状逻辑
            if let Some(issue) =
                stale_issue_by_mtime(&src, &abs, page_time, dir, lang, &file_name, page)
            {
                issues.push(issue);
            }
        }
    }
    issues
}

/// 在归一化的指纹表中查找源文件的生成时指纹（返回指纹值）。
///
/// 候选键按命中率排序：① 页面相关文件原文归一化（最常见——生成层写
/// `src/lib.rs` 相对项目根，与 from_insights 的键同基准）；② 解析出的
/// 绝对路径相对项目根归一化（页面写绝对路径时）③ 绝对路径归一化兜底。
/// 不在表内返回 None → 调用方走 mtime 退化路径（保守，不误报）。
fn lookup_generation_fingerprint<'a>(
    norm_fps: &'a std::collections::HashMap<String, &'a str>,
    src: &str,
    abs: &Path,
    project_root: &Path,
) -> Option<&'a str> {
    let src_norm = crate::incremental::norm_sep(src);
    if let Some(fp) = norm_fps.get(&src_norm) {
        return Some(*fp);
    }
    if let Ok(rel) = abs.strip_prefix(project_root) {
        let rel_norm = crate::incremental::norm_sep(&rel.to_string_lossy());
        if let Some(fp) = norm_fps.get(&rel_norm) {
            return Some(*fp);
        }
    }
    let abs_norm = crate::incremental::norm_sep(&abs.to_string_lossy());
    norm_fps.get(&abs_norm).copied()
}

/// mtime 对比兜底（退化路径）：源文件 mtime 晚于页面生成时间 → stale；
/// 源文件缺失 → source-missing；无法解析基准（空 abs）→ 跳过并 warn。
/// 与 A8 前 check_stale 的逻辑逐条一致（保留现状语义）。
fn stale_issue_by_mtime(
    src: &str,
    abs: &Path,
    page_time: std::time::SystemTime,
    dir: &'static str,
    lang: &str,
    file_name: &str,
    page: &Path,
) -> Option<LintIssue> {
    match std::fs::metadata(abs) {
        Ok(meta) => {
            if let Ok(src_time) = meta.modified()
                && src_time > page_time
            {
                Some(LintIssue {
                    kind: "stale",
                    path: format!("{dir}/{lang}/{file_name}"),
                    message: format!(
                        "过时: 源文件 {src} 的修改时间晚于页面生成时间(源码已变更,文档可能未更新)"
                    ),
                    severity: Severity::Error,
                })
            } else {
                None
            }
        }
        Err(_) => {
            // 空 abs（source_roots 为空/无可解析基准）不是"缺失"而是"无从
            // 解析"，跳过不报（KNOWN-07 语义，与既有行为一致）
            if abs.as_os_str().is_empty() {
                tracing::warn!(
                    "lint stale 无法解析源路径（无源码根基准）: `{src}` (page: {})",
                    page.display()
                );
                None
            } else {
                Some(LintIssue {
                    kind: "source-missing",
                    path: format!("{dir}/{lang}/{file_name}"),
                    message: format!(
                        "源文件缺失: 产物引用的源文件 `{src}` 不存在（{}）",
                        abs.display()
                    ),
                    severity: Severity::Error,
                })
            }
        }
    }
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
            // 统一路径逃逸过滤（与 check_stale/check_vctx_tokens 同一
            // detect_path_escape）：`..` 越界段 / Windows 根相对与盘符相对
            // 形态 / 绝对路径越出源码根——任一命中即按无效引用拒绝，不 stat
            // /读取 root 外文件（P1 不对称消除）。project_root.join(abs)=abs
            // 会把 root 外绝对路径直接送进 fs，必须在 join 前拦截。
            if let Some(reason) = detect_path_escape(&citation.path, source_roots) {
                issues.push(LintIssue {
                    kind: "bad-citation",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("{reason}: `{}`", citation.path),
                    severity: Severity::Error,
                });
                continue;
            }
            // 引用路径统一解析（resolve_output_relative_path）：相对项目根优先、
            // source_roots 兜底——与 check_vctx_tokens 共用同一解析器（P1 收敛，
            // 消除两处逐份复制的 project_root.join 直连）
            let abs = resolve_output_relative_path(output_dir, source_roots, &citation.path);
            let total_lines = std::fs::read_to_string(&abs)
                .map(|s| s.lines().count())
                .ok();
            let Some(n) = total_lines else {
                issues.push(LintIssue {
                    kind: "bad-citation",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("引用不存在: `{}` 指向的文件找不到", citation.path),
                    severity: Severity::Error,
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
                    severity: Severity::Error,
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
                    severity: Severity::Error,
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
    let end: usize = end_str.parse().map_err(|_| "结束行号非数字".to_string())?;
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
        // 跳过机器生成页（api/overview/architecture/_toc/index）：生成页正文
        // 可能内嵌 lint 自身文档注释里的 vctx 协议示例文本（render_api_reference
        // 渲染 doc 注释时把 `[[vctx:path#L-<start>-L-<end>@<hash8>]]` 原样带出），
        // naive 扫描 fail-closed 把示例当格式错 → 每次 regen 都是持久误报。
        // vctx 是人工/工具手写护栏，不覆盖生成页；_log 例外——note 命令追加的
        // 手写知识日志中 vctx 是真实标记，保留检查。
        let stem = page
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.trim_end_matches(".md"))
            .unwrap_or("");
        if stem != "_log" && is_global_page(stem) {
            continue;
        }
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
                        severity: Severity::Error,
                    });
                    continue;
                }
            };
            // 统一路径逃逸过滤（与 check_stale/check_citations 同一
            // detect_path_escape）：`..` 越界段 / Windows 根相对与盘符相对
            // 形态 / 绝对路径越出源码根——任一命中即按无效标记拒绝，不读取
            // root 外文件做哈希校验（project_root.join(abs)=abs 的绕过在
            // join 前拦截，P1 不对称消除）。
            if let Some(reason) = detect_path_escape(&token.path, source_roots) {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("vctx {reason}: `{}`", token.path),
                    severity: Severity::Error,
                });
                continue;
            }
            // 标记路径统一解析（resolve_output_relative_path）：相对项目根优先、
            // source_roots 兜底——与 check_citations 共用同一解析器（P1 收敛）
            let abs = resolve_output_relative_path(output_dir, source_roots, &token.path);
            // 严格 UTF-8 读取：失败 = 文件不存在或非 UTF-8（vericontext 同
            // fail-closed 语义：file_missing / invalid_utf8 均拒绝）
            let Ok(source) = std::fs::read_to_string(&abs) else {
                issues.push(LintIssue {
                    kind: "bad-vctx",
                    path: format!("wiki/{lang}/{file_name}"),
                    message: format!("vctx 目标不存在或非 UTF-8: `{}`", token.path),
                    severity: Severity::Error,
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
                    severity: Severity::Error,
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
                    severity: Severity::Error,
                });
            }
        }
    }
    issues
}

/// 相对路径绝对化（实体表键与 containment 判定的统一基准形态）。
///
/// 核实结论（KNOWN-05）：调用方确会喂相对路径（相对 `--root ../foo` 派生的
/// 源码根、root 下 walk 条目），且其语义即相对 cwd（CLI 参数帧内相对路径
/// 就是相对 cwd，`--root ../foo` 按 cwd 解析）——不是产物相对路径（产物
/// 相对路径由 resolve_source_path 按 root-first 解析，与 cwd 无关，P1 已修）。
/// 因此 cwd 基准不是"兜底"，是相对路径的正当解析基准；current_dir 失败
/// 时显式 panic——静默返回相对路径会把实体表键/containment 判定指向错误
/// 基准（KNOWN-05）。
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|e| panic!("获取当前工作目录失败（相对路径基准解析需要）: {e}"))
        .join(p)
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
        .filter_map(api_claim_entity_name)
        .collect()
}

/// 从 api.md 实体声称行提取实体名（`- \`...\`` 行 + entity_name_from_signature）。
/// 签名如 `pub fn authenticate(username: &str) -> Option<User>`：取第一个 '('
/// 前的最后标识符（跳过 pub/fn 等关键字前缀）。api_known_entities 与
/// api_entity_module_map 共用，保证"实体名"口径唯一。
fn api_claim_entity_name(line: &str) -> Option<String> {
    if !line.trim_start().starts_with("- `") {
        return None;
    }
    let inner = &line[line.find('`').unwrap() + 1..];
    inner.split('`').next().and_then(entity_name_from_signature)
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

/// 实体名 → 归属模块名 映射（api.md `## ` 节标题为模块归属，A8 归属校验用）。
///
/// 提取口径复用 api_claim_entity_name（与 api_known_entities 同函数），保证
/// 权威侧实体名唯一口径；`## ` 节标题即实体所属模块（api.md 由 render_api_
/// reference 按 ModuleCluster.name 分组渲染，节标题 = 模块名）。preamble
/// 实体（首个 `## ` 前，正常不会出现）归属为空串。
fn api_entity_module_map(api_content: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut current_module = String::new();
    for line in api_content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            current_module = rest.trim().to_string();
        } else if let Some(name) = api_claim_entity_name(line) {
            map.insert(name, current_module.clone());
        }
    }
    map
}

/// 实体名 → api.md 声称行的 file:line 引用（A8 归属校验的"bad-citation 级
/// 引用"证据：跨模块声称需页面内存在该实体的真实 file:line）。
///
/// 从 api.md 的 `- \`...\`` 行提取（render_api_reference 输出行尾带
/// ` — 文件:起始行` 定位），extract_citations 复用引用契约的提取器——
/// 与 lint 的 bad-citation 判定同一 parser，避免另造解析。
fn api_entity_citations(
    api_content: &str,
) -> std::collections::HashMap<String, Vec<crate::output::citation::Citation>> {
    let mut map: std::collections::HashMap<String, Vec<crate::output::citation::Citation>> =
        std::collections::HashMap::new();
    for line in api_content.lines() {
        if let Some(name) = api_claim_entity_name(line) {
            let cites = crate::output::citation::extract_citations(line);
            if !cites.is_empty() {
                map.entry(name).or_default().extend(cites);
            }
        }
    }
    map
}

/// 5. 实体覆盖率检查（P1-4 零成本评测）：模块页核心实体须存在于 api.md
///    （api.md 由 graph 权威渲染，页面声称的实体若不在 = LLM 编造实体名，
///    防幻觉第二道闸；api.md 仅主语言一份，只检查主语言目录）
///
/// 放行判定（两侧同口径，v0.7.1 DEFECT-B）：`known`（api.md 叶子实体）与
/// `modules`（api.md `##` 节标题 = 容器名）是权威清单侧；`source_entity_names`
/// 与 `source_path_names`（目录段名 + 文件 stem）是源码现实校验侧——真实存在于
/// 代码库的名字不是编造。四者任一命中即放行，全不满足才报 entity-coverage。
fn check_entity_coverage(
    pages: &[PathBuf],
    api_path: &Path,
    lang: &str,
    output_dir: &Path,
    source_entity_names: &std::collections::HashSet<String>,
    source_path_names: &std::collections::HashSet<String>,
) -> Vec<LintIssue> {
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
            // 声称命中权威侧（叶子实体/模块名）或源码现实侧（AST 解析实体/
            // 真实目录段名/文件 stem）即非编造；其余报错。
            //
            // 明确取舍（DEFECT-B）：编造名恰与真实目录/文件 stem 同名时不再
            // 报——与「模块名引用放行」同一哲学，entity-coverage 管「名字是否
            // 存在于代码库」而非「引用类别是否准确」；引用类别准确性归
            // source-missing/bad-citation 管辖。
            if known.contains(&entity)
                || modules.contains(&entity)
                || source_entity_names.contains(&entity)
                || source_path_names.contains(&entity)
            {
                continue;
            }
            issues.push(LintIssue {
                kind: "entity-coverage",
                path: format!("wiki/{lang}/{file_name}"),
                message: format!(
                    "实体覆盖率: 页面声称的实体 `{entity}` 不在 api.md 清单中（可能是编造或已删除）"
                ),
                severity: Severity::Error,
            });
        }
    }
    issues
}

/// 5a. 实体归属校验收紧（A8 幻觉缓解核心）：模块页声称的实体必须归属正确。
///
/// 调用方（lint）已把合成页（无模块归属）分流到 check_entity_coverage，
/// 本函数只处理模块页。判定矩阵（依次）：
///   1. e ∈ api 权威实体集合且归属模块 == 页面模块 → 放行（归属正确）
///   2. e ∈ api 权威实体集合且归属模块 != 页面模块 → 需 bad-citation 级引用
///      （api.md 中 e 的 file:line 存在于页面）；有引用 → 放行，无引用 → 报
///      entity-ownership（error）——拦截"跨模块声称无证据"的幻觉；R2 起
///      「声称行自带引用」或「实体名过短」两类归属不可靠情况降为告警级
///   3. e ∈ source_entity_names 且文件级归属正确 → 放行：所属文件 ∈ 页面
///      关联文件；模块页无「相关文件」节（wiki 页输出格式不含该节，只有
///      卡片有）时退化按「实体文件 stem == 模块短名」判定（R2 修结构性
///      死代码——修复前 related_files 恒空使规则 3 恒假，源码实体直接落
///      规则 5 误报 entity-coverage）
///   4. e 仅命中 source_path_names（目录段名/文件 stem）→ 放行但降为告警级
///      （保留 DEFECT-B 宽容，但不再完全静默）
///   5. 全不满足 → 保持 entity-coverage（error，防编造）
///
/// 页面模块 = 文件 stem（模块页文件名 = module_path.join("_") 的落盘命名）；
/// api 模块名以 `::` 连接目录段（ModuleCluster.name），归一 `::`→`_` 后与
/// 页面 stem 同基准比较（src::config ↔ src_config）。此命名一致性由生成层
/// 保证（页面 title = module_path.join("::")，api.md 节标题 = 模块名），
/// 归一后两侧同源。
///
/// 拦截效果：编造名与真实文件名同名（如编造 `Authenticator` 恰有
/// authenticator.rs）不在 api 权威集、不在源码 AST → 只命中 stem 落规则 4
/// 告警（不再完全静默）；若编造名恰是其他模块的 api 实体 → 规则 2 要求
/// 页面内有真实 file:line 引用，无引用被 entity-ownership 兜底。
fn check_entity_ownership(
    pages: &[PathBuf],
    api_path: &Path,
    output_dir: &Path,
    lang: &str,
    source_entity_names: &std::collections::HashSet<String>,
    source_path_names: &std::collections::HashSet<String>,
    entity_name_files: &std::collections::HashMap<String, Vec<std::path::PathBuf>>,
) -> Vec<LintIssue> {
    if primary_language(output_dir) != *lang {
        return Vec::new();
    }
    let Ok(api_content) = std::fs::read_to_string(api_path) else {
        return Vec::new();
    };
    let known = api_known_entities(&api_content);
    let modules = api_module_names(&api_content);
    let entity_module = api_entity_module_map(&api_content);
    let entity_citations = api_entity_citations(&api_content);

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
        let page_module = file_name.trim_end_matches(".md").to_string();
        let related_files = extract_source_files(&content);
        let page_citations = crate::output::citation::extract_citations(&content);
        // 模块页短名（模块路径最后一段）：模块页无「相关文件」节时，规则 3
        // 靠「实体文件 stem == 模块短名」判定文件级归属（见 source_entity_file_owned）
        let module_short_name = page_module_short_name(&modules, &page_module);

        for (entity, claim_line) in extract_entity_claims_with_lines(&content, &modules) {
            // 规则 1-2：api 权威实体（归属校验）
            match known_entity_issue(
                &entity,
                &known,
                &entity_module,
                &entity_citations,
                &page_citations,
                &claim_line,
                &page_module,
                lang,
                &file_name,
            ) {
                // 非 api 权威实体 → 继续规则 3
                Rule12Outcome::NotKnown => {}
                // 规则 1 归属正确 / 规则 2 有引用 → 放行，结束本实体处理
                Rule12Outcome::Pass => continue,
                Rule12Outcome::Issue(issue) => {
                    issues.push(issue);
                    continue;
                }
            }
            // 规则 3：源码 AST 实体且文件级归属正确 → 放行
            if source_entity_file_owned(
                &entity,
                source_entity_names,
                entity_name_files,
                &related_files,
                module_short_name.as_deref(),
            ) {
                continue;
            }
            // 规则 4：仅命中目录段名/文件 stem → 告警级放行（保留 DEFECT-B
            // 宽容但不再静默——目录/文件引用归 source-missing/bad-citation 管，
            // 这里只降级提示未确认归属）
            if let Some(issue) = stem_collision_issue(&entity, source_path_names, lang, &file_name)
            {
                issues.push(issue);
                continue;
            }
            // 规则 5：全不满足 → 保持 entity-coverage（防编造）
            issues.push(coverage_issue(&entity, lang, &file_name));
        }
    }
    issues
}

/// 规则 1-2 判定结果：三态区分「非 api 权威实体」（继续规则 3）与
/// 「api 权威实体已处理」（放行或上报）——两态混淆会让放行的已知实体
/// 错误落入规则 5 报 entity-coverage。
enum Rule12Outcome {
    /// 非 api 权威实体 → 继续规则 3/4/5
    NotKnown,
    /// 规则 1 归属正确 / 规则 2 有引用 → 放行，结束本实体的处理
    Pass,
    /// 规则 2 跨模块声称无引用 → 上报（含归属降级为告警）
    Issue(LintIssue),
}

/// 规则 1-2 判定：api 权威实体归属校验。
///
/// 参数多（9 个）是规则上下文所需（权威集/归属表/引用区间/页面位置），
/// 分组为结构体会让调用点与消息构造都失真，按项目惯例 allow。
#[allow(clippy::too_many_arguments)]
fn known_entity_issue(
    entity: &str,
    known: &std::collections::HashSet<String>,
    entity_module: &std::collections::HashMap<String, String>,
    entity_citations: &std::collections::HashMap<String, Vec<crate::output::citation::Citation>>,
    page_citations: &[crate::output::citation::Citation],
    claim_line: &str,
    page_module: &str,
    lang: &str,
    file_name: &str,
) -> Rule12Outcome {
    if !known.contains(entity) {
        return Rule12Outcome::NotKnown;
    }
    let owned_module = entity_module.get(entity).map(String::as_str).unwrap_or("");
    if owned_module.replace("::", "_") == page_module {
        // 规则 1：归属正确（实体属于页面自己的模块）
        return Rule12Outcome::Pass;
    }
    // 规则 2：跨模块声称需 bad-citation 级引用（api.md 中 e 的 file:line
    // 与页面引用区间重叠）；有 → 放行，无 → 报错
    let has_citation = entity_citations.get(entity).is_some_and(|api_cites| {
        page_citations.iter().any(|pc| {
            api_cites.iter().any(|ac| {
                crate::output::citation::citation_overlaps_entity(pc, &[(ac.start, ac.end)])
            })
        })
    });
    if has_citation {
        return Rule12Outcome::Pass;
    }
    // 附带处理（R2）：归属不可靠的两类情况降为告警（Warning）而非 error——
    // ① 声称行自带 file:line 引用（实体确有引用证据，只是引用文件/位置与
    //    api 权威不同，如 `path` 被 `## tests` 伞模块聚类归属到错误模块）；
    // ② 实体名过短（≤4 字符，如 `path`/`fs`，api 权威集污染高发形态）。
    // 降级语义：这些情况下"跨模块归属"是聚类伪影而非真幻觉，只提示不阻断。
    let downgraded =
        !citation::extract_citations(claim_line).is_empty() || entity.chars().count() <= 4;
    Rule12Outcome::Issue(LintIssue {
        kind: "entity-ownership",
        path: format!("wiki/{lang}/{file_name}"),
        message: format!(
            "实体归属: 页面声称的实体 `{entity}` 属于模块 `{owned_module}`（非本页面模块 `{page_module}`），且页面无该实体的 file:line 引用"
        ),
        severity: if downgraded {
            Severity::Warning
        } else {
            Severity::Error
        },
    })
}

/// 规则 3 判定：源码 AST 实体且文件级归属正确 → 放行。
///
/// 模块页无「相关文件」节（wiki 页输出格式不含该节，只有卡片有）时，
/// 退化用模块短名判定：实体文件 stem == 页面模块短名即认为文件级归属
/// 正确。为什么：related_files 恒空时规则 3 恒假，pub(crate) 等未进 api
/// 权威集的源码实体（如 is_cjk/extract_keywords）会直接落规则 5 误报
/// entity-coverage——模块页因此对源码实体无覆盖价值，结构性死代码。
fn source_entity_file_owned(
    entity: &str,
    source_entity_names: &std::collections::HashSet<String>,
    entity_name_files: &std::collections::HashMap<String, Vec<std::path::PathBuf>>,
    related_files: &[String],
    module_short_name: Option<&str>,
) -> bool {
    if !source_entity_names.contains(entity) {
        return false;
    }
    let Some(files) = entity_name_files.get(entity) else {
        return false;
    };
    if !related_files.is_empty() {
        // 既有路径：实体文件 ∈ 页面关联文件（文件级归属正确）
        return files
            .iter()
            .any(|f| related_files.iter().any(|r| file_matches_related(f, r)));
    }
    // 模块页退化路径：实体文件 stem == 页面模块短名
    let Some(short) = module_short_name else {
        return false;
    };
    files
        .iter()
        .any(|f| f.file_stem().is_some_and(|s| s.to_string_lossy() == short))
}

/// 规则 4 判定：实体仅命中目录段名/文件 stem → 告警级放行（保留 DEFECT-B
/// 宽容但不再静默——目录/文件引用归 source-missing/bad-citation 管，这里
/// 只降级提示未确认归属）。
fn stem_collision_issue(
    entity: &str,
    source_path_names: &std::collections::HashSet<String>,
    lang: &str,
    file_name: &str,
) -> Option<LintIssue> {
    if !source_path_names.contains(entity) {
        return None;
    }
    Some(LintIssue {
        kind: "entity-ownership",
        path: format!("wiki/{lang}/{file_name}"),
        message: format!(
            "实体归属: 声称的实体 `{entity}` 仅命中目录/文件 stem（非 api 权威实体），归属未确认"
        ),
        severity: Severity::Warning,
    })
}

/// 规则 5：全不满足 → entity-coverage（防编造）
fn coverage_issue(entity: &str, lang: &str, file_name: &str) -> LintIssue {
    LintIssue {
        kind: "entity-coverage",
        path: format!("wiki/{lang}/{file_name}"),
        message: format!(
            "实体覆盖率: 页面声称的实体 `{entity}` 不在 api.md 清单中（可能是编造或已删除）"
        ),
        severity: Severity::Error,
    }
}

/// 页面模块短名（模块路径最后一段）。
///
/// 从 api.md 模块名精确反查（`tests::tokenize` → `tokenize`，用
/// `::`→`_` 归一后与页面 stem 同基准匹配）；未命中（页面模块不在 api.md，
/// 正常不会出现）时回退从页面 stem 末段切取（`_` 分隔的末段）。
fn page_module_short_name(
    modules: &std::collections::HashSet<String>,
    page_module: &str,
) -> Option<String> {
    modules
        .iter()
        .find(|m| m.replace("::", "_") == page_module)
        .and_then(|m| m.rsplit("::").next())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            page_module
                .rsplit('_')
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
}

/// 源文件路径是否与页面关联文件引用匹配（A8 规则 3 的文件级归属判定）。
///
/// 匹配口径：归一化路径全等 / 实体文件绝对路径以「/关联路径」结尾（页面
/// 写相对项目根的路径、实体文件是绝对路径）/ 文件 basename 相等（兜底，
/// 容忍页面用缩写路径）。宽松 basename 匹配是刻意取舍：文件级归属是
/// "名字确实在页面自己的源码里"的宽松证据，宁松勿误报（规则 3 只放行
/// 不报错，收紧靠规则 2 的跨模块引用要求）。
fn file_matches_related(entity_file: &Path, related: &str) -> bool {
    let ef = crate::incremental::norm_sep(&entity_file.to_string_lossy());
    let r = crate::incremental::norm_sep(related);
    ef == r || (!r.is_empty() && ef.ends_with(&format!("/{r}"))) || {
        Path::new(related)
            .file_name()
            .is_some_and(|n| n == entity_file.file_name().unwrap_or_default())
    }
}

/// 扫描源码根并解析全部实体（stale-entity / bad-citation-overlap /
/// entity-ownership 共用一次扫描，避免 lint 对源码做多遍 AST 解析）
///
/// 返回 (norm_sep 绝对路径 → 实体行区间列表, 全部实体名集合, 目录段名+文件
/// stem 集合, 实体名 → 源文件路径列表, 文件 → imports 顶级 crate 集合)。
/// 第三项为 entity-coverage 的「源码现实
/// 校验」输入（DEFECT-B）：真实子目录名/文件 stem 是目录/文件引用而非叶子
/// 实体，parser 不会产出，但它们在代码库中真实存在——与 AST 解析出的实体名
/// 互补，共同构成「名字是否存在于源码」的判定面。第四项为 entity-ownership
/// 的「文件级归属」输入（A8）：实体声称须与页面关联文件对上才放行，需要
/// 实体名反查到其所属源文件（同一实体名可能出现在多个文件，用 Vec）。
/// 第五项为 dependency-fabricated 的「每文件 imports 顶级 crate」输入（v0.7.2
/// P0-3）：与实体同一次 parse 顺带收集，零额外解析成本——即使文件无 imports
/// 也插入空表，让调用方能区分「已解析」与「未解析」。
/// 目录段名/文件 stem 相对 source_root 收集，避免把临时目录/绝对路径段
/// （如 temp 目录名）算作合法名。
/// 解析失败的文件跳过（文件级损坏不是文档问题）；源码根不存在/为空时
/// 返回空表——调用方据此跳过对应检查（扫描失败 ≠ 文档过期/引用错误，
/// 两种错误信号不能混淆）。
// 五个返回投影的元组型已超 clippy type_complexity 阈值；每项都是独立的
// 检查输入（各自消费不同投影），拆结构体会让五处调用点都失真，按项目
// 惯例 allow（与 known_entity_issue 的 too_many_arguments 同理）。
#[allow(clippy::type_complexity)]
fn collect_source_entities(
    source_roots: &[PathBuf],
) -> (
    crate::output::citation::EntityRanges,
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
    std::collections::HashMap<String, Vec<std::path::PathBuf>>,
    std::collections::HashMap<String, Vec<String>>,
) {
    let mut ranges: crate::output::citation::EntityRanges = std::collections::HashMap::new();
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut path_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entity_files: std::collections::HashMap<String, Vec<std::path::PathBuf>> =
        std::collections::HashMap::new();
    let mut file_imports: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let registry = crate::ingest::parser::ParserRegistry::new();
    for root in source_roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walk_files(root) {
            collect_path_names(&entry, root, &mut path_names);
            let Some(processor) = registry.get_for_file(&entry) else {
                continue;
            };
            let Ok(source) = std::fs::read_to_string(&entry) else {
                continue;
            };
            if let Ok(insight) = processor.parse(&source, &entry) {
                let key = citation_key(&entry);
                ranges.insert(
                    key.clone(),
                    insight
                        .entities
                        .iter()
                        .map(|e| (e.line_start, e.line_end))
                        .collect(),
                );
                for entity in &insight.entities {
                    names.insert(entity.name.clone());
                    entity_files
                        .entry(entity.name.clone())
                        .or_default()
                        .push(entry.clone());
                }
                // 每文件 imports 顶级 crate（dependency-fabricated 权威集；
                // 排序去重保证确定性，无 imports 也插空表区分「已解析」）
                let mut crates: Vec<String> = insight
                    .imports
                    .iter()
                    .filter_map(|i| i.source.split("::").next())
                    .map(|s| s.to_string())
                    .collect();
                crates.sort();
                crates.dedup();
                file_imports.insert(key, crates);
            }
        }
    }
    (ranges, names, path_names, entity_files, file_imports)
}

/// 把 entry 相对 root 的路径分解为「目录段名 + 文件 stem」收集进 out
/// （DEFECT-B 源码现实校验用；一次遍历顺带完成，避免第二次 I/O）。
/// 只收集 Normal 组件，过滤 RootDir/Prefix/CurDir/ParentDir；文件段取 stem
/// （去扩展名）——声称侧路径引用（`src/main.rs`）已由 extract_entity_names
/// 先行剔除，这里放行的形态是裸目录段名（`core`）与裸文件 stem（`main`）。
fn collect_path_names(entry: &Path, root: &Path, out: &mut std::collections::HashSet<String>) {
    let Ok(rel) = entry.strip_prefix(root) else {
        return;
    };
    let mut components: Vec<std::path::Component> = rel.components().collect();
    if let Some(std::path::Component::Normal(file_seg)) = components.pop() {
        let name = file_seg.to_string_lossy();
        // 去扩展名取 stem；`.gitignore` 这类前导点文件 rsplit_once 前半为空，
        // 回退整名（把 `.gitignore` 算作合法名无害且更符合直觉）
        let stem = match name.rsplit_once('.') {
            Some((head, _)) if !head.is_empty() => head.to_string(),
            _ => name.to_string(),
        };
        out.insert(stem);
    }
    for comp in components {
        if let std::path::Component::Normal(seg) = comp {
            out.insert(seg.to_string_lossy().into_owned());
        }
    }
}

/// 7. 符号漂移检查（v13 D1，N1 + A8 反向定位）：api.md 权威清单中的实体
///    在当前源码中不存在 → "文档引用了已删除实体"（entity-coverage 的反向：
///    前者防 LLM 编造，本检查防文档过期——增量更新未覆盖、模块重构改名、
///    人工删改产物）。零 LLM，源码侧直接 AST 解析（与生成侧同一 parser，
///    口径一致）。
///
/// A8 反向定位：先求 stale 实体（api 已知 ∩ 不在 source_entity_names），
/// 再扫描全部模块页提取声称实体构建「实体 → [引用页面]」反向图——对每个
/// stale 实体，引用它的页面报 stale-entity（message 带"页面引用了已删除
/// 实体 X"，path 指向引用页面，定位到具体文档）；无任何页面引用的 stale
/// 实体仍在 api.md 上报一条（保留原信号，避免丢失孤儿 stale 实体）。
fn check_stale_entities(
    pages: &[PathBuf],
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

    // stale 实体集合（api 已知 ∩ 不在源码）
    let stale_set: std::collections::HashSet<&str> = known
        .iter()
        .filter(|e| !source_entity_names.contains(*e))
        .map(|e| e.as_str())
        .collect();
    if stale_set.is_empty() {
        return Vec::new();
    }
    let modules = api_module_names(&api_content);

    // 构建「实体 → [引用页面]」反向图：扫描全部**模块页**声称实体
    // （api.md 自身是权威清单不是引用页面，合成页/受管页一并跳过——
    // 否则 api.md 列的 stale 实体恒被自己"引用"，反向定位失去意义）
    let mut refs: std::collections::HashMap<&str, Vec<PathBuf>> = std::collections::HashMap::new();
    for page in pages {
        let file_name = page
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_global_page(file_name.trim_end_matches(".md")) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(page) else {
            continue;
        };
        for entity in extract_entity_names(&content, &modules) {
            if let Some(&stale) = stale_set.get(entity.as_str()) {
                refs.entry(stale).or_default().push(page.clone());
            }
        }
    }

    let mut issues = Vec::new();
    for stale in stale_set.into_iter().collect::<Vec<_>>() {
        match refs.remove(stale) {
            // 有页面引用：逐个引用页面报错，定位到具体文档
            Some(pages) => {
                let mut pages = pages;
                pages.sort();
                pages.dedup();
                for page in pages {
                    let file_name = page
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    issues.push(LintIssue {
                        kind: "stale-entity",
                        path: format!("wiki/{lang}/{file_name}"),
                        message: format!(
                            "符号漂移: 页面引用了已删除实体 `{stale}`（api.md 权威清单有但当前源码不存在，已删除或重命名）"
                        ),
                        severity: Severity::Error,
                    });
                }
            }
            // 无页面引用：在 api.md 上兜底报一条（保留原信号）
            None => {
                issues.push(LintIssue {
                    kind: "stale-entity",
                    path: format!("wiki/{lang}/api.md"),
                    message: format!(
                        "符号漂移: api.md 中的实体 `{stale}` 在当前源码中不存在（已删除或重命名，文档过期）"
                    ),
                    severity: Severity::Error,
                });
            }
        }
    }
    issues
}

/// 8. dependency-fabricated：模块页「## 依赖关系」节的模块声称对照权威集校验
///    （生成期 validate_dependencies 的磁盘级复用——产物页被人工/增量遗留
///    篡改时兜底，防虚构依赖）。
///
/// 权威集（允许集）= export_snapshot 的模块依赖名 ∪ 模块文件 imports 顶级
/// crate ∪ std/core。声称侧直接复用 dependency_check 的
/// extract_dependency_claims + validate_claims（fence 感知 + 诚实标记跳过，
/// 与生成期同口径）。页面→模块映射：module.name.replace("::","_") == 页面
/// stem（与 entity-ownership 同规则，生成层保证模块页文件名 =
/// module_path.join("_")）。
///
/// 严重级别：权威集完整（模块文件全部解析到 imports 来源）→ Error；权威集
/// 不完整（模块有文件但源码根缺失/未解析，外部 crate 判定会全部误报）→
/// 降级 Warning（探索 B 建议，避免高误报阻断 CI）。快照不可读（未 generate
/// 过）→ 无权威集，跳过检查。
fn check_dependency_fabricated(
    pages: &[PathBuf],
    output_dir: &Path,
    lang: &str,
    source_roots: &[PathBuf],
    file_imports: &HashMap<String, Vec<String>>,
) -> Vec<LintIssue> {
    if primary_language(output_dir) != *lang {
        return Vec::new();
    }
    // 快照不可读（未 generate 过 / 快照损坏）→ 无权威无从校验，跳过
    let Ok(snapshot_json) =
        std::fs::read_to_string(crate::output::export_snapshot_path(output_dir))
    else {
        return Vec::new();
    };
    let Ok(snapshot): Result<crate::output::ExportSnapshot, _> =
        serde_json::from_str(&snapshot_json)
    else {
        return Vec::new();
    };
    // 生成期解析缓存（insights_cache.json，键=相对项目根路径）优先；缺失
    // （全量 generate 无缓存）时回退到 lint 同次 parse 的 file_imports
    // （键=绝对路径 citation_key，经 source_roots 定位）
    let cache_imports = load_insights_cache_imports(output_dir);

    let mut issues = Vec::new();
    for module in &snapshot.modules {
        let page_stem = module.name.replace("::", "_");
        let Some(page) = pages.iter().find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|n| n.trim_end_matches(".md") == page_stem)
                .unwrap_or(false)
        }) else {
            continue;
        };
        // 模块 imports 顶级 crate 权威集 + 完整性判定：每个文件都解析到
        // imports 来源才算完整（files 为空时无 imports 可言，恒完整）
        let mut imports_allowed: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut unresolved_files = 0usize;
        for file in &module.files {
            match resolve_file_crate_imports(file, &cache_imports, file_imports, source_roots) {
                Some(crates) => imports_allowed.extend(crates),
                None => unresolved_files += 1,
            }
        }
        let severity = if unresolved_files == 0 {
            Severity::Error
        } else {
            Severity::Warning
        };
        let mut allowed: std::collections::BTreeSet<String> =
            module.dependencies.iter().cloned().collect();
        allowed.extend(imports_allowed);
        // std/core 前缀（Rust 标准库）硬编码进权威集（is_allowed 内建同一
        // 判定，显式写入让权威集自文档化）
        allowed.insert("std".to_string());
        allowed.insert("core".to_string());

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
        let claims = crate::output::dependency_check::extract_dependency_claims(&content);
        let violations = crate::output::dependency_check::validate_claims(
            claims.iter().map(|s| s.as_str()),
            &allowed,
        );
        for v in violations {
            let reason = match v.reason {
                crate::output::dependency_check::DependencyViolationReason::NotADependency => {
                    "该模块存在但不在本模块的依赖列表中"
                }
                crate::output::dependency_check::DependencyViolationReason::UnknownExternal => {
                    "该模块既不在项目模块中、也未被本模块导入，疑似编造"
                }
            };
            issues.push(LintIssue {
                kind: "dependency-fabricated",
                path: format!("wiki/{lang}/{file_name}"),
                message: format!("依赖声称: `{}` 不在权威集中（{reason}）", v.claimed),
                severity,
            });
        }
    }
    issues
}

/// 解析单文件（相对项目根的路径）的 imports 顶级 crate：优先 insights_cache
/// （生成期解析结果，键=相对路径直接命中）；缓存无此文件时回退 file_imports
/// （lint 同次 parse，键=绝对路径 citation_key，逐 source_root 定位）。
fn resolve_file_crate_imports(
    rel: &str,
    cache_imports: &HashMap<String, Vec<String>>,
    file_imports: &HashMap<String, Vec<String>>,
    source_roots: &[PathBuf],
) -> Option<Vec<String>> {
    if let Some(crates) = cache_imports.get(rel) {
        return Some(crates.clone());
    }
    for root in source_roots {
        let abs = root.join(rel);
        if let Some(crates) = file_imports.get(&citation_key(&abs)) {
            return Some(crates.clone());
        }
    }
    None
}

/// 读生成期解析缓存（insights_cache.json）为 文件→imports 顶级 crate 表。
/// 缓存是增量模式的辅助产物：缺失/损坏时返回空表（调用方回退到 lint 同次
/// parse 的 file_imports）。
fn load_insights_cache_imports(output_dir: &Path) -> HashMap<String, Vec<String>> {
    let path = output_dir.join(".state").join("insights_cache.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(cache): Result<Vec<crate::ingest::CachedInsight>, _> = serde_json::from_str(&content)
    else {
        return HashMap::new();
    };
    cache
        .into_iter()
        .map(|c| {
            let mut crates: Vec<String> = c
                .insight
                .imports
                .iter()
                .filter_map(|i| i.source.split("::").next())
                .map(|s| s.to_string())
                .collect();
            crates.sort();
            crates.dedup();
            (c.path, crates)
        })
        .collect()
}

/// 递归收集目录下全部文件（跟随子目录，忽略隐藏目录与符号链接循环——
/// 生产仓库正常布局下深度有限，不引入额外依赖）
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
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
                severity: Severity::Warning,
            });
        }
    }
    issues
}

/// 主语言目录名：api.md 只写主语言一份（render_all 规则），实体覆盖检查以它为权威
///
/// 边界（audit-out-09）：探测法（含 api.md 的目录即主语言）依赖 render_all
/// 只向主语言写 api.md 的约定；改读配置主语言需 lint() 获得 config——调用方
/// main.rs / bench/mod.rs 均持有 config，但签名变更跨出本文件域（output 之外，
/// 并行 worker 域），本批保留探测并排序候选目录保证确定性（多语言残留时
/// 取字典序首目录，不再依赖 read_dir 的无序返回）。
fn primary_language(output_dir: &Path) -> String {
    // 遍历 wiki/ 下的语言目录，取含 api.md 的那个（主语言）；无则返回空串（跳过检查）
    let wiki_root = output_dir.join("wiki");
    if let Ok(entries) = std::fs::read_dir(&wiki_root) {
        let mut candidates: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir() && e.path().join("api.md").is_file())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        candidates.sort();
        return candidates.into_iter().next().unwrap_or_default();
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
    // 1) 可见性与关键字修饰符前缀（pub/pub(crate)/pub(super)/pub(in path)/
    //    async/unsafe/extern "C"/const）必须在找 '(' 之前剥离：`pub(crate)
    //    fn is_cjk(...)` 的 `pub(crate)` 含 '('，按第一个 '(' 截取会把函数
    //    名取成 `pub`（crate 在括号内被丢弃）——api 权威集与 stale-entity
    //    两侧都取到污染名（R2 实测 `pub` 被当实体报 stale-entity）。
    let stripped = strip_modifier_prefix(after_attr);
    // 声明关键字分支（v0.7.2 P0-1）：impl 块 / type 别名 / 解构模式用通用
    // 「最后标识符」提取会得到污染名（`impl`/`From`/`EdgeIndex`），且三者的
    // 语义都不是「可命名叶子实体」——在找 '(' 之前提前分支处理。
    // impl 块（`impl<'a> X<'a>` / `impl From<..> for E`）：不是可命名叶子
    // 实体，返回 None。其涉及的 Type 名已由 struct/enum/trait 声明行单独
    // 进权威集（Rust parser 对 impl_item 记 kind="impl"，name=被 impl 的
    // 类型），这里不得把 `impl`/`From`/`Iterator` 当实体名。
    if stripped.starts_with("impl ") || stripped.starts_with("impl<") {
        return None;
    }
    // type 别名（`type EdgeId = EdgeIndex<u32>` / Go `type Foo struct`）：
    // 真名 = `type` 后紧跟的标识符（EdgeId/Foo）；等号右侧被别名类型
    // （EdgeIndex）或 struct 关键字是旧提取的污染名（regen 后新误报）。
    if let Some(rest) = stripped.strip_prefix("type ") {
        let ident = rest
            .split(['=', ';', '{', '<', '(', ',', ' ', '\t'])
            .next()
            .unwrap_or("")
            .trim();
        // 与通用分支同口径：过滤单字符/纯数字
        return (ident.len() > 1 && !ident.chars().all(|c| c.is_ascii_digit()))
            .then(|| ident.to_string());
    }
    // 解构模式（`{ stdout, stderr, exitCode }`）：无实体名，返回 None
    if stripped.starts_with('{') {
        return None;
    }
    let mut head = match stripped.find('(') {
        Some(open) => &stripped[..open],
        None => stripped,
    };
    // 2) 泛型约束子句（C# class Foo where T : class / Rust impl<T> Foo<T> where T: Clone）：
    //    其中的 ':' 会误导继承剥离，必须先切掉
    if let Some(w) = head.find("where") {
        head = &head[..w];
    }
    // 3) 继承/实现段（C# class Foo : Base, IBar / Java class Foo extends Bar 的 ':'）：
    //    基类名/接口名会污染最后标识符（实测 ScriptableObject/IDisposable 误报）
    if let Some(colon) = head.find(':') {
        head = &head[..colon];
    }
    // 4) 泛型参数列表（RegisterInstance<TService> / fn foo<T>）：'<' 后是类型参数名
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

/// 剥离签名开头的可见性与关键字修饰符前缀，返回剩余段。
///
/// 支持可叠加组合：`pub(crate) async unsafe extern "C" fn`。逐类剥：
/// - 带括号可见性：`pub(crate)`/`pub(super)`/`pub(self)`/`pub(in path)`
/// - 裸关键字：`pub`/`async`/`unsafe`/`const`；`extern` 后可能带 ABI
///   字符串（`extern "C" fn`）一并剥掉。
///
/// 为什么：修饰符前缀含 `(`（pub(crate)）或紧跟 ABI 串时，按第一个 `(`
/// 截取会把实体名取成 `pub` 等污染名（见 entity_name_from_signature）。
/// 前缀判定严格按「关键字 + 空白」：`public`/`async_thing` 等以关键字开头
/// 的普通标识符不是修饰符，不得剥离。
fn strip_modifier_prefix(sig: &str) -> &str {
    let mut s = sig.trim();
    loop {
        let prev = s;
        // 带括号可见性：pub(crate) / pub(super) / pub(self) / pub(in path)
        if let Some(rest) = s.strip_prefix("pub(")
            && let Some(end) = rest.find(')')
        {
            s = rest[end + 1..].trim();
        }
        // 裸关键字修饰符（可叠加）；extern 后可能带 "C" ABI 串
        for kw in ["pub", "async", "unsafe", "const", "extern"] {
            let Some(rest) = s.strip_prefix(kw) else {
                continue;
            };
            let after_kw = rest.trim_start();
            // 关键字后必须紧跟空白（或 ABI 引号）才是修饰符；`public` 的
            // 剩余段 `lic` 无前导空白 → 不是修饰符，跳过本关键字
            if after_kw.len() == rest.len() && !after_kw.starts_with('"') {
                continue;
            }
            if kw == "extern"
                && let Some(abi_rest) = after_kw.strip_prefix('"')
                && let Some(end) = abi_rest.find('"')
            {
                s = abi_rest[end + 1..].trim();
            } else {
                s = after_kw;
            }
            break;
        }
        if s == prev {
            break;
        }
    }
    s
}

/// 提取 `- \`...\`` 声称行的反引号内文（非声称行返回 None）。
/// extract_entity_names 借它做模块名原文精确匹配（多段名提取后会被截断），
/// 不能只依赖 entity_name_from_signature 的提取结果
fn claimed_backtick_inner(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("- `")
        .and_then(|rest| rest.find('`').map(|end| &rest[..end]))
}

/// 非实体声称节判定：`## 依赖关系`/`## 使用方式`（容忍 LLM 措辞变体，
/// 与 dependency_check 的依赖节判定口径一致）。
///
/// 为什么：依赖/使用小节里的 `- \`...\`` 是模块引用与使用说明，不是实体
/// 声称——`- \`crate::project::ProjectRoot\`` 会被截断成 `crate`、
/// `- \`anyhow\`` 外部 crate 名会被当实体、`- \`path\` 模块` 跨模块归属
/// 不可靠。整节跳过消除这四类误报（R2 实测 6 条 error 误报中 4 条源于
/// 依赖节，另 2 条源于规则 3 死代码）。
fn is_non_entity_section(heading: &str) -> bool {
    let lower = heading.trim_start_matches('#').trim().to_lowercase();
    lower.contains("依赖")
        || lower.contains("dependenc")
        || lower.contains("使用方式")
        || lower.contains("usage")
}

/// 提取页面声称的实体及声称行（`(name, 行文本)`）。
///
/// extract_entity_names 的带行变体：规则 2 归属降级（R2 附带处理）需要判断
/// 「声称行是否自带 file:line 引用」——实体名本身不携带引用位置，只有行
/// 文本能做这个判定。
fn extract_entity_claims_with_lines(
    content: &str,
    modules: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_non_entity_section = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            in_non_entity_section = is_non_entity_section(t);
            continue;
        }
        if in_non_entity_section {
            continue;
        }
        let Some(inner) = claimed_backtick_inner(line) else {
            continue;
        };
        if modules.contains(inner) {
            continue;
        }
        if inner.contains('/') || inner.contains('\\') || has_code_extension(inner) {
            continue;
        }
        if let Some(name) = entity_name_from_signature(inner) {
            out.push((name, line.to_string()));
        }
    }
    out
}

/// 从模块页内容提取声称的实体名：`- `Name`` 核心实体行（反引号内实体真名）。
/// `modules` 为 api.md 的模块名集合：原文精确命中模块名的声称行是模块引用
/// （容器名，如 `src`、`src::storage`）而非实体声称，先行剔除——多段名
/// `src::storage` 经 entity_name_from_signature（`::` 被当作继承段冒号）
/// 会截断为 `src`，必须按原文剔除（P3 误报修复）。
///
/// v0.7.1 DEFECT-B：路径形态声称（反引号内含 `/` 或 `\` 分隔符、或带代码
/// 扩展名，如 `src/main.rs`、`mod.rs`）是文件引用，归 source-missing/
/// bad-citation 管辖，不是实体声称——`entity_name_from_signature` 会把
/// `src/main.rs` 截成路径派生 token `rs` 造成误报，这里先行剔除（复用
/// has_code_extension，与 extract_source_files 的判定口径一致）。
///
/// R2：只扫「核心实体」等实体节，跳过「依赖关系」/「使用方式」小节（见
/// extract_entity_claims_with_lines 的节跟踪），依赖/使用节的名字是模块
/// 引用与使用说明而非实体声称。
fn extract_entity_names(content: &str, modules: &std::collections::HashSet<String>) -> Vec<String> {
    extract_entity_claims_with_lines(content, modules)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
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
///
/// KNOWN-06：只提取代码文件（SUPPORTED_EXTENSIONS 限定）——相关文件段应只
/// 列源码，非代码引用（README/配置/产物 .md/.json 等）不参与 stale 与
/// source-missing 检查，否则删除的文档/配置引用会被误报"源文件缺失"。
fn extract_source_files(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("- `") && line.ends_with('`') {
            let inner = &line[3..line.len() - 1];
            if has_code_extension(inner) {
                out.push(inner.to_string());
            }
        }
    }
    out
}

/// 代码扩展名判定（KNOWN-06）：对齐 ingest 扫描层 SUPPORTED_EXTENSIONS
/// （管线只收可解析语言，"源文件"同理只认代码文件）。
fn has_code_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| format!(".{ext}"))
        .is_some_and(|ext| crate::ingest::parser::SUPPORTED_EXTENSIONS.contains(&ext.as_str()))
}

/// Windows 根相对（`\foo`、`/foo`）与盘符相对（`C:foo`）形态检测——
/// `Path::is_absolute()` 对二者均返回 false（is_absolute = has_prefix &&
/// has_root），可绕过绝对路径 containment 校验；join 语义还会把路径引向
/// root 外（`C:\proj` join `\foo` = `C:\foo` 替换 prefix 后全部内容、
/// join `C:foo` 整体替换 self）。KNOWN-04 对抗实证：三调用方由此可
/// stat/读取 root 外文件。
///
/// Unix 不受影响：`\foo`/`C:foo` 是普通相对路径（反斜杠/冒号为普通字符，
/// root.join 不逃逸）；`/foo` 是绝对路径（is_absolute=true），已被既有
/// containment 分支拦截，不会走到根相对分支。
///
/// pub(crate)：生成层 citation.rs `check_citation_file_level` 复用同一组件级
/// 判定（KNOWN-04 全局收敛——两处逐份复制正是此前不对称的根因）。
pub(crate) fn is_root_relative_or_drive_relative(p: &Path) -> bool {
    let mut comps = p.components();
    let Some(first) = comps.next() else {
        return false;
    };
    // 根相对：仅根分隔符开头、无盘符前缀（`\foo`、`/foo`）
    if matches!(first, std::path::Component::RootDir) && !p.is_absolute() {
        return true;
    }
    // 盘符相对：有盘符前缀但无根（`C:foo`）
    if matches!(first, std::path::Component::Prefix(_)) && !p.has_root() {
        return true;
    }
    false
}

/// 统一路径逃逸判定（check_stale/check_citations/check_vctx_tokens 三调用方
/// 共用——判定逻辑逐处复制正是此前 P1 不对称的根因，DRY 同时保证三处行为
/// 一致）。返回第一个命中的拒绝原因；None 表示路径可安全解析。
/// 判定顺序与既有过滤一致：`..` 越界段 → Windows 根相对/盘符相对 →
/// 绝对路径越出源码根。
fn detect_path_escape(src: &str, source_roots: &[PathBuf]) -> Option<&'static str> {
    if src.split(['/', '\\']).any(|seg| seg == "..") {
        return Some("路径含越界段 ..");
    }
    let p = Path::new(src);
    if is_root_relative_or_drive_relative(p) {
        return Some("路径为根相对或盘符相对形态（无法验证 containment）");
    }
    if p.is_absolute() && !absolute_path_within_roots(p, source_roots) {
        return Some("绝对路径越出源码根");
    }
    None
}

/// 绝对路径 containment 校验：canonicalize 后必须落在任一源码根的
/// canonicalize 结果内才允许解析（阻止 root 外 stat/读取——元数据 oracle
/// 消除，与 `..` 越界段过滤同一语义）。
///
/// canonicalize 依赖路径存在：目标不存在/不可解析时不能判定为越界——root
/// 内不存在的绝对路径同样失败。此时改按词法 containment（absolute_path_
/// within_roots 的 `\\?\` 前缀比较被 canonicalize 破坏，词法侧用未加前缀的
/// 绝对化路径）区分"在 root 内但缺失"（放行，后续 fs 操作报缺失）与"root
/// 外"（拒绝，KNOWN-07：修复前 root 内不存在文件被误报"越出源码根"）。
/// root 可能是相对路径（--root ../foo）——canonicalize 自身按 cwd 解析
/// 相对路径，无需先 absolutize。Windows 上 canonicalize 统一加 `\\?\` 前缀，
/// 两侧同源前缀、starts_with 直接可比（无需 dunce）；Windows 路径比较
/// 大小写不敏感（std Path 语义）。
///
/// 性能说明：lint 主导成本是源码扫描（walk_files + AST 解析），引用/文件
/// 数量的 canonicalize 开销相对可忽略；跨调用缓存需共享全局态，与并行
/// 测试的多 root 冲突，故不缓存。
fn absolute_path_within_roots(p: &Path, source_roots: &[PathBuf]) -> bool {
    let canon_p = match p.canonicalize() {
        Ok(c) => c,
        Err(_) => return path_lexically_within_roots(p, source_roots),
    };
    source_roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|canon_root| canon_p.starts_with(canon_root))
    })
}

/// 词法 containment：路径绝对化后（不 canonicalize，容忍目标不存在）是否
/// 落在任一源码根内。p 恒为绝对路径（调用方只对 is_absolute 的路径调用
/// containment）；root 相对（--root ../foo）时按 cwd 基准绝对化——相对
/// --root 的语义即相对 cwd。`..` 越界段已被调用方统一过滤
/// （detect_path_escape），此处只做前缀比较；相对 root 含 `..` 时前缀比较
/// 可能失配（该组合下把"缺失"误判为"越界"，仅影响消息措辞，不产生
/// root 外探测——两个分支都不会解析该路径）。
fn path_lexically_within_roots(p: &Path, source_roots: &[PathBuf]) -> bool {
    let abs_p = absolutize(p);
    source_roots.iter().any(|root| {
        let root_abs = absolutize(root);
        abs_p.starts_with(&root_abs)
    })
}

/// 产物内引用/vctx 路径的统一解析入口（P1 收敛，audit-out-01/03）：
/// 相对项目根（output_dir 的父目录）优先，未命中回退 source_roots 逐根尝试。
///
/// 引用契约（citation.rs 模块头）：正文 `path:line` 的路径相对项目根，故优先
/// 按 project_root.join 解析；source_roots 与项目根分离（--root/--source 独立
/// 配置）时，产物引用的源文件可能只在某个源码根下，回退由 resolve_source_path
/// 按 root-first 逐根解析（含绝对路径 containment 校验，root 外不可达）。
/// check_citations 与 check_vctx_tokens 共用本函数，消除两处逐份复制的
/// project_root.join 直连（此前正是 P1 不对称的温床）。
///
/// 安全前提：调用方已在 detect_path_escape 拒绝 `..` 越界段、根相对/盘符相对
/// 形态与 root 外绝对路径，故 project_root.join(rel) 恒落在项目根内（不产生
/// root 外 exists()/读取探测）。
fn resolve_output_relative_path(output_dir: &Path, source_roots: &[PathBuf], rel: &str) -> PathBuf {
    let project_root = output_dir.parent().unwrap_or_else(|| Path::new("."));
    let primary_abs = project_root.join(rel);
    if primary_abs.exists() {
        primary_abs
    } else {
        resolve_source_path(source_roots, rel)
    }
}

/// 将产物中记录的源路径解析为绝对路径（相对源码根逐根尝试）
///
/// root-first 解析（对齐产物路径相对 root 的实践，见 src/ingest/mod.rs 的
/// strip_prefix 基准）：相对路径只按 `root.join(p)` 解析，不再有 cwd 相对
/// 分支——cwd 与 --root 分离时，cwd 下同名文件会把 stale/引用/vctx 检查
/// 指向错误文件（P1 修复）。绝对路径须通过 containment 校验（canonicalize
/// 后落在某个源码根内）才允许解析，root 外绝对路径返回空路径（不可达，
/// 使 metadata/读取必失败、不 stat root 外，与 `..` 越界段过滤同一语义）。
fn resolve_source_path(source_roots: &[PathBuf], src: &str) -> PathBuf {
    let p = Path::new(src);
    if p.is_absolute() {
        // 绝对路径 containment 校验：落在某个源码根内才放行（KNOWN-07：
        // root 内不存在的文件按"存在但缺失"放行，由后续 fs 操作报缺失；
        // root 外返回空路径，metadata 必失败，不 stat root 外文件——元数据
        // oracle 消除）。
        if absolute_path_within_roots(p, source_roots) {
            return p.to_path_buf();
        }
        return PathBuf::new();
    }
    for root in source_roots {
        let candidate = root.join(p);
        // 纵深防御（KNOWN-04）：join 结果必须仍落在 root 内才放行——Windows
        // 根相对（root.join(`\foo`) 替换 prefix）与盘符相对（root.join(`C:foo`)
        // 整体替换 self）即使绕过调用方过滤（detect_path_escape），此处也不
        // 会 exists() 探测 root 外路径。正常相对路径 join 后 starts_with 恒真，
        // 不影响既有解析。
        if candidate.starts_with(root) && candidate.exists() {
            return candidate;
        }
    }
    // 全部未命中：返回首个 root.join(p)（供 metadata 报错定位到 root 内；
    // 不再返回 cwd 相对路径，那会把 metadata/读取指向 cwd 同名对象）。
    // 兜底同样校验 starts_with(root)（KNOWN-04 纵深防御）：根相对/盘符相对
    // 形态即使走到兜底也不返回 root 外路径——逃逸时返回空路径，metadata 必
    // 失败。source_roots 为空时返回空路径，使 metadata 必失败，杜绝任何
    // cwd 探测。
    source_roots
        .first()
        .map(|root| {
            let candidate = root.join(p);
            if candidate.starts_with(root) {
                candidate
            } else {
                PathBuf::new()
            }
        })
        .unwrap_or_default()
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
        let _ = filetime_set(
            &wiki.join("b.md"),
            now - std::time::Duration::from_secs(3600),
        );
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
            issues
                .iter()
                .any(|i| i.kind == "orphan" && i.path.ends_with("a.md")),
            "a.md 无入链应为孤儿, 实际: {:?}",
            issues
        );
        // b.md 被 a 链接 → 不应是孤儿
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "orphan" && i.path.ends_with("b.md")),
            "b.md 有入链不应是孤儿, 实际: {:?}",
            issues
        );
        // 断链: a.md 指向 c.md 不存在
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "broken" && i.message.contains("c.md")),
            "a.md → c.md 应为断链, 实际: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-2 回归：append_note 写 wiki/{lang}/_log.md 后 lint 不得报 _log 孤儿
    ///（note 是追加式知识日志，无任何入链；修复前全局豁免表缺 _log 误报 orphan）
    #[test]
    fn test_lint_log_not_reported_as_orphan() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_log_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 模拟 note 命令产物：_log.md 无任何入链
        std::fs::write(
            wiki.join("_log.md"),
            "# 知识日志\n\n- 2026-08-13: 记录一条决策\n",
        )
        .unwrap();
        // 一个普通页面（无入链，仍应报孤儿——豁免只针对 _log，不扩大化）
        std::fs::write(wiki.join("m.md"), "# M\n\n内容\n").unwrap();

        let issues = lint(&dir, &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "orphan" && i.path.ends_with("_log.md")),
            "_log.md 不得报孤儿, 实际: {:?}",
            issues
        );
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "orphan" && i.path.ends_with("m.md")),
            "普通无入链页面仍应报孤儿, 实际: {:?}",
            issues
        );
    }

    /// v0.9 W2 回归：architecture-map.md 是确定性合成产物（无任何入链），
    /// lint 不得报孤儿——修复前全局豁免表缺 architecture-map 误报 orphan
    ///（AGENTS.md 注入块引用的是仓库根文件，wiki 页之间无链接指向它）
    #[test]
    fn test_lint_architecture_map_not_reported_as_orphan() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_arch_map_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 确定性合成产物：无任何入链（模块名/依赖均为裸文本，无链接）
        std::fs::write(
            wiki.join("architecture-map.md"),
            "# 架构地图\n\n## 模块总览\n\n- src — 无描述\n\n## 模块依赖\n\n- src → 依赖: 无；被依赖: 无\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "orphan" && i.path.ends_with("architecture-map.md")),
            "architecture-map.md 不得报孤儿, 实际: {:?}",
            issues
        );
    }

    /// 过时检查:产物引用源文件且源文件 mtime 更新 → 报 stale。
    /// 独立构造 fixture(不共享 make_fixture,避免并行测试竞态):
    /// 页面引用源文件绝对路径,先写页面再写源文件(源严格更新),
    /// 重写源文件刷新 mtime 后 lint 应报 stale。
    #[test]
    fn test_lint_stale_detects_newer_source() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_stale_{}", std::process::id()));
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

    /// P1 root-first：cwd（cargo test 默认 = 本仓库根）存在与产物路径同名的
    /// 相对文件（cwd/src/lib.rs 恰为本仓库真实文件）时，resolve_source_path
    /// 必须取 root 下的文件而非 cwd 文件——修复前 cwd 相对分支会先命中。
    #[test]
    fn test_resolve_source_path_root_first_over_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_resolve_rootfirst_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "ROOT VERSION\n").unwrap();
        // cwd = 本仓库根，存在同名 src/lib.rs（同名碰撞）
        let resolved = resolve_source_path(std::slice::from_ref(&dir), "src/lib.rs");
        assert_eq!(resolved, dir.join("src/lib.rs"));
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "ROOT VERSION\n",
            "应读取到 root 下文件的版本, 而非 cwd 同名文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 绝对路径 containment：root 内绝对路径原样返回（回归保护）；
    /// root 外绝对路径与空 source_roots 均视为不可达（返回空路径，
    /// metadata 必失败，不 stat root 外）
    #[test]
    fn test_resolve_source_path_absolute() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_resolve_abs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("abs.rs");
        std::fs::write(&abs, "x\n").unwrap();
        // root 内绝对路径：containment 通过，原样返回
        let resolved = resolve_source_path(std::slice::from_ref(&dir), &abs.to_string_lossy());
        assert_eq!(resolved, abs);
        // root 外绝对路径（dir 的兄弟文件，真实存在）→ 不可达
        let outside = std::env::temp_dir().join(format!(
            "code_repo_wiki_resolve_abs_out_{}",
            std::process::id()
        ));
        std::fs::write(&outside, "x\n").unwrap();
        let blocked = resolve_source_path(std::slice::from_ref(&dir), &outside.to_string_lossy());
        assert_ne!(blocked, outside, "root 外绝对路径不得原样返回");
        assert!(
            std::fs::metadata(&blocked).is_err(),
            "root 外绝对路径必须不可达（metadata 失败）"
        );
        // 空 source_roots：任何绝对路径都不放行（无 containment 基准）
        let empty = resolve_source_path(&[], &abs.to_string_lossy());
        assert_eq!(empty, PathBuf::new(), "无源码根时绝对路径也不得放行");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 全未命中兜底：非空 roots 返回首个 root.join(p)（定位到 root 内，
    /// 供 metadata 报错）；source_roots 为空返回空路径，metadata 必失败，
    /// 杜绝任何 cwd 探测。
    #[test]
    fn test_resolve_source_path_miss_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_resolve_miss_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("proj");
        let missed = resolve_source_path(std::slice::from_ref(&root), "src/ghost.rs");
        assert_eq!(missed, root.join("src/ghost.rs"));
        assert!(!missed.exists(), "未命中的兜底路径不应存在");
        let empty = resolve_source_path(&[], "src/ghost.rs");
        assert_eq!(empty, PathBuf::new());
        assert!(
            std::fs::metadata(&empty).is_err(),
            "空路径 metadata 必须失败"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KNOWN-04 组件级判定：Windows 根相对（`\foo`、`/foo`）与盘符相对
    /// （`C:foo`）形态必须识别为不可验证 containment（is_absolute 对二者
    /// 返回 false，join 可逃逸 root）。正常相对/盘符绝对不受影响。
    #[test]
    fn test_is_root_relative_or_drive_relative_forms() {
        #[cfg(windows)]
        {
            assert!(
                is_root_relative_or_drive_relative(Path::new(r"\foo")),
                "根相对 \\foo"
            );
            assert!(
                is_root_relative_or_drive_relative(Path::new(r"/foo")),
                "根相对 /foo"
            );
            assert!(
                is_root_relative_or_drive_relative(Path::new("C:foo")),
                "盘符相对 C:foo"
            );
            assert!(
                !is_root_relative_or_drive_relative(Path::new("src/foo.rs")),
                "正常相对"
            );
            assert!(
                !is_root_relative_or_drive_relative(Path::new(r"C:\foo")),
                "盘符绝对走 containment"
            );
            assert!(
                !is_root_relative_or_drive_relative(Path::new(r"C:/foo")),
                "盘符绝对走 containment"
            );
        }
        #[cfg(not(windows))]
        {
            // Unix 上 `\foo`/`C:foo` 是普通相对路径（反斜杠/冒号为普通字符）
            assert!(!is_root_relative_or_drive_relative(Path::new(r"\foo")));
            assert!(!is_root_relative_or_drive_relative(Path::new("C:foo")));
            // /foo 是绝对路径（is_absolute=true），不落入根相对分支
            assert!(!is_root_relative_or_drive_relative(Path::new("/foo")));
        }
    }

    /// KNOWN-04：detect_path_escape 必须拒绝根相对/盘符相对形态（Windows），
    /// 正常相对路径与含 `..` 越界段路径行为不变。
    #[test]
    fn test_detect_path_escape_rejects_root_relative_and_drive_relative() {
        #[cfg(windows)]
        {
            assert!(
                detect_path_escape(r"\foo.rs", &[]).is_some(),
                "\\foo.rs 应拒绝"
            );
            assert!(
                detect_path_escape(r"/foo.rs", &[]).is_some(),
                "/foo.rs 应拒绝"
            );
            assert!(
                detect_path_escape("C:foo.rs", &[]).is_some(),
                "C:foo.rs 应拒绝"
            );
        }
        assert!(
            detect_path_escape("../foo.rs", &[]).is_some(),
            ".. 越界段应拒绝"
        );
        assert!(
            detect_path_escape("src/foo.rs", &[]).is_none(),
            "正常相对路径不应拒绝"
        );
    }

    /// KNOWN-04 纵深防御：resolve_source_path 对根相对/盘符相对形态即使绕过
    /// 调用方过滤也不得解析到 root 外（修复前 root.join 会把路径引向 root 外
    /// 并返回该逃逸路径，metadata 探测 root 外文件）。修复后返回空路径
    /// （不可达）；常规相对路径不受影响。
    #[test]
    #[cfg(windows)]
    fn test_resolve_source_path_never_escapes_root() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_resolve_escape_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 常规相对路径正常解析到 root 内
        let normal = resolve_source_path(std::slice::from_ref(&dir), "src/ghost.rs");
        assert!(
            normal.starts_with(&dir),
            "常规相对路径必须解析到 root 内: {:?}",
            normal
        );
        // 根相对/盘符相对形态：必须返回空路径（不可达，不返回 root 外路径）
        for bad in [r"\foo.rs", r"/foo.rs", "C:foo.rs"] {
            let resolved = resolve_source_path(std::slice::from_ref(&dir), bad);
            assert!(
                resolved.as_os_str().is_empty() && std::fs::metadata(&resolved).is_err(),
                "路径 {bad} 必须不可达（修复前会返回 root 外路径 {:?}，metadata 探测 root 外）",
                resolved
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KNOWN-04 集成（Windows）：相关文件段含根相对/盘符相对形态时不得报
    /// stale 或 source-missing（join 会逃逸 root，必须在上游拒绝）。
    #[test]
    #[cfg(windows)]
    fn test_lint_stale_rejects_root_relative_and_drive_relative_paths() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_rootrel_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n## 相关文件\n\n- `\\foo.rs`\n- `/foo.rs`\n- `C:foo.rs`\n",
        )
        .unwrap();
        // root 内真实代码文件（collect_source_entities 正常工作的前提）
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("real.rs"), "pub fn real() {}\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "stale" || i.kind == "source-missing"),
            "根相对/盘符相对相关文件不得解析（不报 stale/source-missing）, 实际: {:?}",
            issues
        );
    }

    /// KNOWN-06：产物引用的代码源文件缺失 → source-missing（修复前静默跳过，
    /// 产物引用已删除/未生成的源文件无任何告警）。
    #[test]
    fn test_lint_stale_reports_missing_source() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_missing_src_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n## 相关文件\n\n- `src/ghost.rs`\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "source-missing" && i.message.contains("src/ghost.rs")),
            "缺失的代码源文件应报 source-missing, 实际: {:?}",
            issues
        );
    }

    /// KNOWN-06 不误伤：非代码引用（README.md 等）缺失不得报 source-missing
    /// （相关文件段只对代码扩展名做 stale/source-missing 检查）。
    #[test]
    fn test_lint_stale_ignores_non_code_missing_reference() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_noncode_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n## 相关文件\n\n- `docs/README.md`\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("real.rs"), "pub fn real() {}\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "source-missing"),
            "非代码引用不得报 source-missing, 实际: {:?}",
            issues
        );
    }

    /// P1 集成：cwd ≠ root 且 cwd 存在与产物路径同名文件时，stale 检查必须
    /// 命中 root 下的文件。fixture 的 root=dir 含 src/lib.rs；cwd（本仓库根）
    /// 也含 src/lib.rs（mtime 为历史提交时间，早于本测试新写页面）。先写页面
    /// 再写 root 源文件（root 源文件严格更新）→ 应报 stale；若走 cwd 兜底会
    /// 命中仓库自己的 src/lib.rs（更旧）→ 不报 stale，测试即可检出旧缺陷。
    #[test]
    fn test_lint_stale_resolves_root_not_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_rootfirst_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 先写页面（mtime 早）
        std::fs::write(
            wiki.join("lib.md"),
            "# Lib\n\n## 相关文件\n\n- `src/lib.rs`\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // 再写 root（= dir）下的 src/lib.rs（mtime 严格更新）
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "stale" && i.path.ends_with("lib.md")),
            "应命中 root 的 src/lib.rs 并报 stale, 实际: {:?}",
            issues
        );
    }

    /// P1 越界段防护：相关文件段含 `..` 时跳过（与 check_citations/check_vctx
    /// 对齐）——目标文件真实存在于 root 外（root 相对 `..` 可解析到）且更新
    /// 也绝不报 stale，消除 root 外 metadata 探测不对称。修复前 cwd 相对分支
    /// 兜底会命中该 root 外文件并误报 stale，本测试可检出。
    #[test]
    fn test_lint_stale_rejects_dotdot_source_path() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_dotdot_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // root 外（dir 的父级 = temp_dir）真实存在该文件（越界但存在）
        let escape_dir = dir
            .parent()
            .unwrap()
            .join(format!("escape_dir_stale_{}", std::process::id()));
        std::fs::create_dir_all(&escape_dir).unwrap();
        let escape_path_rel = format!("../escape_dir_stale_{}/x.rs", std::process::id());
        std::fs::write(
            wiki.join("m.md"),
            format!("# M\n\n## 相关文件\n\n- `{escape_path_rel}`\n"),
        )
        .unwrap();
        // escape 文件写晚于页面（更新，若被解析会报 stale）——必须被 `..` 过滤拦截
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(escape_dir.join("x.rs"), "line1\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&escape_dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "stale"),
            "含 .. 的相关文件路径应被跳过, 不得报 stale(也不得探测 root 外), 实际: {:?}",
            issues
        );
    }

    /// P1 绝对路径越界防护（本轮新增）：相关文件段含 root 外绝对路径
    /// （真实存在、mtime 更新）时不得报 stale——canonicalize 后 containment
    /// 校验拒绝，不 stat root 外文件（与 `..` 越界段同一语义）。修复前
    /// resolve_source_path 对绝对路径原样放行，metadata 命中 root 外文件
    /// 会误报 stale，本测试可检出。
    #[test]
    fn test_lint_stale_rejects_out_of_root_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_absout_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // root（= dir）外真实存在该文件（绝对路径越界但存在）
        let outside =
            std::env::temp_dir().join(format!("outside_abs_stale_{}.rs", std::process::id()));
        std::fs::write(&outside, "line1\n").unwrap();
        let abs = outside.to_string_lossy().to_string();
        // 页面相关文件段写 root 外绝对路径
        std::fs::write(
            wiki.join("m.md"),
            format!("# M\n\n## 相关文件\n\n- `{abs}`\n"),
        )
        .unwrap();
        // outside 文件写晚于页面（更新，若被解析会报 stale）——必须被 containment 拦截
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&outside, "line1\nupdated\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "stale"),
            "root 外绝对路径应被跳过, 不得报 stale(也不得探测 root 外), 实际: {:?}",
            issues
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
        // src/lib.rs，project_root.join 会落在 temp_dir 下而非真实源码位置，
        // resolve_source_path 又已去除 cwd 兜底（root-first），文件将不可达、
        // 被误报 bad-citation，实体表键恒不命中
        let rel = format!("{}/src/lib.rs", dir.file_name().unwrap().to_string_lossy());
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
        let dot_roots = vec![
            src_root
                .join(".")
                .join("lib.rs")
                .parent()
                .unwrap()
                .to_path_buf(),
        ];
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
        assert_eq!(
            files,
            vec!["src/lib.rs".to_string(), "tests/a.rs".to_string()]
        );
        // 链接行不应误提取
        assert!(extract_source_files("- [x](wiki/zh/a.md)").is_empty());
    }

    #[test]
    fn test_lint_empty_dir() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty(), "空目录应无问题, 实际: {:?}", issues);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言目录缺失时 lint 不 panic
    #[test]
    fn test_lint_no_wiki_dir() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_nodir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let issues = lint(&dir, &[]);
        assert!(issues.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-4 引用存在性：产物中的 `path:line` 引用指向不存在的文件 → bad-citation
    #[test]
    fn test_lint_bad_citation_missing_file() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_cite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki"); // output_dir 的父目录 = 项目根
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 页面引用不存在的文件
        std::fs::write(wiki.join("m.md"), "# M\n\n核心逻辑见 `src/ghost.rs:10`\n").unwrap();
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
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_cite_ok_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("real.rs"), "line1\nline2\n").unwrap();
        // 页面引用真实文件（相对项目根路径,output_dir 父目录解析命中）
        std::fs::write(wiki.join("m.md"), "# M\n\n核心逻辑见 `src/real.rs:1`\n").unwrap();

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
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_overlap_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        let src_root = dir.join("src");
        std::fs::create_dir_all(&src_root).unwrap();
        // 10 行源码：实体区间 (2,2)（fn server 定义在第 2 行）
        let source =
            "line1\npub fn server() {}\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
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
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_cov_{}", std::process::id()));
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
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(cov.len(), 1, "只应报编造实体, 实际: {:?}", issues);
        assert!(
            cov[0].message.contains("FakeEntity"),
            "应指向 FakeEntity: {}",
            cov[0].message
        );
        assert!(!cov[0].message.contains("Foo"), "真实实体不应误报");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3 已知噪声防回归：合成页（architecture.md 等）引用模块名（api.md 的
    /// `## ` 节标题，如 `src`、`src::storage`）不应报 entity-coverage——
    /// 模块名是容器而非叶子实体，不在 api_known_entities 清单中；
    /// 编造的实体名仍必须报（防幻觉语义不变）
    #[test]
    fn test_lint_entity_coverage_accepts_module_names() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_cov_mod_{}",
            std::process::id()
        ));
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
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(cov.len(), 1, "只应报编造实体, 实际: {:?}", issues);
        assert!(
            cov[0].message.contains("GhostEntity"),
            "应指向编造实体: {}",
            cov[0].message
        );
        assert!(
            !cov.iter().any(|i| i.message.contains("src")),
            "模块名引用不应误报: {:?}",
            cov
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// DEFECT-B 防回归：真实子目录名（`core`/`net`/`service`）与路径引用
    /// （`src/main.rs`）不应误报 entity-coverage——子目录名/文件 stem 经源码
    /// 现实校验放行（目录/文件引用而非叶子实体），路径引用由声称侧剔除
    /// （不派生 `rs` token）。追加真编造名（GhostFactory）仍必须报（防幻觉
    /// 语义不变）。
    #[test]
    fn test_lint_entity_coverage_accepts_real_subdir_and_path_refs() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_cov_path_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 源码树：src/{core,net,service}/mod.rs + src/main.rs（真实目录名/文件 stem）
        let src = dir.join("src");
        for (sub, func) in [("core", "Foo"), ("net", "Bar"), ("service", "Baz")] {
            std::fs::create_dir_all(src.join(sub)).unwrap();
            std::fs::write(
                src.join(sub).join("mod.rs"),
                format!("pub fn {func}() {{}}\n"),
            )
            .unwrap();
        }
        std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        // api.md：模块名 src / src::lib + 叶子实体 Foo/Bar（权威清单）
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## src\n\n- `Foo`\n\n## src::lib\n\n- `Bar`\n",
        )
        .unwrap();
        // 页面：裸子目录名引用 + 相关文件段路径引用
        std::fs::write(
            wiki.join("src.md"),
            "# 模块 src\n\n## 模块\n\n- `core` — 核心模块\n- `net` — 网络模块\n- `service` — 服务模块\n\n## 相关文件\n\n- `src/main.rs`\n",
        )
        .unwrap();

        // 双 source_roots：仓库根 + src 子目录，覆盖现实校验的两种挂载点
        let issues = lint(&dir, &[dir.clone(), src.clone()]);
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(
            cov.len(),
            0,
            "真实子目录名与路径引用不应误报, 实际: {:?}",
            issues
        );
        // A8 新 kind 分支：子目录名仅命中 stem → 降为告警级 entity-ownership
        // （保留 DEFECT-B 宽容但不再完全静默），不再是全放行
        let warn: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-ownership" && i.severity == Severity::Warning)
            .collect();
        assert_eq!(
            warn.len(),
            3,
            "core/net/service 三个 stem 命中应各报一条告警, 实际: {:?}",
            issues
        );
        assert!(
            warn.iter().any(|i| i.message.contains("core"))
                && warn.iter().any(|i| i.message.contains("net"))
                && warn.iter().any(|i| i.message.contains("service")),
            "告警应覆盖三个子目录名: {:?}",
            warn
        );
        // 路径扩展名防回归：`## 相关文件` 的 `src/main.rs` 不产生 `rs` 声称
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "entity-coverage" && i.message.contains("`rs`")),
            "路径引用不应派生 `rs` 声称: {:?}",
            issues
        );

        // 追加真编造名 → 仍报 1 条
        let mut content = std::fs::read_to_string(wiki.join("src.md")).unwrap();
        content.push_str("\n- `GhostFactory` — 编造的实体\n");
        std::fs::write(wiki.join("src.md"), content).unwrap();
        let issues = lint(&dir, &[dir.clone(), src.clone()]);
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(cov.len(), 1, "真编造应报 1 条, 实际: {:?}", issues);
        assert!(
            cov[0].message.contains("GhostFactory"),
            "应指向 GhostFactory: {}",
            cov[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A8 归属校验：本模块实体 → 放行；跨模块实体带真实 file:line 引用 → 放行；
    /// 跨模块实体无引用 → entity-ownership（error，核心新判定）。
    #[test]
    fn test_lint_entity_ownership_cross_module() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_cross_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // api.md：模块 m_a 属 Shared、模块 m_b 属 Local（带 file:line 定位）
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m_a\n\n- `Shared` — 描述 — src/shared.rs:1\n\n## m_b\n\n- `Local` — 描述 — src/local.rs:1\n",
        )
        .unwrap();
        // m_a.md：Shared（本模块）+ Local（跨模块，带引用）
        std::fs::write(
            wiki.join("m_a.md"),
            "# M_A\n\n- `Shared` — 本模块实体\n- `Local` — 跨模块实体 src/local.rs:1\n",
        )
        .unwrap();
        // m_b.md：Local（本模块）+ Shared（跨模块，无引用）
        std::fs::write(
            wiki.join("m_b.md"),
            "# M_B\n\n- `Local` — 本模块实体\n- `Shared` — 跨模块实体无引用\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        // m_a.md：Shared 本模块放行；Local 跨模块有引用放行 → 无归属问题
        assert!(
            !issues
                .iter()
                .any(|i| i.kind == "entity-ownership" && i.path.ends_with("m_a.md")),
            "跨模块带引用应放行, 实际: {:?}",
            issues
        );
        // m_b.md：Local 本模块放行；Shared 跨模块无引用 → 报 entity-ownership
        let own: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-ownership" && i.path.ends_with("m_b.md"))
            .collect();
        assert_eq!(own.len(), 1, "跨模块无引用应报归属错误, 实际: {:?}", issues);
        assert!(
            own[0].message.contains("Shared") && own[0].message.contains("m_a"),
            "应指向 Shared 与其归属模块 m_a: {}",
            own[0].message
        );
        // Shared/Local 都是 api 权威实体 → 不得误报 entity-coverage
        assert!(
            !issues.iter().any(|i| i.kind == "entity-coverage"),
            "api 权威实体不应误报覆盖率: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A8 归属校验：合成页（architecture.md）只做存在性——编造实体报
    /// entity-coverage，模块名引用放行，不触发归属校验（无模块归属）。
    #[test]
    fn test_lint_entity_ownership_synthetic_page_exists_only() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_synth_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## src\n\n- `Foo` — src/main.rs:1\n",
        )
        .unwrap();
        // 合成页：模块名 src 引用 + 编造实体 GhostThing
        std::fs::write(
            wiki.join("architecture.md"),
            "# 架构\n\n## 模块\n\n- `src` — 核心模块\n- `GhostThing` — 编造的实体\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(cov.len(), 1, "合成页只做存在性检查, 实际: {:?}", issues);
        assert!(
            cov[0].message.contains("GhostThing"),
            "应指向编造实体: {}",
            cov[0].message
        );
        assert!(
            !issues.iter().any(|i| i.kind == "entity-ownership"),
            "合成页不应触发归属校验: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A8 归属校验：编造名与真实文件 stem 同名（DEFECT-B 宽容收窄）——
    /// 仅命中 source_path_names → 降为告警级 entity-ownership（不再完全
    /// 静默）；完全编造（无任何命中）→ 保持 entity-coverage。
    #[test]
    fn test_lint_entity_ownership_stem_collision_warns() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_stem_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        // 源码：authenticator.rs（真实文件，内含真实实体 RealAuth）
        std::fs::write(src.join("authenticator.rs"), "pub fn RealAuth() {}\n").unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `Foo` — m.rs:1\n",
        )
        .unwrap();
        // 模块页 m.md：声称 authenticator（撞 stem）+ GhostNope（纯编造）
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n- `authenticator` — 目录/文件引用\n- `GhostNope` — 编造的实体\n",
        )
        .unwrap();

        let issues = lint(&dir, &[src]);
        let warn: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-ownership" && i.severity == Severity::Warning)
            .collect();
        assert_eq!(warn.len(), 1, "stem 撞名应降为告警, 实际: {:?}", issues);
        assert!(
            warn[0].message.contains("authenticator"),
            "告警应指向撞 stem 的名字: {}",
            warn[0].message
        );
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(cov.len(), 1, "纯编造仍应报覆盖率, 实际: {:?}", issues);
        assert!(
            cov[0].message.contains("GhostNope"),
            "应指向 GhostNope: {}",
            cov[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2 规则 3 结构性修复：模块页无「相关文件」节时，源码实体
    /// （pub(crate) 等未进 api 权威集）靠「实体文件 stem == 模块短名」判定
    /// 文件级归属 → 放行。修复前 related_files 恒空导致规则 3 恒假，
    /// is_cjk 这类源码实体直接落规则 5 误报 entity-coverage。
    #[test]
    fn test_lint_entity_ownership_rule3_module_page_short_name_passes() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_rule3_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        // 源码：src/tokenize.rs 内定义 pub(crate) 实体（不进 api 权威集）
        std::fs::write(
            src.join("tokenize.rs"),
            "pub(crate) fn is_cjk(c: char) -> bool {}\n",
        )
        .unwrap();
        // api.md：模块节标题存在但无实体条目（如 src::search 空节场景）
        std::fs::write(wiki.join("api.md"), "# API 参考\n\n## src::tokenize\n").unwrap();
        // 模块页 src_tokenize.md：核心实体声称 is_cjk，无「相关文件」节
        std::fs::write(
            wiki.join("src_tokenize.md"),
            "# src::tokenize\n\n## 核心实体\n\n- `is_cjk(c: char) -> bool` — 定义于 src/tokenize.rs:15\n- `GhostThing` — 编造的实体\n",
        )
        .unwrap();

        let issues = lint(&dir, &[src]);
        // is_cjk 文件级归属正确 → 放行；GhostThing 全不满足 → 仍报 entity-coverage
        let cov: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-coverage")
            .collect();
        assert_eq!(
            cov.len(),
            1,
            "源码实体应放行、编造实体仍应报, 实际: {:?}",
            issues
        );
        assert!(
            cov[0].message.contains("GhostThing"),
            "应指向编造实体: {}",
            cov[0].message
        );
        assert!(
            !issues.iter().any(|i| i.kind == "entity-ownership"),
            "is_cjk 不应报归属问题: {:?}",
            issues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2 附带处理：规则 2 归属降级——跨模块声称且声称行自带 file:line 引用
    /// （引用文件与 api 权威文件不同，归属不可靠）→ 降为告警（Warning）
    /// 而非 error（`## tests` 伞模块聚类把 src/project.rs 的 path 归属到
    /// tests 的误报形态）。
    #[test]
    fn test_lint_entity_ownership_rule2_downgraded_when_claim_line_cites() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_rule2w_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // api.md：path 真实实体在 src/project.rs，但被 `## tests` 伞模块聚类归入
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## tests\n\n- `pub fn path(&self) -> &Path` — 项目根路径 — src/project.rs:42\n",
        )
        .unwrap();
        // src_config.md：声称 path，声称行自带指向 src/config/mod.rs 的引用
        // （文件与权威不同 → 归属不可靠）
        std::fs::write(
            wiki.join("src_config.md"),
            "# src::config\n\n## 核心实体\n\n- `path` — 路径模块（src/config/mod.rs:25）\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let own: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-ownership")
            .collect();
        assert_eq!(
            own.len(),
            1,
            "跨模块声称应报一条归属问题, 实际: {:?}",
            issues
        );
        assert!(
            own[0].severity == Severity::Warning,
            "声称行自带引用 → 归属降为告警, 实际: {:?}",
            own[0]
        );
        assert!(
            own[0].message.contains("path"),
            "应指向 path: {}",
            own[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2 附带处理：规则 2 归属降级——实体名过短（≤4 字符，如 `fs`）→ 降为
    /// 告警（Warning）。实体名过短是 api 权威集污染高发形态（类型/模块引用
    /// 名被伞模块聚类归属到错误模块）。
    #[test]
    fn test_lint_entity_ownership_rule2_downgraded_for_short_name() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_own_rule2s_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## tests\n\n- `pub fn fs(&self) -> &Path` — 文件系统 — src/fs.rs:42\n",
        )
        .unwrap();
        // 声称 fs，无引用（短名自身即降级触发条件）
        std::fs::write(
            wiki.join("src_config.md"),
            "# src::config\n\n## 核心实体\n\n- `fs` — 文件系统模块\n",
        )
        .unwrap();

        let issues = lint(&dir, &[]);
        let own: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "entity-ownership")
            .collect();
        assert_eq!(
            own.len(),
            1,
            "跨模块声称应报一条归属问题, 实际: {:?}",
            issues
        );
        assert!(
            own[0].severity == Severity::Warning,
            "过短实体名 → 归属降为告警, 实际: {:?}",
            own[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A8 stale 指纹化核心回归：状态指纹表含源文件时，touch（改 mtime 不改
    /// 内容）→ **不** stale。修复前 mtime 对比会把 CI checkout / touch 的
    /// 假 stale 误报为文档过期。
    #[test]
    fn test_lint_stale_fingerprint_touch_not_stale() {
        use crate::incremental::state::GenerationState;
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_fp_touch_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let src_file = src.join("lib.rs");
        let content = "pub fn f() {}\n";
        std::fs::write(&src_file, content).unwrap();
        // 页面先写（mtime 早于后续 touch）
        std::fs::write(
            wiki.join("lib.md"),
            "# Lib\n\n## 相关文件\n\n- `src/lib.rs`\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // 保存生成状态（指纹表记录当前内容；键 = 相对项目根路径 src/lib.rs）
        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::from([(
                "src/lib.rs".to_string(),
                GenerationState::compute_file_fingerprint(&src_file).unwrap(),
            )]),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
            tool_version: None,
            failed_modules: Vec::new(),
        };
        state.save(&dir.join(".state")).unwrap();
        // touch：重写完全相同内容（mtime 更新、内容不变）——mtime 兜底会误报
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src_file, content).unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "stale"),
            "touch 不改内容不应报 stale（指纹一致）, 实际: {:?}",
            issues
        );
    }

    /// A8 stale 指纹化：内容变更（指纹不一致）→ stale
    #[test]
    fn test_lint_stale_fingerprint_content_change_stale() {
        use crate::incremental::state::GenerationState;
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_fp_change_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let src_file = src.join("lib.rs");
        std::fs::write(&src_file, "pub fn f() {}\n").unwrap();
        std::fs::write(
            wiki.join("lib.md"),
            "# Lib\n\n## 相关文件\n\n- `src/lib.rs`\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::from([(
                "src/lib.rs".to_string(),
                GenerationState::compute_file_fingerprint(&src_file).unwrap(),
            )]),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
            tool_version: None,
            failed_modules: Vec::new(),
        };
        state.save(&dir.join(".state")).unwrap();
        // 改写内容（指纹必变）
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src_file, "pub fn updated() {}\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "stale" && i.path.ends_with("lib.md")),
            "内容变更应报 stale, 实际: {:?}",
            issues
        );
    }

    /// A8 stale 指纹化退化路径：状态不可读（无状态文件）→ 回退 mtime 对比，
    /// 源文件更新仍报 stale（现状逻辑兜底，不因状态缺失静默放行）。
    #[test]
    fn test_lint_stale_fingerprint_state_missing_mtime_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_fp_nostate_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let src_file = src.join("lib.rs");
        std::fs::write(&src_file, "pub fn f() {}\n").unwrap();
        std::fs::write(
            wiki.join("lib.md"),
            "# Lib\n\n## 相关文件\n\n- `src/lib.rs`\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // 不写 .state 状态文件；源文件更新（mtime 更新 + 内容变更）
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src_file, "pub fn updated() {}\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            issues.iter().any(|i| i.kind == "stale"),
            "状态缺失走 mtime 兜底仍应报 stale, 实际: {:?}",
            issues
        );
    }

    /// A8 stale 指纹化退化路径：状态可用但源文件不在指纹表（新文件）→
    /// 保守回退 mtime 对比（不误报也不静默）：新文件 mtime 晚于页面 → stale
    #[test]
    fn test_lint_stale_fingerprint_new_file_mtime_fallback() {
        use crate::incremental::state::GenerationState;
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_fp_new_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // 页面先写
        std::fs::write(
            wiki.join("lib.md"),
            "# Lib\n\n## 相关文件\n\n- `src/lib.rs`\n",
        )
        .unwrap();
        // 状态指纹表为空（该源文件从未在生成时扫描）
        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: std::collections::HashMap::new(),
            doc_fingerprints: std::collections::HashMap::new(),
            doc_modules: std::collections::HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
            tool_version: None,
            failed_modules: Vec::new(),
        };
        state.save(&dir.join(".state")).unwrap();
        // 新文件在状态保存后才创建（mtime 晚于页面）→ 不在指纹表 → mtime 兜底 stale
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            issues
                .iter()
                .any(|i| i.kind == "stale" && i.path.ends_with("lib.md")),
            "新文件（不在指纹表）走 mtime 兜底应报 stale, 实际: {:?}",
            issues
        );
    }

    /// A8 stale-entity 反向定位：模块页声称已删除实体 → 在该引用页上报
    /// stale-entity（而非 api.md），message 带"页面引用了已删除实体"。
    #[test]
    fn test_lint_stale_entity_reverse_located_on_page() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_rev_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        // 源码只有 alpha（beta 已删除）
        std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `alpha` — m.rs:1\n- `beta` — m.rs:2\n",
        )
        .unwrap();
        // 模块页 m.md 声称 beta（stale 实体）
        std::fs::write(wiki.join("m.md"), "# M\n\n- `beta` — 声称已删除实体\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&src));
        let stale: Vec<_> = issues.iter().filter(|i| i.kind == "stale-entity").collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(stale.len(), 1, "只应报 beta, 实际: {:?}", issues);
        assert!(
            stale[0].path.ends_with("m.md"),
            "反向定位应指向引用页, 实际: {}",
            stale[0].path
        );
        assert!(
            stale[0].message.contains("beta") && stale[0].message.contains("页面引用了已删除实体"),
            "message 应带反向定位描述: {}",
            stale[0].message
        );
    }

    /// A8 stale-entity 反向定位：stale 实体无人引用 → 仍在 api.md 兜底报
    /// （保留原信号，不丢失孤儿 stale 实体）。
    #[test]
    fn test_lint_stale_entity_unreferenced_falls_back_to_api() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_api_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `alpha` — m.rs:1\n- `beta` — m.rs:2\n",
        )
        .unwrap();
        // 无模块页声称 beta

        let issues = lint(&dir, std::slice::from_ref(&src));
        let stale: Vec<_> = issues.iter().filter(|i| i.kind == "stale-entity").collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            stale.len(),
            1,
            "无人引用的 stale 实体应兜底报, 实际: {:?}",
            issues
        );
        assert!(
            stale[0].path.ends_with("api.md"),
            "兜底应挂在 api.md: {}",
            stale[0].path
        );
        assert!(stale[0].message.contains("beta"));
    }

    /// A8 stale-entity 反向定位：源码存在且被页面引用的实体 → 不报 stale-entity
    #[test]
    fn test_lint_stale_entity_fresh_not_reported() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_fresh_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        let src = dir.join("src");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n## m\n\n- `alpha` — m.rs:1\n",
        )
        .unwrap();
        std::fs::write(wiki.join("m.md"), "# M\n\n- `alpha` — 源码存在的实体\n").unwrap();

        let issues = lint(&dir, std::slice::from_ref(&src));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "stale-entity"),
            "源码存在的实体不应报 stale-entity, 实际: {:?}",
            issues
        );
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
        assert!(
            !names.contains(&"fn".to_string()),
            "关键字不应被提取: {:?}",
            names
        );
        assert!(names.contains(&"foo_bar".to_string()));
    }

    /// R2 P0-c(a)：extract_entity_names 只扫「核心实体」等实体节，跳过
    /// 「依赖关系」/「使用方式」小节——依赖节声称是模块引用/外部 crate 名
    /// （`crate::project::ProjectRoot` 被截断成 `crate`、`anyhow` 被当实体、
    /// `path` 模块跨模块归属不可靠），不是实体声称。
    #[test]
    fn test_extract_entity_names_skips_dependency_usage_sections() {
        let content = "## 核心实体\n\n- `Server` — 服务\n\n## 依赖关系\n\n- `crate::project::ProjectRoot` — 依赖\n- `anyhow` — 外部 crate\n- `path` — 模块\n\n## 使用方式\n\n- `NotAnEntity` — 使用说明\n";
        let names = extract_entity_names(content, &std::collections::HashSet::new());
        assert!(
            names.contains(&"Server".to_string()),
            "核心实体节仍应提取: {:?}",
            names
        );
        assert!(
            !names.contains(&"crate".to_string()),
            "依赖节的 crate:: 声称不应截成 crate: {:?}",
            names
        );
        assert!(
            !names.contains(&"anyhow".to_string()),
            "依赖节的外部 crate 名不应被当实体: {:?}",
            names
        );
        assert!(
            !names.contains(&"path".to_string()),
            "依赖节的模块名不应被当实体: {:?}",
            names
        );
        assert!(
            !names.contains(&"NotAnEntity".to_string()),
            "使用节的声称不应被提取: {:?}",
            names
        );
    }

    /// v19 t03：单字符与纯数字 token 是 LLM 编造噪声（双仓库实测
    /// `P`/`_`/`a`/`2`），应被过滤以免污染 entity-coverage 声称侧；
    /// 多字符正常实体不受影响。
    #[test]
    fn test_entity_name_filters_noise_tokens() {
        assert_eq!(entity_name_from_signature("`P`"), None, "单字符应过滤");
        assert_eq!(
            entity_name_from_signature("`_`"),
            None,
            "下划线单字符应过滤"
        );
        assert_eq!(entity_name_from_signature("`2`"), None, "纯数字应过滤");
        assert_eq!(
            entity_name_from_signature("fn x()"),
            None,
            "单字符函数名应过滤"
        );
        let content =
            "## 核心实体\n\n- `Server`（struct）\n- `src` — 目录\n- `P` — 噪声\n- `2` — 数字\n";
        let names = extract_entity_names(content, &std::collections::HashSet::new());
        assert!(
            names.contains(&"Server".to_string()),
            "正常实体应保留: {:?}",
            names
        );
        assert!(
            names.contains(&"src".to_string()),
            "多字符实体应保留: {:?}",
            names
        );
        assert!(
            !names.contains(&"P".to_string()),
            "单字符噪声不应声称: {:?}",
            names
        );
        assert!(
            !names.contains(&"2".to_string()),
            "纯数字噪声不应声称: {:?}",
            names
        );
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

    /// R2 P0-c(b)：可见性/关键字修饰符前缀剥离——`pub(crate) fn is_cjk(...)`
    /// 的 `pub(crate)` 含 '('，修复前按第一个 '(' 截取把实体名取成 `pub`
    /// （api 权威集污染 + stale-entity 误报）。修复后取到 `is_cjk`。
    #[test]
    fn test_entity_name_strips_visibility_modifiers() {
        assert_eq!(
            entity_name_from_signature("pub(crate) fn is_cjk(c: char) -> bool"),
            Some("is_cjk".into()),
            "pub(crate) 的括号不得抢先于函数名括号"
        );
        assert_eq!(
            entity_name_from_signature("pub(super) fn connect() {}"),
            Some("connect".into())
        );
        assert_eq!(
            entity_name_from_signature("pub(in crate::foo) fn helper()"),
            Some("helper".into())
        );
        // 叠加修饰符 + extern ABI 串
        assert_eq!(
            entity_name_from_signature("pub async unsafe extern \"C\" fn run()"),
            Some("run".into())
        );
        assert_eq!(
            entity_name_from_signature("const fn compute() -> u32"),
            Some("compute".into())
        );
        // 无修饰符 / 以关键字开头的普通标识符不回退
        assert_eq!(
            entity_name_from_signature("pub fn authenticate(username: &str) -> Option<User>"),
            Some("authenticate".into())
        );
        assert_eq!(
            entity_name_from_signature("pubkey()"),
            Some("pubkey".into()),
            "以 pub 开头的标识符不是修饰符，不得剥离"
        );
        // impl 块签名（无 '('，`impl<'a> X<'a>`）：impl 块不是可命名叶子
        // 实体，返回 None（其类型名经 struct/enum/trait 声明行进权威集，
        // 不再把 `impl` 当实体名——旧行为是错误提取）
        assert_eq!(
            entity_name_from_signature("impl<'a> ModuleDetector<'a> {"),
            None
        );
    }

    /// v0.7.2 P0-1：声明关键字分支——impl 块 / type 别名 / 解构模式的语义与
    /// 函数/裸名不同，通用「最后标识符」提取会得到污染名。impl 块返回 None
    ///（其类型名已由 struct/enum/trait 行单独进权威集）；type 别名取别名本身
    ///（等号右侧被别名类型是污染名）；解构模式无实体名返回 None。
    #[test]
    fn test_entity_name_declaration_keyword_branches() {
        // impl 块（含泛型/无泛型/带 trait bound）→ None
        assert_eq!(entity_name_from_signature("impl<'a> X<'a> {"), None);
        assert_eq!(
            entity_name_from_signature("impl From<&EntitySummary> for EntityEntry {"),
            None
        );
        assert_eq!(
            entity_name_from_signature("impl Iterator for MyIter {"),
            None
        );
        assert_eq!(
            entity_name_from_signature("impl Default for Foo {"),
            None,
            "impl 块不是可命名叶子实体"
        );
        // type 别名 → 别名本身（不是等号右侧被别名类型 / struct 关键字）
        assert_eq!(
            entity_name_from_signature("type EdgeId = EdgeIndex<u32>"),
            Some("EdgeId".into())
        );
        assert_eq!(
            entity_name_from_signature("pub type Foo<T> = Bar<T>"),
            Some("Foo".into())
        );
        // 解构模式 → None
        assert_eq!(
            entity_name_from_signature("{ stdout, stderr, exitCode }"),
            None
        );
    }

    /// G2：产物中的 mermaid fence 语法错误 → bad-mermaid；合法图不报
    #[test]
    fn test_lint_bad_mermaid_detects_broken_diagram() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_mermaid_{}",
            std::process::id()
        ));
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
        assert!(
            bad[0].path.ends_with("bad.md"),
            "应指向坏图页面: {}",
            bad[0].path
        );
        assert!(
            bad[0].message.contains("Unterminated"),
            "错误消息应可读: {}",
            bad[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D1（N1）：符号漂移——api.md 权威清单中的实体在当前源码 AST 中不存在
    /// → 报 stale-entity（文档过期/实体已删除）；源码中存在的实体不报。
    /// 源码根为空（扫描失败/无源码）时跳过检查，不把"扫描失败"误报成"文档过期"
    #[test]
    fn test_lint_stale_entity_detects_deleted_symbol() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_stale_entity_{}",
            std::process::id()
        ));
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
        assert!(
            stale[0].message.contains("beta"),
            "应指向 beta: {}",
            stale[0].message
        );
        assert!(
            !stale[0].message.contains("alpha"),
            "源码存在的实体不应误报"
        );

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
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_dotdot_{}", std::process::id()));
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
        assert!(
            bad[0].message.contains("越界段 .."),
            "消息应说明越界: {}",
            bad[0].message
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 绝对路径 containment（本轮新增）：UNC 形态绝对路径（Windows 下
    /// extract_citations 会提取，Path::is_absolute 为 true）越出源码根 →
    /// bad-citation（project_root.join(abs)=abs 的绕过在调用方被拦截）。
    /// 修复前会直接对 root 外路径做 exists/read，本测试可检出。
    /// 仅 Windows 语义成立：Unix 下反斜杠是合法文件名符，`\\server\share`
    /// 非绝对路径，is_absolute 为 false → 不走 containment 分支。
    #[cfg(windows)]
    #[test]
    fn test_lint_bad_citation_rejects_out_of_root_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_cite_absout_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // UNC 绝对路径（`\\server\share\file.rs`）：root 外，按无效处理
        std::fs::write(
            wiki.join("m.md"),
            "# M\n\n核心逻辑见 `\\\\server\\share\\file.rs:1`\n",
        )
        .unwrap();

        let issues = lint(&out, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-citation").collect();
        assert_eq!(
            bad.len(),
            1,
            "root 外绝对路径引用应报 bad-citation, 实际: {:?}",
            issues
        );
        assert!(
            bad[0].message.contains("越出源码根"),
            "消息应说明越界: {}",
            bad[0].message
        );
    }

    /// v28 t06：vctx 只读校验——合法标记（文件存在/行区间有效/哈希正确）不报错。
    /// 哈希期望值硬编码为独立算法的已知输出（SHA-256("hello") 前 8 位 =
    /// 2cf24dba），防实现自身偏差（自洽计算无法发现"两侧同错"）。
    #[test]
    fn test_lint_vctx_valid_passes() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_ok_{}",
            std::process::id()
        ));
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
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_miss_{}",
            std::process::id()
        ));
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
        assert_eq!(
            bad.len(),
            2,
            "缺失文件与格式不完整各报一条, 实际: {:?}",
            issues
        );
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
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_range_{}",
            std::process::id()
        ));
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
        assert!(
            bad[0].message.contains("越界"),
            "消息应说明越界: {}",
            bad[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 t06：文件内容变更但标记保留旧哈希 → bad-vctx（哈希防内容漂移：
    /// 行号对、内容错也报警，补 bad-citation 结构校验之外的内容维度）
    #[test]
    fn test_lint_vctx_hash_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_hash_{}",
            std::process::id()
        ));
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
        assert!(
            bad[0].message.contains("哈希不匹配"),
            "消息应说明哈希不一致: {}",
            bad[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 绝对路径越界防护（本轮新增）：vctx 标记含 root 外绝对路径时按
    /// 无效处理（bad-vctx），不读取 root 外文件做哈希校验——project_root
    /// .join(abs)=abs 会绕过 resolve_source_path 的 containment，必须在调用
    /// 方拦截。修复前会直接读取 root 外文件并（哈希正确时）静默通过，
    /// 本测试可检出。
    #[test]
    fn test_lint_vctx_rejects_out_of_root_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_absout_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // root（= dir）外真实存在该文件，内容哈希恰好匹配标记（若被读取会
        // 静默通过哈希校验）——必须被 containment 拦截并报 bad-vctx
        let outside = std::env::temp_dir().join(format!("outside_abs_vctx_{}", std::process::id()));
        std::fs::write(&outside, "hello\n").unwrap();
        let abs = outside.to_string_lossy().to_string();
        std::fs::write(
            wiki.join("m.md"),
            format!("# M\n\n见 [[vctx:{abs}#L-1-L-1@2cf24dba]]\n"),
        )
        .unwrap();

        let issues = lint(&out, std::slice::from_ref(&dir));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
        let bad: Vec<_> = issues.iter().filter(|i| i.kind == "bad-vctx").collect();
        assert_eq!(
            bad.len(),
            1,
            "root 外绝对路径 vctx 应报 bad-vctx, 实际: {:?}",
            issues
        );
        assert!(
            bad[0].message.contains("越出源码根"),
            "消息应说明越界: {}",
            bad[0].message
        );
    }

    /// P1 绝对路径 containment 回归：vctx 标记指向 root 内绝对路径仍正常
    /// 通过哈希校验（containment 放行，不误报）
    #[test]
    fn test_lint_vctx_absolute_path_within_root_passes() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_absin_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // root 内源文件（content "hello" → SHA-256 前 8 位 2cf24dba）
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let src_file = dir.join("src").join("real.rs");
        std::fs::write(&src_file, "hello\n").unwrap();
        let abs = src_file.to_string_lossy().to_string();
        std::fs::write(
            wiki.join("m.md"),
            format!("# M\n\n见 [[vctx:{abs}#L-1-L-1@2cf24dba]]\n"),
        )
        .unwrap();

        let issues = lint(&out, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "bad-vctx"),
            "root 内绝对路径 vctx 不应报错, 实际: {:?}",
            issues
        );
    }

    /// v0.7.2 P0-2：api.md 是机器生成页——render_api_reference 会把 lint.rs
    /// 自身文档注释里的 vctx 协议示例文本（`[[vctx:path#L-<start>-L-<end>@<hash8>]]`）
    /// 渲染进 api.md，naive 扫描 fail-closed 报格式错 → 每次 regen 都是持久
    /// 误报。跳过生成页后 0 bad-vctx（模块页人工标记仍由既有 test_lint_vctx_*
    /// 校验，_log 手写日志保留检查）。
    #[test]
    fn test_lint_vctx_skips_generated_pages() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_vctx_apimd_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join(".code-repo-wiki");
        let wiki = out.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        // api.md 内嵌协议示例文本（doc 注释渲染产物，`<start>` 非数字必然
        // 解析失败——修复前每次 regen 都报 bad-vctx）
        std::fs::write(
            wiki.join("api.md"),
            "# API 参考\n\n[[vctx:path#L-<start>-L-<end>@<hash8>]]\n",
        )
        .unwrap();
        let issues = lint(&out, &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !issues.iter().any(|i| i.kind == "bad-vctx"),
            "api.md 内嵌示例不应报 bad-vctx, 实际: {:?}",
            issues
        );
    }

    /// v0.7.2 P0-3：dependency-fabricated 磁盘级接线——模块页「## 依赖关系」
    /// 节声称对照权威集（export_snapshot 模块依赖 + 模块文件 imports 顶级
    /// crate + std/core）校验。本用例覆盖：外部 crate 声称放行（tokio 被
    /// 导入）、编造外部名报 UnknownExternal、模块存在但非本模块依赖报
    /// NotADependency、诚实标记 + 围栏内声称跳过。权威集完整（源码根可
    /// 解析模块文件 imports）→ Error 级。
    #[test]
    fn test_lint_dependency_fabricated_detects_and_allows() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_lint_depfab_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".state")).unwrap();
        // 权威集快照：src::m 无依赖（src::db 是真实模块但非其依赖）；
        // files 供文件→模块映射（src::m 关联 src/m.rs）
        std::fs::write(
            dir.join(".state").join("export_snapshot.json"),
            r#"{
              "version": 1,
              "documents": [],
              "cards": [],
              "modules": [
                {"name": "src::m", "files": ["src/m.rs"], "cohesion": 1.0, "coupling": 0.0, "features": [], "dependencies": []},
                {"name": "src::db", "files": [], "cohesion": 1.0, "coupling": 0.0, "features": [], "dependencies": []}
              ]
            }"#,
        )
        .unwrap();
        // 源文件先写（页面后写，避免页面被误判过时）：src::m 导入 tokio
        std::fs::write(
            dir.join("src").join("m.rs"),
            "use tokio::time;\n\npub fn helper() {}\n",
        )
        .unwrap();
        // api.md 让 primary_language 判定主语言（无实体行，不产 issues）
        std::fs::write(wiki.join("api.md"), "# API 参考\n").unwrap();
        // 模块页：真实依赖、真实导入 crate、编造外部名、诚实标记、围栏示例
        std::fs::write(
            wiki.join("src_m.md"),
            "# M\n\n## 依赖关系\n\n- src::db — 持久层\n- tokio — 异步运行时\n- totally_made_up_crate — 编造\n- 某外部服务 — （信息不足）\n\n```rust\n- fake_crate_in_fence\n```\n\n## 使用方式\n用法\n",
        )
        .unwrap();

        let issues = lint(&dir, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        let dep: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "dependency-fabricated")
            .collect();
        assert_eq!(dep.len(), 2, "应捕获 2 条虚构依赖, 实际: {:?}", issues);
        // 外部 crate 声称放行（tokio 被 src::m 导入）
        assert!(
            !issues.iter().any(|i| i.message.contains("tokio")),
            "被导入 crate 不应判违反: {:?}",
            issues
        );
        // 诚实标记 + 围栏内声称跳过（不产生违反）
        assert!(
            !issues
                .iter()
                .any(|i| i.message.contains("fake_crate_in_fence")),
            "围栏内声称不应判违反: {:?}",
            issues
        );
        // 编造外部名 → UnknownExternal（Error，权威集完整）
        let fabricated = dep
            .iter()
            .find(|i| i.message.contains("totally_made_up_crate"))
            .expect("编造外部名应报 UnknownExternal");
        assert!(
            fabricated.message.contains("疑似编造"),
            "{}",
            fabricated.message
        );
        assert_eq!(fabricated.severity, Severity::Error);
        // 模块存在但非本模块依赖 → NotADependency（Error）
        let not_dep = dep
            .iter()
            .find(|i| i.message.contains("src::db"))
            .expect("非本模块依赖应报 NotADependency");
        assert!(
            not_dep.message.contains("不在本模块的依赖列表中"),
            "{}",
            not_dep.message
        );
        assert_eq!(not_dep.severity, Severity::Error);
    }

    /// v0.7.2 P0-3 降级路径：权威集不完整（模块有文件但源码根缺失/未解析，
    /// 本用例 source_roots 为空）→ 外部 crate 判定全部不可信 → 降级 Warning
    ///（避免高误报阻断 CI；快照缺失/不可读则完全跳过）
    #[test]
    fn test_lint_dependency_fabricated_warns_when_authority_incomplete() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_lint_depfab_warn_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(dir.join(".state")).unwrap();
        std::fs::write(
            dir.join(".state").join("export_snapshot.json"),
            r#"{"version":1,"documents":[],"cards":[],"modules":[
                {"name":"src::m","files":["src/m.rs"],"cohesion":1.0,"coupling":0.0,"features":[],"dependencies":[]}
            ]}"#,
        )
        .unwrap();
        std::fs::write(wiki.join("api.md"), "# API 参考\n").unwrap();
        std::fs::write(
            wiki.join("src_m.md"),
            "# M\n\n## 依赖关系\n\n- totally_made_up_crate\n",
        )
        .unwrap();

        // 无源码根：模块文件 src/m.rs 无法解析到 imports 来源 → 权威集不完整
        let issues = lint(&dir, &[]);
        let _ = std::fs::remove_dir_all(&dir);
        let dep: Vec<_> = issues
            .iter()
            .filter(|i| i.kind == "dependency-fabricated")
            .collect();
        assert_eq!(dep.len(), 1, "应报 1 条, 实际: {:?}", issues);
        assert_eq!(
            dep[0].severity,
            Severity::Warning,
            "权威集不完整应降级 Warning: {:?}",
            dep[0]
        );
    }
}
