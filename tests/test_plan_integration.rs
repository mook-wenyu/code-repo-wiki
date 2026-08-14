//! wiki_plan.yaml 端到端接线测试（v0.9 W1）
//!
//! 覆盖：scope 覆盖扫描范围（include/exclude）、repowiki.documents 生成
//! 自定义页面（parent 挂载 _toc）、无 plan 文件时默认行为（零破坏）。
//! 全部使用 Mock LLM Provider（无网络），root 经 ProjectRoot 显式注入。

use std::path::Path;

use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig, WikiSection};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 构造临时仓库：src/a/mod.rs + src/b/mod.rs + config.toml（mock provider）
/// + 可选 wiki_plan.yaml。
fn build_repo(repo: &Path, plan_yaml: Option<&str>) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src").join("a"))?;
    std::fs::create_dir_all(repo.join("src").join("b"))?;
    std::fs::write(
        repo.join("src").join("a").join("mod.rs"),
        r#"
//! 模块 A
pub struct Alpha;
impl Alpha { pub fn run(&self) -> u32 { 42 } }
"#,
    )?;
    std::fs::write(
        repo.join("src").join("b").join("mod.rs"),
        r#"
//! 模块 B
pub fn beta() -> &'static str { "beta" }
"#,
    )?;

    let config = WikiConfig {
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: "zh".into(),
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(repo.join("config.toml"), toml::to_string_pretty(&config)?)?;
    if let Some(plan) = plan_yaml {
        std::fs::write(repo.join("wiki_plan.yaml"), plan)?;
    }
    Ok(())
}

/// 列出现有 wiki 页面文件名集合（wiki/zh/*.md）
fn list_wiki_pages(repo: &Path) -> Vec<String> {
    let dir = repo.join(".code-repo-wiki").join("wiki").join("zh");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// 构造仓库 + 返回 ProjectRoot/config_path（每个测试独立临时目录）
fn setup(
    plan_yaml: Option<&str>,
) -> (
    std::path::PathBuf,
    code_repo_wiki::project::ProjectRoot,
    std::path::PathBuf,
) {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_plan_e2e_{}_{}",
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("创建临时仓库失败");
    build_repo(&repo, plan_yaml).expect("构造测试仓库失败");
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    (repo, root, config_path)
}

/// scope 覆盖（include 白名单）：只生成白名单内模块页，白名单外模块不被扫描
#[test]
fn test_plan_scope_include_filters_modules() {
    let plan = "knowledgecard:\n  scope:\n    include: [\"src/a/**\"]\n";
    let (repo, root, config_path) = setup(Some(plan));
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(
        pages.iter().any(|p| p == "src_a.md"),
        "白名单内模块 src/a 应生成独立页, 实际: {:?}",
        pages
    );
    assert!(
        !pages.iter().any(|p| p == "src_b.md"),
        "白名单外模块 src/b 不应生成独立页（未被扫描）, 实际: {:?}",
        pages
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// scope 覆盖（exclude 黑名单）：排除模块不被扫描，其余照常生成
#[test]
fn test_plan_scope_exclude_filters_modules() {
    let plan = "knowledgecard:\n  scope:\n    exclude: [\"src/b/**\"]\n";
    let (repo, root, config_path) = setup(Some(plan));
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(pages.iter().any(|p| p == "src_a.md"), "src/a 应生成, 实际: {:?}", pages);
    assert!(
        !pages.iter().any(|p| p == "src_b.md"),
        "被 exclude 的模块 src/b 不应生成, 实际: {:?}",
        pages
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// 顶层 scope 显式别名（兼容历史形态）：与 knowledgecard.scope 同效
#[test]
fn test_plan_scope_top_level_alias() {
    let plan = "scope:\n  include: [\"src/a/**\"]\n";
    let (repo, root, config_path) = setup(Some(plan));
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(pages.iter().any(|p| p == "src_a.md"));
    assert!(!pages.iter().any(|p| p == "src_b.md"));
    let _ = std::fs::remove_dir_all(&repo);
}

/// 无 plan 文件：默认行为零破坏（全部模块生成）
#[test]
fn test_plan_absent_default_generates_all() {
    let (repo, root, config_path) = setup(None);
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(
        pages.iter().any(|p| p == "src_a.md") && pages.iter().any(|p| p == "src_b.md"),
        "无 plan 应生成全部模块页, 实际: {:?}",
        pages
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// repowiki.documents 生成自定义页面：与自动模块页并存，parent 挂载 _toc
#[test]
fn test_plan_documents_generates_custom_pages() {
    let plan = r#"
repowiki:
  documents:
    - title: "接入指南"
      goal: "介绍如何集成本库"
      parent: "运维手册"
      hints: "突出快速开始"
"#;
    let (repo, root, config_path) = setup(Some(plan));
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(
        pages.iter().any(|p| p == "接入指南.md"),
        "自定义文档页应生成, 实际: {:?}",
        pages
    );
    // 自动模块页仍并存
    assert!(pages.iter().any(|p| p == "src_a.md"), "模块页仍应生成");
    // _toc 挂载：parent 非空 → 归入「## 运维手册」组
    let toc = std::fs::read_to_string(repo.join(".code-repo-wiki").join("_toc.md")).unwrap();
    assert!(
        toc.contains("## 运维手册"),
        "parent 应决定 _toc 挂载, 实际 _toc:\n{}",
        toc
    );
    assert!(toc.contains("接入指南"), "_toc 应列出自定义页");
    let _ = std::fs::remove_dir_all(&repo);
}

/// 坏 wiki_plan.yaml：解析失败显式报错终止（不静默忽略、不兜底）
#[test]
fn test_plan_bad_yaml_aborts_pipeline() {
    let (repo, root, config_path) = setup(Some("version: [坏"));
    let err = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .err()
    .expect("坏 wiki_plan.yaml 应使流水线报错");
    assert!(
        err.to_string().contains("解析 wiki_plan.yaml 失败"),
        "错误应含解析上下文: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&repo);
}
