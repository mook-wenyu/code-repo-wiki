#![cfg(test)]

//! 项目级卡片（Spec / TechStack）流水线接入 e2e 测试（worker D）
//!
//! 覆盖：
//! 1. 全量 generate：含 Cargo.toml 时生成 cards/zh/project/tech-stack.md、
//!    _index.json 含 kind=tech-stack 条目、llms.txt 含 project/tech-stack.md、
//!    lint 无 error 级问题；无 AGENTS.md 时 Spec 卡不生成（输入驱动防幻觉）。
//! 2. 改 Cargo.toml（版本变化）→ 增量 update → tech-stack.md 刷新为新版本。
//! 3. 不动清单/规约 → 增量 update → 项目卡保留（回填成功，未丢失）。
//! 4. 删 Cargo.toml → 增量 update → tech-stack.md 被清理（差集语义）。
//!
//! Spec 卡的 LLM 生成路径由 project_card.rs 单元测试覆盖（C worker 域）；
//! 本文件的 mock provider 不产生合法 Spec JSON，故采用「无规约文件 → Spec
//! 卡不生成」的输入驱动防幻觉方向验证。清单/规约文件（Cargo.toml 等）不
//! 属于源码扩展名，不产 FileInsight——增量接入以 watch_paths 注入变更路径
//! （与 real-world `watch` 保存即触发同一机制）。

use std::collections::HashMap;
use std::path::Path;

use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig, WikiSection};

/// 构造临时 git 仓库：src/main.rs（使有模块页）+ Cargo.toml + 可选 AGENTS.md
fn build_repo(repo: &Path, src: &str, cargo_toml: &str, agents_md: Option<&str>) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src"))?;
    std::fs::write(repo.join("src").join("main.rs"), src)?;
    std::fs::write(repo.join("Cargo.toml"), cargo_toml)?;
    if let Some(a) = agents_md {
        std::fs::write(repo.join("AGENTS.md"), a)?;
    }

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

    let git = git2::Repository::init(repo)?;
    let mut cfg = git.config()?;
    cfg.set_str("user.name", "test")?;
    cfg.set_str("user.email", "test@test.com")?;
    Ok(())
}

/// git2 提交当前工作区全部文件（Windows libgit2 竞态有界重试，与既有测试同源）
fn git_commit_all(repo: &Path, message: &str) -> String {
    code_repo_wiki::test_git::commit_all(repo, message)
}

/// 全量生成辅助
fn full_generate(repo: &Path, root: &code_repo_wiki::project::ProjectRoot) {
    let config_path = repo.join("config.toml");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
}

/// 增量生成辅助（watch_paths 注入变更文件路径，模拟 watch 保存即触发）
fn incremental_generate(
    repo: &Path,
    root: &code_repo_wiki::project::ProjectRoot,
    watch_paths: Vec<std::path::PathBuf>,
) {
    let config_path = repo.join("config.toml");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths,
            change_kind: None,
        },
    )
    .expect("增量更新失败");
}

/// 读取 tech-stack.md 内容（不存在返回 None）
fn tech_stack_md(repo: &Path) -> Option<String> {
    std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join("cards")
            .join("zh")
            .join("project")
            .join("tech-stack.md"),
    )
    .ok()
}

/// 解析 _index.json 里的卡片条目（name → kind + path）
fn cards_index(repo: &Path) -> HashMap<String, (String, String)> {
    let index_text = std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join("cards")
            .join("zh")
            .join("_index.json"),
    )
    .expect("_index.json 应存在");
    let v: serde_json::Value = serde_json::from_str(&index_text).expect("_index.json 应可解析");
    v["cards"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    let name = c["name"].as_str().unwrap_or("").to_string();
                    let kind = c["kind"].as_str().unwrap_or("").to_string();
                    let path = c["path"].as_str().unwrap_or("").to_string();
                    (name, (kind, path))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 脚手架（每个测试独立临时目录），返回 (repo, root)
fn setup(repo_tag: &str, src: &str, cargo_toml: &str, agents_md: Option<&str>) -> (std::path::PathBuf, code_repo_wiki::project::ProjectRoot) {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_project_cards_{}_{}",
        std::process::id(),
        repo_tag
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("创建临时仓库失败");
    build_repo(&repo, src, cargo_toml, agents_md).expect("构造 fixture 失败");
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    (repo, root)
}

const MAIN_RS: &str = "pub fn main_fn() -> u32 { 42 }\n";

/// 场景 1：全量生成。含 Cargo.toml → tech-stack.md 生成 + _index.json
/// kind=tech-stack + llms.txt 含 project/tech-stack.md + lint 无 error；
/// 无 AGENTS.md → Spec 卡不生成（输入驱动防幻觉）。
#[test]
fn test_project_cards_full_generate() {
    let (repo, root) = setup(
        "full",
        MAIN_RS,
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        None,
    );
    git_commit_all(&repo, "init");
    full_generate(&repo, &root);

    let output = repo.join(".code-repo-wiki");
    // 1. tech-stack.md 落盘（project/ 子目录）
    let stack = tech_stack_md(&repo).expect("tech-stack.md 应生成");
    assert!(
        stack.contains("serde@1.0"),
        "tech-stack.md 应含 serde@1.0, 实际:\n{stack}"
    );
    // 2. Spec 卡不生成（无 AGENTS.md → 输入驱动防幻觉）
    assert!(
        !output
            .join("cards")
            .join("zh")
            .join("project")
            .join("spec.md")
            .exists(),
        "无 AGENTS.md 时 Spec 卡不应生成（防幻觉）"
    );
    // 3. _index.json 含 kind=tech-stack 条目，路径指向 project/ 子目录
    let index = cards_index(&repo);
    let tech = index
        .get("project_tech-stack")
        .unwrap_or_else(|| panic!("_index.json 应含 project_tech-stack 条目, 实际: {:?}", index));
    assert_eq!(tech.0, "tech-stack", "卡片 kind 应为 tech-stack");
    assert_eq!(
        tech.1, "cards/zh/project/tech-stack.md",
        "卡片相对路径应指向 project/ 子目录（正斜杠）"
    );
    // 4. llms.txt 含 project/tech-stack.md（项目卡站点地图条目）
    let llms = std::fs::read_to_string(output.join("llms.txt")).expect("llms.txt 应存在");
    assert!(
        llms.contains("cards/zh/project/tech-stack.md"),
        "llms.txt 应含项目卡路径, 实际:\n{llms}"
    );
    // 5. lint 无 error 级问题（Warning 可存在但 error 无）
    let issues = code_repo_wiki::output::lint::lint(&output, std::slice::from_ref(&repo));
    let errors: Vec<_> = issues.iter().filter(|i| !i.is_warning()).collect();
    assert!(
        errors.is_empty(),
        "lint 不应有 error 级问题, 实际: {:?}",
        errors
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 2：改 Cargo.toml（serde 1.0 → 2.0）→ 增量 update → tech-stack.md
/// 刷新为新版本（输入变更重生成）。
#[test]
fn test_project_cards_manifest_change_refreshes_techstack() {
    let (repo, root) = setup(
        "refresh",
        MAIN_RS,
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        None,
    );
    git_commit_all(&repo, "init");
    full_generate(&repo, &root);
    assert!(tech_stack_md(&repo).unwrap().contains("serde@1.0"));

    // 改 Cargo.toml：serde 版本提升
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"2.0\"\n",
    )
    .unwrap();
    git_commit_all(&repo, "bump serde to 2.0");
    // watch_paths 注入清单变更（清单不产 FileInsight，经 watch 事件进入变更集）
    incremental_generate(&repo, &root, vec![std::path::PathBuf::from("Cargo.toml")]);

    let stack = tech_stack_md(&repo).expect("tech-stack.md 应仍存在");
    assert!(
        stack.contains("serde@2.0"),
        "改 Cargo.toml 后 tech-stack.md 应刷新为新版本, 实际:\n{stack}"
    );
    assert!(
        !stack.contains("serde@1.0"),
        "tech-stack.md 不应残留旧版本 serde@1.0, 实际:\n{stack}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 3：不动清单/规约（仅改源码）→ 增量 update → 项目卡保留（回填，
/// 未丢失）。llms.txt 与导出快照仍含项目卡。
#[test]
fn test_project_cards_preserved_when_inputs_unchanged() {
    let (repo, root) = setup(
        "preserve",
        MAIN_RS,
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        None,
    );
    git_commit_all(&repo, "init");
    full_generate(&repo, &root);
    assert!(tech_stack_md(&repo).is_some());

    // 仅改源码（源码属于 FileInsight，经指纹比对进入变更集）
    std::fs::write(repo.join("src").join("main.rs"), "pub fn main_fn() -> u32 { 4242 }\n")
        .unwrap();
    git_commit_all(&repo, "change src only");
    incremental_generate(&repo, &root, vec![]);

    // 项目卡保留
    assert!(
        tech_stack_md(&repo).is_some(),
        "未改动清单/规约时 tech-stack 卡应保留（回填）"
    );
    // llms.txt 仍含项目卡
    let llms = std::fs::read_to_string(repo.join(".code-repo-wiki").join("llms.txt"))
        .expect("llms.txt 应存在");
    assert!(
        llms.contains("cards/zh/project/tech-stack.md"),
        "llms.txt 应仍含项目卡路径, 实际:\n{llms}"
    );
    // 导出快照仍含项目卡（export --skip-generate 契约）
    let snapshot_text = std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join(".state")
            .join("export_snapshot.json"),
    )
    .expect("导出快照应存在");
    assert!(
        snapshot_text.contains("tech-stack"),
        "导出快照应仍含项目卡, 实际:\n{snapshot_text}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 4：删 Cargo.toml → 增量 update → tech-stack.md 被清理（差集语义：
/// 项目卡输入缺失 → 重生成不产出 → 磁盘残留清除）。
#[test]
fn test_project_cards_delete_manifest_cleans_techstack() {
    let (repo, root) = setup(
        "delete",
        MAIN_RS,
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        None,
    );
    git_commit_all(&repo, "init");
    full_generate(&repo, &root);
    assert!(tech_stack_md(&repo).is_some());

    // 删 Cargo.toml（唯一清单 → 无依赖可生成 tech-stack）
    std::fs::remove_file(repo.join("Cargo.toml")).unwrap();
    git_commit_all(&repo, "delete Cargo.toml");
    incremental_generate(&repo, &root, vec![std::path::PathBuf::from("Cargo.toml")]);

    // tech-stack.md 磁盘残留清除
    assert!(
        !tech_stack_md(&repo).is_some(),
        "删 Cargo.toml 后 tech-stack.md 应被清理（差集语义）"
    );
    // _index.json 不再含 tech-stack 条目
    assert!(
        !cards_index(&repo).contains_key("project_tech-stack"),
        "_index.json 不应再含 project_tech-stack 条目"
    );
    // llms.txt 不再含项目卡路径
    let llms = std::fs::read_to_string(repo.join(".code-repo-wiki").join("llms.txt"))
        .expect("llms.txt 应存在");
    assert!(
        !llms.contains("cards/zh/project/tech-stack.md"),
        "llms.txt 不应再含项目卡路径, 实际:\n{llms}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 5：人工改项目卡 → 下次生成受保护跳过。
///
/// mock provider 不产 Spec 卡（无规约输入防幻觉），故以 tech-stack 卡验证
/// 保护语义：人工改写 tech-stack.md 后，下一次不涉及清单变更的增量生成
/// 不得覆盖人工内容（指纹保护，与模块卡同机制）。项目卡指纹经
/// state::record_doc_fingerprints 的 card_write_path 记录（project/ 子目录）。
#[test]
fn test_project_cards_manual_edit_protected() {
    let (repo, root) = setup(
        "protect",
        MAIN_RS,
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        None,
    );
    git_commit_all(&repo, "init");
    full_generate(&repo, &root);
    let stack_path = repo
        .join(".code-repo-wiki")
        .join("cards")
        .join("zh")
        .join("project")
        .join("tech-stack.md");
    assert!(stack_path.exists());

    // 人工改写项目卡（模拟 Agent 编辑内容）
    std::fs::write(&stack_path, "## 人工维护的技术栈注记\n").unwrap();

    // 仅改源码 → 增量 update（不涉及清单，项目卡回填复用人工内容应被保护）
    std::fs::write(repo.join("src").join("main.rs"), "pub fn main_fn() -> u32 { 777 }\n")
        .unwrap();
    git_commit_all(&repo, "change src only, do not touch manifest");
    incremental_generate(&repo, &root, vec![]);

    let content = std::fs::read_to_string(&stack_path).expect("tech-stack.md 应仍存在");
    assert!(
        content.contains("人工维护的技术栈注记"),
        "人工改写的项目卡应受保护不被自动覆盖, 实际:\n{content}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
