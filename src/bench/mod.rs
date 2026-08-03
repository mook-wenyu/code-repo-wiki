//! 评测基准自动层（U10，对齐 RepoDocBench 协议的可落地子集）
//!
//! `repo-wiki bench` 对目标仓库跑五维自动评测，输出 Markdown/JSON 报告：
//!
//! 1. **Coverage 实体提及率**：AST 提取实体清单（复用 ingest 解析），
//!    统计每个实体在 Wiki 产物中的被提及数 → 提及/总数（RepoDoc 定义：
//!    文档覆盖 public API 的比例）。
//! 2. **Doc Info 文本统计**：页面数 / 词数 / 交叉引用数 / 代码块数 /
//!    Mermaid 图数（全部确定性统计，不调用 LLM）。
//! 3. **lint 健康**：复用 lint 6 类检查（孤儿页/断链/过时/bad-citation/
//!    entity-coverage/bad-mermaid），问题数即质量分。
//! 4. **Update Recall 增量召回**：git commit 回放（最多 20 个），逐个
//!    checkout 后跑增量更新，统计"有源码变更的 commit 中成功触发
//!    重生成的占比"——增量链路正确性的可复现指标（对齐 RepoDoc 的
//!    Update Recall：正确更新的组件 / 需更新的组件）。
//! 5. **Time 耗时**：扫描/增量生成各阶段耗时（LLM 侧用 mock provider，
//!    耗时反映流水线确定性开销，与模型无关）。
//!
//! LLM 裁判层（TQS 打分）在 U11，需真实 API key，独立子命令。

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use crate::config::schema::WikiConfig;
use crate::project::ProjectRoot;

/// 增量回放的最大 commit 数（对齐 RepoDoc 每仓库 20 commit 协议）
const MAX_RECALL_COMMITS: usize = 20;

/// 评测报告（Markdown 与 JSON 的公共数据源，JSON 由 serde 直出）
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// 仓库名（报告标识，缺省取 root 目录名）
    pub repo_name: String,
    /// 评测时间（ISO 8601）
    pub generated_at: String,
    pub coverage: CoverageReport,
    pub doc_info: DocInfoReport,
    pub lint: LintReport,
    pub update_recall: UpdateRecallReport,
    pub time: TimeReport,
}

/// 维度 1：实体覆盖率
#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    /// 实体总数（AST 解析产出，去重）
    pub total_entities: usize,
    /// 在产物中被提及的实体数
    pub covered_entities: usize,
    /// 覆盖率 = covered / total（total 为 0 时为 1.0 空集约定）
    pub ratio: f64,
}

/// 维度 2：文本统计
#[derive(Debug, Clone, Serialize)]
pub struct DocInfoReport {
    /// 产物页面数（wiki/{lang}/*.md）
    pub pages: usize,
    /// 全部页面词数（空白分隔，去 markdown 围栏不影响统计口径）
    pub words: usize,
    /// 交叉引用链接数（[文本](目标) 形态）
    pub cross_references: usize,
    /// 代码块数（``` 围栏对）
    pub code_blocks: usize,
    /// Mermaid 图数
    pub diagrams: usize,
}

/// 维度 3：lint 健康
#[derive(Debug, Clone, Serialize)]
pub struct LintReport {
    /// 问题总数（0 = 健康）
    pub total_issues: usize,
    /// 各类问题计数（kind → 数量）
    pub by_kind: std::collections::BTreeMap<String, usize>,
}

/// 维度 4：增量召回
#[derive(Debug, Clone, Serialize)]
pub struct UpdateRecallReport {
    /// 回放的 commit 数（≤ MAX_RECALL_COMMITS）
    pub commits_scanned: usize,
    /// 有源码变更的 commit 数（前置条件：无变更 commit 不要求触发）
    pub commits_with_changes: usize,
    /// 成功触发重生成的 commit 数（documents 非空）
    pub correctly_updated: usize,
    /// 召回率 = correctly / with_changes（with_changes 为 0 时为 1.0 空集约定）
    pub recall: f64,
}

/// 维度 5：耗时
#[derive(Debug, Clone, Serialize)]
pub struct TimeReport {
    /// 扫描 + 解析耗时（毫秒）
    pub scan_ms: u64,
    /// 增量更新流水线耗时（毫秒）
    pub generate_ms: u64,
    /// 总耗时（毫秒）
    pub total_ms: u64,
}

/// 收集全部产物页内容（wiki/{lang}/*.md，主语言 + 扩展语言）
fn collect_wiki_pages(output_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut pages = Vec::new();
    let Ok(entries) = std::fs::read_dir(output_dir.join("wiki")) else {
        return pages;
    };
    for lang in entries.flatten() {
        if !lang.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(lang.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                pages.push((path, content));
            }
        }
    }
    pages
}

/// 维度 1：实体覆盖率
///
/// 实体清单 = 全仓库 AST 解析结果（含各语言差异点：Go const/var、
/// TS enum、Java 构造器、Python 模块常量），去重后逐一在产物文本中
/// 做子串包含判定（实体名是文档必提的标识符，子串口径与 RepoDoc 的
/// AST 提及率一致——同名误报由名称唯一性控制）。
fn measure_coverage(root: &ProjectRoot, config: &WikiConfig, pages: &[(PathBuf, String)]) -> Result<CoverageReport> {
    let insights = crate::ingest::scan_and_parse_at(root, config)?;
    let mut entities: Vec<String> = insights
        .iter()
        .flat_map(|i| i.entities.iter().map(|e| e.name.clone()))
        .collect();
    entities.sort();
    entities.dedup();
    let total = entities.len();

    let corpus: String = pages
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let covered = entities
        .iter()
        .filter(|name| corpus.contains(name.as_str()))
        .count();
    let ratio = if total == 0 { 1.0 } else { covered as f64 / total as f64 };
    Ok(CoverageReport { total_entities: total, covered_entities: covered, ratio })
}

/// 维度 2：文本统计
fn measure_doc_info(pages: &[(PathBuf, String)]) -> DocInfoReport {
    let mut words = 0usize;
    let mut cross_references = 0usize;
    let mut code_blocks = 0usize;
    let mut diagrams = 0usize;

    for (_, content) in pages {
        words += content.split_whitespace().count();
        // 交叉引用：[文本](目标) 形态（粗粒度统计：`](` 出现次数）
        cross_references += content.matches("](").count();
        // 代码块围栏对数：``` 行数 / 2（未闭合按 1 对计，结构损坏由 lint 暴露）
        let fences = content.lines().filter(|l| l.trim_start().starts_with("```")).count();
        code_blocks += fences.div_ceil(2);
        // Mermaid 图数：```mermaid 起始围栏数
        diagrams += content.lines().filter(|l| l.trim_start().starts_with("```mermaid")).count();
    }

    DocInfoReport {
        pages: pages.len(),
        words,
        cross_references,
        code_blocks,
        diagrams,
    }
}

/// 维度 3：lint 健康（复用 lint 6 类检查，问题数即质量分）
fn measure_lint(output_dir: &Path, config: &WikiConfig) -> LintReport {
    let source_roots = crate::commands::source_roots_from_include(&config.scope.include);
    let issues = crate::output::lint::lint(output_dir, &source_roots);
    let mut by_kind: std::collections::BTreeMap<String, usize> = Default::default();
    for issue in &issues {
        *by_kind.entry(issue.kind.to_string()).or_default() += 1;
    }
    LintReport { total_issues: issues.len(), by_kind }
}

/// 维度 4：增量召回（git commit 回放）
///
/// 逐 commit checkout 到工作区 → 跑增量更新（mock provider，不触网）→
/// 记录该 commit 是否有源码变更（git diff 判定）以及是否成功触发重生成
/// （run_pipeline 返回 documents 非空 = 有页面被重生成）。
/// 召回率 = 触发重生成的变更 commit / 有变更的 commit。
/// 边界：非 git 仓库返回空集（recall = 1.0 空集约定）；commit 不足 20 个
/// 按实际数量回放；checkout 失败（脏工作区/文件冲突）跳过该 commit 并告警。
fn measure_update_recall(
    config_path: &Path,
    root: &ProjectRoot,
) -> Result<UpdateRecallReport> {
    let repo = match git2::Repository::open(root.path()) {
        Ok(r) => r,
        Err(_) => {
            // 非 git 仓库：增量回放无 commit 可循（与 get_head_commit_hash_at
            // 的非 git 空值语义一致），报告空集而非报错
            tracing::warn!("bench: 非 git 仓库，增量召回维度跳过");
            return Ok(UpdateRecallReport {
                commits_scanned: 0,
                commits_with_changes: 0,
                correctly_updated: 0,
                recall: 1.0,
            });
        }
    };

    // 回放安全闸：工作区必须干净。回放用 git reset --hard 逐 commit
    // 回滚工作区与 HEAD，未提交改动会被**直接吞噬且无法恢复**（实测事故：
    // 脏工作区跑 bench 导致全部未提交改动丢失）。因此评测前强制检查，
    // 有未提交改动即拒绝执行——安全边界优先于评测便利性，宁可拒绝也不
    // 静默破坏用户数据（与"禁止兜底掩盖 bug"同源：这里是禁止兜底掩盖
    // 数据丢失）。
    let statuses = repo
        .statuses(None)
        .context("bench: 读取 git 状态失败")?;
    if !statuses.is_empty() {
        anyhow::bail!(
            "评测前工作区必须干净（存在 {} 个未提交改动），请先 git commit 或 stash 后再运行 bench——回放会 reset --hard，未提交改动将被丢弃",
            statuses.len()
        );
    }

    // 收集最近 MAX_RECALL_COMMITS 个 commit（revwalk 从 HEAD 起）
    let mut commits = Vec::new();
    if let Ok(mut walk) = repo.revwalk() {
        walk.push_head().ok();
        for oid in walk.flatten().take(MAX_RECALL_COMMITS) {
            if let Ok(commit) = repo.find_commit(oid) {
                commits.push(commit);
            }
        }
    }
    commits.reverse(); // 从旧到新回放

    let mut scanned = 0usize;
    let mut with_changes = 0usize;
    let mut correctly_updated = 0usize;

    // 记录原 HEAD（回放结束恢复；git2 reset 移动 HEAD 后，增量 diff 的
    // 基准（状态 last_commit_hash → HEAD）与回放 commit 对应，才能验证
    // 增量链路；仅 checkout_tree 不移动 HEAD，diff 基准恒为最新 commit，
    // 回放会得到错误的"无变更"短路）
    let original_head = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.id());

    for (i, commit) in commits.iter().enumerate() {
        let commit_id = commit.id();
        // reset --hard 到该 commit：工作区与 HEAD 均恢复（回放语义，
        // 每次回放到干净的 commit 状态；产物/状态一并回滚，由 update 重建）
        let obj = match repo.find_object(commit_id, None) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("bench: commit {commit_id} 对象解析失败，跳过: {e}");
                continue;
            }
        };
        if let Err(e) = repo.reset(&obj, git2::ResetType::Hard, None) {
            tracing::warn!("bench: commit {commit_id} reset 失败，跳过: {e}");
            continue;
        }
        scanned += 1;

        // 有源码变更判定：相对前一 commit 的 diff（首个 commit 视为基线）
        let has_changes = if i == 0 {
            false
        } else {
            let prev_tree = commits[i - 1].tree().ok();
            let cur_tree = commit.tree().ok();
            matches!(prev_tree.as_ref().zip(cur_tree.as_ref()), Some((a, b)) if {
                repo.diff_tree_to_tree(Some(a), Some(b), None).map(|d| d.deltas().len() > 0).unwrap_or(false)
            })
        };
        if has_changes {
            with_changes += 1;
        }

        // 增量更新（mock provider）：documents 非空 = 触发重生成
        let result = crate::run_pipeline(
            config_path,
            None,
            false,
            root,
            &crate::GenerationMode::Incremental {
                watch_paths: Vec::new(),
                change_kind: None,
            },
        );
        match result {
            Ok(res) if !res.documents.is_empty() => {
                if has_changes {
                    correctly_updated += 1;
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("bench: commit {commit_id} 增量更新失败（跳过判定）: {e}");
            }
        }
    }

    // 恢复原始 HEAD（回放期间 reset 移动了 HEAD，恢复避免污染用户仓库）
    if let Some(oid) = original_head
        && let Ok(obj) = repo.find_object(oid, None)
    {
        let _ = repo.reset(&obj, git2::ResetType::Hard, None);
    }

    let recall = if with_changes == 0 { 1.0 } else { correctly_updated as f64 / with_changes as f64 };
    Ok(UpdateRecallReport {
        commits_scanned: scanned,
        commits_with_changes: with_changes,
        correctly_updated,
        recall,
    })
}

/// 运行自动层评测，返回报告
///
/// `config_path` 为目标仓库的配置文件路径（增量回放复用同一份配置，
/// 注意：回放会 checkout 目标仓库的 git commit——这是评测语义的一部分，
/// 运行前请确认工作区无未提交改动（脏工作区会跳过对应 commit）。
pub fn run_bench(
    config_path: &Path,
    root: &ProjectRoot,
    config: &WikiConfig,
    repo_name: &str,
) -> Result<BenchReport> {
    let start = Instant::now();

    let scan_start = Instant::now();
    let pages = collect_wiki_pages(Path::new(&config.output.dir));
    let coverage = measure_coverage(root, config, &pages)?;
    let scan_ms = scan_start.elapsed().as_millis() as u64;

    let doc_info = measure_doc_info(&pages);
    let lint = measure_lint(Path::new(&config.output.dir), config);

    let gen_start = Instant::now();
    let update_recall = measure_update_recall(config_path, root)?;
    let generate_ms = gen_start.elapsed().as_millis() as u64;

    Ok(BenchReport {
        repo_name: repo_name.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        coverage,
        doc_info,
        lint,
        update_recall,
        time: TimeReport {
            scan_ms,
            generate_ms,
            total_ms: start.elapsed().as_millis() as u64,
        },
    })
}

/// 渲染 Markdown 报告（人类可读，CI/人工复跑对比用）
pub fn render_markdown(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 评测报告: {}\n\n", report.repo_name));
    out.push_str(&format!("> 生成时间: {}\n\n", report.generated_at));

    out.push_str("## 1. 实体覆盖率（Coverage）\n\n");
    out.push_str(&format!(
        "- 实体总数: {}\n- 已覆盖: {}（{:.1}%）\n\n",
        report.coverage.total_entities,
        report.coverage.covered_entities,
        report.coverage.ratio * 100.0
    ));

    out.push_str("## 2. 文本统计（Doc Info）\n\n");
    out.push_str(&format!(
        "- 页面: {}\n- 词数: {}\n- 交叉引用: {}\n- 代码块: {}\n- Mermaid 图: {}\n\n",
        report.doc_info.pages,
        report.doc_info.words,
        report.doc_info.cross_references,
        report.doc_info.code_blocks,
        report.doc_info.diagrams
    ));

    out.push_str("## 3. lint 健康\n\n");
    if report.lint.total_issues == 0 {
        out.push_str("- 通过（无孤儿页/断链/过时/引用/覆盖/mermaid 问题）\n\n");
    } else {
        out.push_str(&format!("- 问题总数: {}\n", report.lint.total_issues));
        for (kind, count) in &report.lint.by_kind {
            out.push_str(&format!("  - {kind}: {count}\n"));
        }
        out.push('\n');
    }

    out.push_str("## 4. 增量召回（Update Recall）\n\n");
    out.push_str(&format!(
        "- 回放 commit: {}（上限 {}）\n- 有变更: {}\n- 正确更新: {}（{:.1}%）\n\n",
        report.update_recall.commits_scanned,
        MAX_RECALL_COMMITS,
        report.update_recall.commits_with_changes,
        report.update_recall.correctly_updated,
        report.update_recall.recall * 100.0
    ));

    out.push_str("## 5. 耗时（Time）\n\n");
    out.push_str(&format!(
        "- 扫描: {}ms\n- 增量: {}ms\n- 总计: {}ms\n",
        report.time.scan_ms, report.time.generate_ms, report.time.total_ms
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{LlmProviderType, LlmSection, OutputSection, WikiSection};
    use std::path::PathBuf;

    /// 构造临时小仓库：src/a.rs + src/b.rs（含 git 仓库，供增量回放）
    fn bench_repo(tag: &str) -> (ProjectRoot, PathBuf, WikiConfig) {
        let dir = std::env::temp_dir().join(format!("repo_wiki_bench_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha(x: u32) -> u32 { x + 1 }\n").unwrap();
        std::fs::write(dir.join("src").join("b.rs"), "pub fn beta(x: u32) -> u32 { x + 2 }\n").unwrap();

        let config = WikiConfig {
            output: OutputSection { dir: dir.join(".repo-wiki").to_string_lossy().into_owned() },
            wiki: WikiSection { language: "zh".into(), ..Default::default() },
            llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
            ..Default::default()
        };
        std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();

        // git init（增量回放的前置条件；首次提交需签名）
        let git = git2::Repository::init(&dir).unwrap();
        let mut cfg = git.config().unwrap();
        cfg.set_str("user.name", "bench").unwrap();
        cfg.set_str("user.email", "bench@test.com").unwrap();
        let root = ProjectRoot::new(dir.clone());
        (root, dir.join("config.toml"), config)
    }

    /// git2 提交当前工作区，返回 commit id
    fn commit_all(repo_path: &Path, message: &str) -> String {
        let repo = git2::Repository::open(repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("bench", "bench@test.com").unwrap();
        let commit_id = match repo.head().ok() {
            Some(head) => {
                let parent = head.peel_to_commit().unwrap();
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent]).unwrap()
            }
            None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]).unwrap(),
        };
        commit_id.to_string()
    }

    /// 覆盖率：全量生成后实体应全部被提及（mock 产物含模块页）
    #[test]
    fn test_coverage_after_generate() {
        let (root, config_path, config) = bench_repo("cov");
        commit_all(root.path(), "init");
        crate::run_pipeline(&config_path, None, false, &root, &crate::GenerationMode::Full).unwrap();

        let pages = collect_wiki_pages(Path::new(&config.output.dir));
        assert!(!pages.is_empty(), "全量生成后应有产物页");
        let cov = measure_coverage(&root, &config, &pages).unwrap();
        assert_eq!(cov.total_entities, 2, "应解析出 alpha/beta 两个实体");
        assert_eq!(cov.covered_entities, 2, "mock 生成后产物应提及全部实体");
        assert!((cov.ratio - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 覆盖率：无产物时覆盖率为 0（实体存在但无页面提及）
    #[test]
    fn test_coverage_zero_without_pages() {
        let (root, _, config) = bench_repo("cov0");
        let pages = collect_wiki_pages(Path::new(&config.output.dir));
        assert!(pages.is_empty());
        let cov = measure_coverage(&root, &config, &pages).unwrap();
        assert_eq!(cov.total_entities, 2);
        assert_eq!(cov.covered_entities, 0);
        assert!((cov.ratio - 0.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 文本统计：含 mermaid 与链接的页面各计数正确
    #[test]
    fn test_doc_info_counts() {
        let pages = vec![(
            PathBuf::from("wiki/zh/x.md"),
            "# 标题\n\n[模块](wiki/zh/a.md) 与 [源码](src/a.rs:1)。\n\n```rust\nfn x() {}\n```\n\n```mermaid\nflowchart LR\nA --> B\n```\n".to_string(),
        )];
        let info = measure_doc_info(&pages);
        assert_eq!(info.pages, 1);
        assert_eq!(info.cross_references, 2);
        assert_eq!(info.code_blocks, 2);
        assert_eq!(info.diagrams, 1);
        assert!(info.words > 0);
    }

    /// 增量召回：有变更的 commit 应全部触发重生成（mock 下正确更新）
    #[test]
    fn test_update_recall_with_changes() {
        let (root, config_path, _config) = bench_repo("recall");
        commit_all(root.path(), "init");
        crate::run_pipeline(&config_path, None, false, &root, &crate::GenerationMode::Full).unwrap();

        // 第二个 commit：修改 b.rs
        std::fs::write(root.path().join("src").join("b.rs"), "pub fn beta(x: u32) -> u32 { x + 100 }\n").unwrap();
        commit_all(root.path(), "change beta");

        let report = measure_update_recall(&config_path, &root).unwrap();
        assert_eq!(report.commits_scanned, 2, "应回放 2 个 commit");
        assert_eq!(report.commits_with_changes, 1, "第 2 个 commit 有变更");
        assert_eq!(report.correctly_updated, 1, "变更 commit 应正确触发重生成");
        assert!((report.recall - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 报告渲染：Markdown 报告含五个维度标题
    #[test]
    fn test_render_markdown_sections() {
        let report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 0, covered_entities: 0, ratio: 1.0 },
            doc_info: DocInfoReport { pages: 0, words: 0, cross_references: 0, code_blocks: 0, diagrams: 0 },
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport { commits_scanned: 0, commits_with_changes: 0, correctly_updated: 0, recall: 1.0 },
            time: TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 },
        };
        let md = render_markdown(&report);
        for section in ["实体覆盖率", "文本统计", "lint 健康", "增量召回", "耗时"] {
            assert!(md.contains(section), "报告应含 {section} 节: {md}");
        }
    }
}
