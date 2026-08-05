//! 第二档评测：多仓库清单跑分框架（v21 E 组，t10）
//!
//! 用途：对一批仓库（本地路径或 git URL）批量执行
//! Coverage / Doc Info / lint / Time 四个快维度，输出仓库×维度矩阵。
//! 设计决策：
//! - Update Recall（git commit 回放）在清单模式**跳过**——批量场景下
//!   回放会 reset --hard 工作区，安全闸误触与成本都不可接受；需要
//!   深度回放请用单仓库 `bench`（含 `--rubrics-only` 裁判模式）。
//! - 每个仓库的产物输出到 `work_dir/<仓库名>-out/`（覆盖模板配置的
//!   `output.dir`），互不污染；模板配置其余字段（scope/llm/…）原样生效。
//! - 单仓库失败（clone 失败/扫描失败）记录到该行 `error` 字段后继续——
//!   批量评测的外部依赖失败不应中断整批（与流水线"失败只告警"策略一致）。
//! - 清单格式：每行一个仓库；`#` 开头为注释；空行跳过；
//!   本地路径直接使用，`https://…` / `git@…` 视为远程 URL（clone 到 work_dir）。

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::config::schema::WikiConfig;
use crate::project::ProjectRoot;

use super::{collect_wiki_pages, measure_coverage, measure_doc_info, measure_lint, DocInfoReport, LintReport, TimeReport, CoverageReport};

/// 清单中的单个仓库条目（本地路径与远程 URL 二选一）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    /// 仓库名（远程 URL 必须显式指定；本地路径缺省取目录名）
    pub name: String,
    /// 远程 git URL（Some 时 local 必须为 None）
    pub url: Option<String>,
    /// 本地仓库路径（Some 时 url 必须为 None）
    pub local: Option<PathBuf>,
}

/// 单仓库跑分结果（error 非空表示该仓库未完成评测）
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoReport {
    pub name: String,
    pub coverage: CoverageReport,
    pub doc_info: DocInfoReport,
    pub lint: LintReport,
    pub time: TimeReport,
    /// 该仓库评测失败原因（clone/扫描/生成失败）；None=完成
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 清单跑分聚合报告（确定性：仓库顺序 = 清单顺序）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ManifestReport {
    /// 评测时间（ISO 8601）
    pub generated_at: String,
    pub repos: Vec<RepoReport>,
}

/// 解析清单文件：每行一个仓库（`#` 注释/空行跳过）
///
/// 行格式：
/// - 本地路径：`D:\path\to\repo` 或 `./relative`（相对清单文件所在目录解析）
/// - 远程 URL：`https://github.com/owner/repo`（仓库名取 URL 最后一段，去 `.git`）
/// - 带名形式：`名字 | 路径或URL`（路径/URL 中的 `|` 需谨慎，文档约定名字在前）
pub fn parse_manifest(path: &Path) -> Result<Vec<RepoEntry>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取清单文件失败: {}", path.display()))?;
    let manifest_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut entries = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, target) = match line.split_once('|') {
            Some((n, t)) => (n.trim().to_string(), t.trim()),
            None => (String::new(), line),
        };
        // 远程 URL 判定：http(s):// 或 git@ 前缀
        let entry = if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("git@")
        {
            let name = if name.is_empty() {
                // 取 URL 最后一段作为仓库名（去 .git 后缀）
                target
                    .trim_end_matches('/')
                    .rsplit(['/', ':'])
                    .next()
                    .unwrap_or("unknown")
                    .trim_end_matches(".git")
                    .to_string()
            } else {
                name
            };
            RepoEntry { name, url: Some(target.to_string()), local: None }
        } else {
            let name = if name.is_empty() {
                Path::new(target)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                name
            };
            // 相对路径基于清单文件所在目录解析（本地路径不校验存在性——
            // 执行期统一失败标注，便于批处理继续）
            let p = PathBuf::from(target);
            let abs = if p.is_absolute() { p } else { manifest_dir.join(p) };
            RepoEntry { name, url: None, local: Some(abs) }
        };
        if entry.name.is_empty() {
            bail!("清单第 {} 行解析失败（仓库名为空）: {line}", idx + 1);
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// 执行清单跑分：对每个仓库跑 mock/模板生成 + 四快维度测量
///
/// - `template_config`：模板配置（scope/llm/provider 等；`output.dir` 会被
///   每仓库覆盖为 `work_dir/<name>-out/`）
/// - `work_dir`：远程仓库 clone 落地目录 + 每仓库产物目录的父目录
/// - 远程 URL 克隆失败/本地路径不存在 → 该仓库 `error` 标注，继续下一仓库
pub fn run_manifest(
    entries: &[RepoEntry],
    template_config: &WikiConfig,
    work_dir: &Path,
) -> Result<ManifestReport> {
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("创建 work_dir 失败: {}", work_dir.display()))?;

    // 模板配置落盘（run_pipeline 从文件加载；output 覆盖由调用方传参）
    let template_path = work_dir.join("template-config.toml");
    std::fs::write(&template_path, toml::to_string_pretty(template_config)?)
        .with_context(|| "写模板配置文件失败")?;

    let mut repos = Vec::with_capacity(entries.len());
    for entry in entries {
        let start = Instant::now();
        // 解析仓库本地路径（远程 clone 到 work_dir/<name>）
        let repo_path = match (&entry.url, &entry.local) {
            (Some(url), _) => {
                let dest = work_dir.join(&entry.name);
                if !dest.exists() {
                    if let Err(e) = git2::Repository::clone(url, &dest) {
                        repos.push(RepoReport {
                            name: entry.name.clone(),
                            coverage: empty_coverage(),
                            doc_info: empty_doc_info(),
                            lint: empty_lint(),
                            time: empty_time(),
                            error: Some(format!("clone 失败: {e}")),
                        });
                        continue;
                    }
                }
                dest
            }
            (None, Some(local)) => {
                if !local.is_dir() {
                    repos.push(RepoReport {
                        name: entry.name.clone(),
                        coverage: empty_coverage(),
                        doc_info: empty_doc_info(),
                        lint: empty_lint(),
                        time: empty_time(),
                        error: Some(format!("本地路径不存在: {}", local.display())),
                    });
                    continue;
                }
                local.clone()
            }
            (None, None) => {
                repos.push(RepoReport {
                    name: entry.name.clone(),
                    coverage: empty_coverage(),
                    doc_info: empty_doc_info(),
                    lint: empty_lint(),
                    time: empty_time(),
                    error: Some("清单条目既无 url 也无 local".to_string()),
                });
                continue;
            }
        };

        // 每仓库独立产物目录：work_dir/<name>-out/（覆盖模板 output.dir）
        let out_dir = work_dir.join(format!("{}-out", entry.name));
        let root = ProjectRoot::new(repo_path);
        let res = crate::run_pipeline(
            &template_path,
            Some(&out_dir),
            true, // force：清单跑分语义=全量重建后测量
            &root,
            &crate::GenerationMode::Full,
        );
        let result = match res {
            Ok(_) => {
                let pages = collect_wiki_pages(&out_dir);
                let cov = measure_coverage(&root, &template_config, &pages);
                let doc_info = measure_doc_info(&pages);
                let lint = measure_lint(&out_dir, &template_config);
                RepoReport {
                    name: entry.name.clone(),
                    coverage: cov.unwrap_or_else(|e| {
                        RepoReport {
                            name: entry.name.clone(),
                            coverage: empty_coverage(),
                            doc_info: empty_doc_info(),
                            lint: empty_lint(),
                            time: empty_time(),
                            error: Some(format!("测量失败: {e}")),
                        }
                        .coverage
                    }),
                    doc_info,
                    lint,
                    time: TimeReport {
                        scan_ms: 0,
                        generate_ms: 0,
                        total_ms: start.elapsed().as_millis() as u64,
                    },
                    error: None,
                }
            }
            Err(e) => RepoReport {
                name: entry.name.clone(),
                coverage: empty_coverage(),
                doc_info: empty_doc_info(),
                lint: empty_lint(),
                time: empty_time(),
                error: Some(format!("生成失败: {e}")),
            },
        };
        repos.push(result);
    }

    Ok(ManifestReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        repos,
    })
}

fn empty_coverage() -> CoverageReport {
    CoverageReport { total_entities: 0, covered_entities: 0, ratio: 0.0 }
}
fn empty_doc_info() -> DocInfoReport {
    DocInfoReport { pages: 0, words: 0, cross_references: 0, code_blocks: 0, diagrams: 0 }
}
fn empty_lint() -> LintReport {
    LintReport { total_issues: 0, by_kind: Default::default() }
}
fn empty_time() -> TimeReport {
    TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 }
}

/// 渲染清单跑分 Markdown 矩阵（确定性：仓库顺序 = 清单顺序，不排序）
pub fn render_manifest_markdown(report: &ManifestReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 清单跑分报告（{} 仓库）\n\n", report.repos.len()));
    out.push_str(&format!("> 生成时间: {}\n\n", report.generated_at));
    out.push_str("| 仓库 | 实体 | 覆盖 | 页面 | 词数 | 交叉引用 | 代码块 | 图 | lint 问题 | 耗时(ms) |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for r in &report.repos {
        if let Some(err) = &r.error {
            out.push_str(&format!("| {} | — | — | — | — | — | — | — | **失败**: {err} | — |\n", r.name));
        } else {
            out.push_str(&format!(
                "| {} | {} | {:.1}% | {} | {} | {} | {} | {} | {} | {} |\n",
                r.name,
                r.coverage.total_entities,
                r.coverage.ratio * 100.0,
                r.doc_info.pages,
                r.doc_info.words,
                r.doc_info.cross_references,
                r.doc_info.code_blocks,
                r.doc_info.diagrams,
                r.lint.total_issues,
                r.time.total_ms,
            ));
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单解析：注释/空行/本地路径/URL/带名形式
    #[test]
    fn test_parse_manifest_formats() {
        let dir = std::env::temp_dir().join(format!("rw_manifest_parse_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.txt");
        std::fs::write(
            &manifest,
            "# 注释行\n\nD:\\tmp\\repo-a\nhttps://github.com/owner/repo-b.git\n命名仓库 | ./local-c\n",
        )
        .unwrap();
        let entries = parse_manifest(&manifest).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "repo-a");
        assert_eq!(entries[0].local.as_deref(), Some(Path::new("D:\\tmp\\repo-a")));
        assert_eq!(entries[1].name, "repo-b");
        assert_eq!(entries[1].url.as_deref(), Some("https://github.com/owner/repo-b.git"));
        assert_eq!(entries[2].name, "命名仓库");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清单跑分冒烟：两个本地小仓库（mock 生成确定性），一个本地路径不存在
    #[test]
    fn test_run_manifest_smoke() {
        let base = std::env::temp_dir().join(format!("rw_manifest_run_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // 两个小仓库 + 一个不存在的路径
        for name in ["repo-a", "repo-b"] {
            let r = base.join(name);
            std::fs::create_dir_all(r.join("src")).unwrap();
            std::fs::write(r.join("src").join("main.rs"), "pub fn alpha() {}\n").unwrap();
        }
        let manifest_path = base.join("manifest.txt");
        std::fs::write(
            &manifest_path,
            format!("{}\n{}\n{}\n", base.join("repo-a").display(), base.join("repo-b").display(), base.join("missing-repo").display()),
        )
        .unwrap();

        let template = WikiConfig {
            llm: crate::config::schema::LlmSection {
                provider: crate::config::schema::LlmProviderType::Mock,
                ..Default::default()
            },
            ..Default::default()
        };
        let work_dir = base.join("work");
        let entries = parse_manifest(&manifest_path).unwrap();
        let report = run_manifest(&entries, &template, &work_dir).unwrap();
        assert_eq!(report.repos.len(), 3);
        assert_eq!(report.repos[0].name, "repo-a");
        assert!(report.repos[0].error.is_none(), "repo-a 应成功: {:?}", report.repos[0].error);
        assert_eq!(report.repos[0].coverage.total_entities, 1, "应解析出 alpha");
        assert!(report.repos[0].doc_info.pages > 0, "mock 生成后应有产物页");
        assert!(report.repos[2].error.is_some(), "缺失路径应标注失败");
        let md = render_manifest_markdown(&report);
        assert!(md.contains("| repo-a |"), "矩阵应含 repo-a 行: {md}");
        assert!(md.contains("**失败**"), "矩阵应含失败标注: {md}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
