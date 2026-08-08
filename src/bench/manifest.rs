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

/// 清单中的单个仓库条目（本地路径与远程 URL 二选一；commit 可选钉死）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    /// 仓库名（远程 URL 必须显式指定；本地路径缺省取目录名）
    pub name: String,
    /// 远程 git URL（Some 时 local 必须为 None）
    pub url: Option<String>,
    /// 本地仓库路径（Some 时 url 必须为 None）
    pub local: Option<PathBuf>,
    /// 评测基准 commit（Some 时执行前 checkout 钉死——CodeWikiBench/
    /// knowing 类评测要求 commit 级可复现；None 用 HEAD）
    pub commit: Option<String>,
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
/// - 带 commit 形式：`名字 | 路径或URL | commit`（v28 t02 扩展：评测基准
///   钉死为指定 commit，保证跨次可复现；缺省 None 用 HEAD）
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
        // 行内分隔：无 `|` → 整行即目标；有 → 1-2 个 `|` 分隔 name/target/commit
        let (name, target, commit) = match line.split_once('|') {
            None => (String::new(), line.to_string(), None),
            Some(_) => {
                let mut parts = line.splitn(3, '|').map(str::trim);
                let n = parts.next().unwrap_or("").to_string();
                let t = parts.next().unwrap_or("").to_string();
                let c = parts.next().map(str::to_string);
                (n, t, c)
            }
        };
        if target.is_empty() {
            bail!("清单第 {} 行解析失败（缺少仓库路径）: {line}", idx + 1);
        }
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
                name.to_string()
            };
            RepoEntry { name, url: Some(target.to_string()), local: None, commit }
        } else {
            let name = if name.is_empty() {
                // 平台无关的路径文件名提取：反斜杠在非 Windows 平台不是分隔符，
                // 但清单里可能含 Windows 盘符路径（如 D:\tmp\repo-a），统一按
                // `/` 与 `\` 双分隔符取最后一段，避免 ubuntu 上把整串当文件名。
                target
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                name
            };
            // 相对路径基于清单文件所在目录解析（本地路径不校验存在性——
            // 执行期统一失败标注，便于批处理继续）
            let p = PathBuf::from(target);
            let abs = if p.is_absolute() { p } else { manifest_dir.join(p) };
            RepoEntry { name, url: None, local: Some(abs), commit }
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
                if !dest.exists()
                    && let Err(e) = git2::Repository::clone(url, &dest)
                {
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

        // commit 钉死：清单指定评测基准 commit 时 checkout（v28 t02）
        // 评测可复现性要求 commit 级确定性；本地路径条目同样生效
        if let Some(commit) = &entry.commit
            && let Err(e) = checkout_commit(&repo_path, commit)
        {
            repos.push(RepoReport {
                name: entry.name.clone(),
                coverage: empty_coverage(),
                doc_info: empty_doc_info(),
                lint: empty_lint(),
                time: empty_time(),
                error: Some(format!("checkout {commit} 失败: {e}")),
            });
            continue;
        }

        // 每仓库独立产物目录：work_dir/<name>-out/（覆盖模板 output.dir）
        let out_dir = work_dir.join(format!("{}-out", entry.name));
        let root = ProjectRoot::new(repo_path);
        let res = crate::run_pipeline(
            Some(&template_path),
            Some(&out_dir),
            true, // force：清单跑分语义=全量重建后测量
            &root,
            &crate::GenerationMode::Full,
        );
        let result = match res {
            Ok(_) => {
                let pages = collect_wiki_pages(&out_dir);
                let cov = measure_coverage(&root, &pages);
                let doc_info = measure_doc_info(&pages);
                let lint = measure_lint(&out_dir, &root);
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
    DocInfoReport {
        pages: 0,
        words: 0,
        cross_references: 0,
        code_blocks: 0,
        diagrams: 0,
        llm_judged: false,
        llm_score: 0.0,
        llm_judged_modules: 0,
        llm_abstain_modules: 0,
    }
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

/// checkout 钉死 commit：解析 `commit` 为仓库内对象并检出到工作树
///
/// 设计约束：
/// - 只接受可解析的 commit 引用（SHA 前缀/分支/标签均可——git2 语义），
///   失败返回错误由调用方标注该仓库失败（评测基准不可复现必须显式失败，
///   不得静默用 HEAD 顶替——那会让跨次结果失去可比性）。
/// - 失败时**不清理已有工作区**（保留 clone 产物便于人工排查）。
fn checkout_commit(repo_path: &Path, commit: &str) -> Result<()> {
    let repo = git2::Repository::open(repo_path)
        .with_context(|| format!("打开仓库失败: {}", repo_path.display()))?;
    let obj = repo
        .revparse_single(commit)
        .with_context(|| format!("commit 无法解析: {commit}"))?;
    repo.checkout_tree(&obj, None)
        .with_context(|| format!("checkout 树失败: {commit}"))?;
    repo.set_head_detached(obj.id())
        .with_context(|| "设置 HEAD 失败".to_string())?;
    Ok(())
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
            "# 注释行\n\nD:\\tmp\\repo-a\nhttps://github.com/owner/repo-b.git\n命名仓库 | ./local-c\n钉死版本 | https://github.com/owner/repo-d.git | abc1234\n",
        )
        .unwrap();
        let entries = parse_manifest(&manifest).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "repo-a");
        assert_eq!(entries[0].local.as_deref(), Some(Path::new("D:\\tmp\\repo-a")));
        assert_eq!(entries[0].commit, None);
        assert_eq!(entries[1].name, "repo-b");
        assert_eq!(entries[1].url.as_deref(), Some("https://github.com/owner/repo-b.git"));
        assert_eq!(entries[2].name, "命名仓库");
        assert_eq!(entries[3].name, "钉死版本");
        assert_eq!(entries[3].url.as_deref(), Some("https://github.com/owner/repo-d.git"));
        assert_eq!(entries[3].commit.as_deref(), Some("abc1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// checkout 钉死行为：临时 git 仓库两 commit，检出第一 commit 后工作树回退
    #[test]
    fn test_checkout_commit_pins_worktree() {
        use git2::Repository;
        let base = std::env::temp_dir().join(format!("rw_manifest_checkout_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let repo = Repository::init(&base).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let mut idx = repo.index().unwrap();
        std::fs::write(base.join("f.txt"), "v1\n").unwrap();
        idx.add_path(Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree1 = idx.write_tree().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "first", &repo.find_tree(tree1).unwrap(), &[]).unwrap();
        let c1 = repo.head().unwrap().peel_to_commit().unwrap().id();
        std::fs::write(base.join("f.txt"), "v2\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("f.txt")).unwrap();
        idx.write().unwrap();
        let tree2 = idx.write_tree().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &repo.find_tree(tree2).unwrap(), &[&repo.find_commit(c1).unwrap()]).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("f.txt")).unwrap(), "v2\n");
        checkout_commit(&base, &c1.to_string()).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("f.txt")).unwrap(), "v1\n", "钉死后工作树应为第一 commit 内容");
        assert!(checkout_commit(&base, "deadbeef00").is_err(), "不存在的 commit 应报错");
        let _ = std::fs::remove_dir_all(&base);
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
