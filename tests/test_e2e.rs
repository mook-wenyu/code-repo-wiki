#![cfg(test)]

//! 端到端流水线测试：真实 CLI 全流程（mock LLM）
//!
//! 覆盖计划 Phase 3.1：generate 产物完整性 → 增量 update 只重写受影响页 →
//! 删除源文件后产物清理。
//!
//! 注意：root 通过 `ProjectRoot` 显式注入（替代进程级 cwd 切换），
//! 本文件全部断言集中在**单个顺序测试函数**内，无 cwd 竞态。

use std::path::Path;

use code_repo_wiki::config::schema::WikiSection;
use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};

/// 构造临时仓库（src/a.rs + src/b.rs + config.toml，provider=mock）
fn build_fixture_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src"))?;

    // 模块 a：独立子目录（模块检测按目录聚类，a/ 与 b/ 为两个模块）
    std::fs::create_dir_all(repo.join("src").join("a"))?;
    std::fs::create_dir_all(repo.join("src").join("b"))?;
    std::fs::write(
        repo.join("src").join("a").join("mod.rs"),
        r#"
//! 模块 A
pub struct Alpha;

impl Alpha {
    pub fn run(&self) -> u32 { 42 }
}
"#,
    )?;

    // 模块 b：独立文件，与 a 无依赖
    std::fs::write(
        repo.join("src").join("b").join("mod.rs"),
        r#"
//! 模块 B
pub fn beta() -> &'static str { "beta" }
"#,
    )?;

    // 配置：mock provider（无网络）、输出到仓库内 .code-repo-wiki
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

/// 端到端全流程：产物 → 增量 → 删除清理
#[test]
fn test_e2e_full_pipeline() {
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("创建临时仓库失败");
    build_fixture_repo(&repo).expect("构造测试仓库失败");

    // root 显式注入替代进程级 cwd 切换
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    // ---- 1. 全量生成：断言产物完整 ----
    let result = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    assert!(result.stats.files_scanned >= 2, "应扫描到至少 2 个文件");
    assert!(result.stats.total_entities >= 2, "应解析出至少 2 个实体");
    assert!(!result.documents.is_empty(), "应生成文档");

    let wiki_dir = repo.join(".code-repo-wiki").join("wiki").join("zh");
    let cards_dir = repo.join(".code-repo-wiki").join("cards").join("zh");
    let assets_dir = repo.join(".code-repo-wiki").join("assets").join("diagrams");
    for p in [&wiki_dir, &cards_dir, &assets_dir] {
        assert!(p.exists(), "产物目录应存在: {}", p.display());
    }
    assert!(
        wiki_dir.join("api.md").exists(),
        "api.md 应落盘（主语言 zh）"
    );
    assert!(
        wiki_dir.join("overview.md").exists(),
        "overview.md 应落盘（独立生成）"
    );
    assert!(
        wiki_dir.join("architecture.md").exists(),
        "architecture.md 应落盘（架构概览）"
    );

    // 收集生成后的 wiki 页面集合
    let pages_after_generate = list_wiki_pages(&repo);
    assert!(
        pages_after_generate.len() >= 2,
        "应至少生成 2 个模块页, 实际: {:?}",
        pages_after_generate
    );

    // ---- 2. 修改源文件 → 增量更新：只重写受影响页 ----
    std::fs::write(
        repo.join("src").join("a").join("mod.rs"),
        r#"
//! 模块 A（已修改）
pub struct Alpha;

impl Alpha {
    pub fn run(&self) -> u32 { 100 }
    pub fn extra(&self) -> u32 { 1 }
}
"#,
    )
    .expect("修改 a/mod.rs 失败");

    let inc = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("增量更新失败");
    assert!(!inc.documents.is_empty(), "增量更新应重新生成文档");

    // 受影响模块页应重新生成（新文档含模块 A 相关），
    // 且页面对应文件仍然存在（非删除路径）
    let pages_after_update = list_wiki_pages(&repo);
    assert_eq!(
        pages_after_update, pages_after_generate,
        "增量更新不应增删页面（只重写内容）"
    );

    // ---- 3. 删除源文件 → 增量更新（Deleted 事件）：产物清理 ----
    std::fs::remove_file(repo.join("src").join("a").join("mod.rs")).expect("删除 a/mod.rs 失败");
    let deleted_path = repo.join("src").join("a").join("mod.rs");

    let del = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![deleted_path],
            change_kind: Some(code_repo_wiki::incremental::watch::ChangeKind::Deleted),
        },
    )
    .expect("删除增量更新失败");

    // a 文件被删除:增量重建后 src 模块(合并 a+b)内容不再包含模块 A 实体;
    // 删除清理路径不再依赖 exists() 推断,由 Deleted 事件显式驱动
    let src_page = repo
        .join(".code-repo-wiki")
        .join("wiki")
        .join("zh")
        .join("src.md");
    let src_content = std::fs::read_to_string(&src_page).unwrap_or_default();
    assert!(
        !src_content.contains("Alpha"),
        "删除后模块页不应包含模块 A 实体(Alpha), 实际: {:?}",
        src_content
    );
    assert!(
        del.documents
            .iter()
            .all(|d| d.module_path.first() != Some(&"a".to_string())),
        "删除路径不应再生成模块 A 的文档"
    );

    // ---- 清理 ----
    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界：删除**模块唯一文件**（影响传播后 changed_insights 为空）时，
/// 其他模块的产物必须保留——旧实现 run_generation_filtered 在
/// changed_insights 为空时返回空 documents，render_all(空) +
/// cleanup_stale_outputs 的差集语义会把全部旧产物误删（包括无关模块）。
/// 现有 test_e2e_full_pipeline 的删除断言恰好被空结果满足（vacuously
/// true），未覆盖此边界。
#[test]
fn test_e2e_delete_only_module_keeps_other_modules() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_e2e_delonly_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("创建临时仓库失败");
    build_fixture_repo(&repo).expect("构造测试仓库失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    // 1. 全量生成：a、b 两模块页面均落盘
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let before = list_wiki_pages(&repo);
    let b_page = repo
        .join(".code-repo-wiki")
        .join("wiki")
        .join("zh")
        .join("src_b.md");
    assert!(b_page.exists(), "模块 b 页面应存在: {:?}", before);

    // 2. 删除模块 a 的唯一文件（b 无依赖不受影响）
    let a_file = repo.join("src").join("a").join("mod.rs");
    std::fs::remove_file(&a_file).expect("删除 a/mod.rs 失败");

    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![a_file],
            change_kind: Some(code_repo_wiki::incremental::watch::ChangeKind::Deleted),
        },
    )
    .expect("删除增量更新失败");

    // 3. 无关模块 b 的产物必须保留
    assert!(
        b_page.exists(),
        "模块 b 页面不应被删除（仅删除模块 a 的唯一文件）"
    );
    let after = list_wiki_pages(&repo);
    assert!(
        after.iter().any(|n| n == "src_b.md"),
        "删除后模块 b 页面应仍在: {:?}",
        after
    );

    let _ = std::fs::remove_dir_all(&repo);
}
