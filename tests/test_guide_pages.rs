#![cfg(test)]

//! v32 9.3 生成引导（[wiki.guide]）边界测试
//!
//! 覆盖 5 类边界：pages 过滤 / 空匹配显式报错 / priority 确定性排序 /
//! 白名单外既有页面保留（保护语义）/ 增量在白名单外变更时正常跳过。
//!
//! 全部使用 Mock LLM Provider（无网络），root 经 ProjectRoot 显式注入，
//! 无进程级 cwd 切换竞态（与 test_e2e.rs 同模式）。

use std::path::Path;

use code_repo_wiki::config::schema::{
    LlmProviderType, LlmSection, WikiConfig, WikiGuideSection, WikiSection,
};

/// 测试目录唯一序号（v19 教训：共享 std::process::id 目录会并发冲突——
/// 原子替换 tmp 文件互相删除；必须每测试独立目录）
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 构造临时仓库（src/a/mod.rs + src/b/mod.rs + config.toml，provider=mock，
/// guide=调用方指定；guide 为空=现行为零破坏基线）
fn build_fixture_repo(repo: &Path, guide: WikiGuideSection) -> anyhow::Result<()> {
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
            guide,
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

/// 构造仓库 + 返回 ProjectRoot/config_path（每个测试独立临时目录）
fn setup(guide: WikiGuideSection) -> (std::path::PathBuf, code_repo_wiki::project::ProjectRoot, std::path::PathBuf) {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_guide_{}_{}", std::process::id(), seq));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("创建临时仓库失败");
    build_fixture_repo(&repo, guide).expect("构造测试仓库失败");
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    (repo, root, config_path)
}

/// 边界 1：pages 白名单过滤——只生成匹配模块页，全局文档（overview/架构）
/// 仍全量汇总；白名单外模块不生成独立页
#[test]
fn test_guide_pages_filters_unmatched_modules() {
    let guide = WikiGuideSection {
        pages: vec!["src/a".into()],
        priority: vec![],
        notes: vec![],
    };
    let (repo, root, config_path) = setup(guide);
    let result = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    // 白名单内模块页生成
    let pages = list_wiki_pages(&repo);
    assert!(
        pages.iter().any(|p| p == "src_a.md"),
        "白名单内模块 src/a 应生成独立页, 实际: {:?}",
        pages
    );
    // 白名单外模块页不生成
    assert!(
        !pages.iter().any(|p| p == "src_b.md"),
        "白名单外模块 src/b 不应生成独立页, 实际: {:?}",
        pages
    );
    // 全局文档仍全量（overview 汇总不受过滤影响）
    assert!(
        pages.iter().any(|p| p == "overview.md"),
        "overview.md 应保留（全量汇总）, 实际: {:?}",
        pages
    );
    // 生成结果中不出现白名单外模块的文档
    assert!(
        result.documents.iter().all(|d| d.module_path != vec!["src".to_string(), "b".to_string()]),
        "生成结果不应包含 src/b 模块文档"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界 2：pages 全部未匹配 → 显式报错（不静默产出空模块页集合）
#[test]
fn test_guide_pages_empty_match_errors() {
    let guide = WikiGuideSection {
        pages: vec!["nonexistent/module".into()],
        priority: vec![],
        notes: vec![],
    };
    let (repo, root, config_path) = setup(guide);
    let err = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .err()
    .expect("空匹配应显式报错（结果应为 Err）");
    assert!(
        err.to_string().contains("未匹配任何模块"),
        "报错应说明未匹配模块, 实际: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界 3：priority 确定性排序——优先条目匹配的模块页生成顺序前置
/// （按 priority 列表顺序，未匹配保持默认顺序）
#[test]
fn test_guide_priority_orders_pages() {
    let guide = WikiGuideSection {
        pages: vec!["src/a".into(), "src/b".into()],
        priority: vec!["src/b".into(), "src/a".into()],
        notes: vec![],
    };
    let (repo, root, config_path) = setup(guide);
    let result = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    // 只取模块页（WikiPage），按生成顺序断言 b 在 a 前
    let module_pages: Vec<Vec<String>> = result
        .documents
        .iter()
        .filter(|d| d.kind == code_repo_wiki::model::DocumentKind::WikiPage)
        .map(|d| d.module_path.clone())
        .collect();
    let pos_b = module_pages
        .iter()
        .position(|p| p == &vec!["src".to_string(), "b".to_string()])
        .expect("src/b 模块页应存在");
    let pos_a = module_pages
        .iter()
        .position(|p| p == &vec!["src".to_string(), "a".to_string()])
        .expect("src/a 模块页应存在");
    assert!(
        pos_b < pos_a,
        "priority 指定 src/b 在前, 实际顺序: {:?}",
        module_pages
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界 4：白名单外既有页面保留——首次无 guide 全量生成 src_b.md，
/// 再次带 guide(pages=[src/a]) 全量生成后 src_b.md 不被清理（模块仍在
/// 扫描集中，preserved_modules 保留；过滤只约束「生成」不约束「删除」）
#[test]
fn test_guide_pages_keeps_existing_unmatched_pages() {
    // 第一轮：无 guide（现行为基线）
    let (repo, root, config_path) = setup(WikiGuideSection::default());
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("首轮全量生成失败");
    assert!(
        list_wiki_pages(&repo).iter().any(|p| p == "src_b.md"),
        "首轮应生成 src_b.md"
    );

    // 第二轮：带 pages 白名单（只生成 src/a）——src_b.md 应保留
    let config2 = WikiConfig {
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: WikiGuideSection {
                pages: vec!["src/a".into()],
                priority: vec![],
                notes: vec![],
            },
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(repo.join("config2.toml"), toml::to_string_pretty(&config2).expect("序列化 config2 失败"))
        .expect("写 config2 失败");
    code_repo_wiki::run_pipeline(
        Some(&repo.join("config2.toml")),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("二轮全量生成失败");

    let pages = list_wiki_pages(&repo);
    assert!(
        pages.iter().any(|p| p == "src_b.md"),
        "白名单外既有页面 src_b.md 应保留（清理语义=模块消失才删）, 实际: {:?}",
        pages
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界 5：增量在白名单外模块变更时正常跳过（不报错）——pages 白名单
/// 只约束「是否生成」，不影响增量影响传播判定；白名单外模块变更不触发
/// 生成也视为正常空集（strict_empty=false 路径）
#[test]
fn test_guide_incremental_skips_unmatched_without_error() {
    let guide = WikiGuideSection {
        pages: vec!["src/a".into()],
        priority: vec![],
        notes: vec![],
    };
    let (repo, root, config_path) = setup(guide);
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    // 白名单外模块 src/b 变更 → 增量更新：不报错
    std::fs::write(
        repo.join("src").join("b").join("mod.rs"),
        r#"
//! 模块 B（已修改）
pub fn beta() -> &'static str { "beta2" }
"#,
    )
    .expect("修改 b/mod.rs 失败");

    let inc = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![repo.join("src").join("b").join("mod.rs")],
            change_kind: None,
        },
    )
    .expect("增量更新不应因白名单外变更报错");
    // 增量不产生 src/b 的文档（白名单外）；也不产生 src/a 的文档（无变更）
    assert!(
        inc.documents.iter().all(|d| d.module_path != vec!["src".to_string(), "b".to_string()]),
        "增量不应生成白名单外模块 src/b 的文档"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
