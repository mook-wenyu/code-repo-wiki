#![cfg(test)]

//! 真实 Git 仓库的增量差分端到端测试（演进计划 T3.1）
//!
//! 背景：既有 test_e2e 的 fixture 不是 git 仓库，GitDiff 策略下
//! analyze_git_diff 失败 → 回退全量，其"只重写受影响页"断言从未真正
//! 走差分路径。本文件用 git init + 两次提交激活真实差分：
//!
//! - 场景 A（实现级变化）：函数体修改 → 仅本模块文档重生成，依赖方不动
//! - 场景 B（接口级变化）：签名修改 → api.md 反映新签名
//! - 场景 C（无变更）：增量跳过，documents 为空
//! - 场景 D（T2 传播闭环）：签名变更 → 调用方模块文档重生成（依赖传播接线验证）
//!
//! fixture 依赖关系：http 社区的 http_serve 调用 net 社区的 tcp_process
//! （跨社区 Calls 边）。

use std::path::Path;

use repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};
use repo_wiki::config::schema::{OutputSection, WikiSection};

/// 构造带跨社区调用的临时 git 仓库：
///
/// - src/net/{tcp.rs, udp.rs}：社区 net（tcp↔udp 互调）
/// - src/http/{server.rs, client.rs}：社区 http（server↔client 互调，
///   server 同时调用 net 的 tcp_process —— 单条跨社区边）
/// - 社区检测（CPM γ=0.5）：跨边 0.7 与内边 0.7 平衡，两社区保持独立，
///   使"签名变更 → 依赖方社区重生成"可在 e2e 层验证。
fn build_git_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src").join("net"))?;
    std::fs::create_dir_all(repo.join("src").join("http"))?;
    std::fs::write(
        repo.join("src").join("net").join("tcp.rs"),
        "pub fn tcp_process(x: u32) -> u32 { udp_process(x) + 1 }\n",
    )?;
    std::fs::write(
        repo.join("src").join("net").join("udp.rs"),
        "pub fn udp_process(x: u32) -> u32 { x + 2 }\n",
    )?;
    std::fs::write(
        repo.join("src").join("http").join("server.rs"),
        "pub fn http_serve(x: u32) -> u32 { client_render(x) + tcp_process(x) }\n",
    )?;
    std::fs::write(
        repo.join("src").join("http").join("client.rs"),
        "pub fn client_render(x: u32) -> u32 { x + 3 }\n",
    )?;

    let config = WikiConfig {
        output: OutputSection {
            dir: repo.join(".repo-wiki").to_string_lossy().into_owned(),
        },
        wiki: WikiSection {
            language: "zh".into(),
            ..Default::default()
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(repo.join("config.toml"), toml::to_string_pretty(&config)?)?;

    // 初始化 git 仓库（GitDiff 策略的前置条件；首次提交需显式签名）
    let git = git2::Repository::init(repo)?;
    let mut cfg = git.config()?;
    cfg.set_str("user.name", "test")?;
    cfg.set_str("user.email", "test@test.com")?;
    Ok(())
}

/// git2 提交当前工作区全部文件，返回 commit id
fn git_commit_all(repo: &Path, message: &str) -> String {
    let repo = git2::Repository::open(repo).expect("打开 git 仓库失败");
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let commit_id = match repo.head().ok() {
        Some(head) => {
            let parent = head.peel_to_commit().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent]).unwrap()
        }
        None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]).unwrap(),
    };
    commit_id.to_string()
}

/// 读取 api.md 全文（增量差分的内容级接缝：实体签名由 graph 渲染，body 变更不改变）
fn read_api(repo: &Path) -> String {
    std::fs::read_to_string(repo.join(".repo-wiki").join("wiki").join("zh").join("api.md"))
        .unwrap_or_default()
}

/// 增量后的文档标题集合（结构级差分接缝：增量 documents 只含重生成的模块）
fn doc_titles(result: &repo_wiki::AnalysisResult) -> Vec<String> {
    let mut titles: Vec<String> = result.documents.iter().map(|d| d.title.clone()).collect();
    titles.sort();
    titles
}

/// 全流程：全量生成 → 真实 git 提交 → 修改 → 增量 → 断言
#[test]
fn test_incremental_git_diff_scenarios() {

    let repo = std::env::temp_dir().join(format!("repo_wiki_git_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_git_repo(&repo).expect("构造 fixture 失败");

    // root 显式注入替代进程级 cwd 切换
    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let tcp_mod = repo.join("src").join("net").join("tcp.rs");

    // ---- 首次提交 + 全量生成（建立基线） ----
    git_commit_all(&repo, "init");
    let base = repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Full).expect("全量生成失败");
    let base_api = read_api(&repo);
    assert!(base_api.contains("tcp_process"), "基线 api.md 应含 tcp_process 签名");
    // 社区划分护栏：net 与 http 应检出为独立模块（依赖传播断言的前提）
    let module_names: Vec<&str> = base.graph.modules.iter().map(|m| m.name.as_str()).collect();
    assert!(
        module_names.iter().any(|n| n.contains("net")) && module_names.iter().any(|n| n.contains("http")),
        "net/http 应聚为两个独立社区，实际: {module_names:?}"
    );

    // ---- 场景 A：实现级变化（函数体修改，签名不变） ----
    std::fs::write(&tcp_mod, "pub fn tcp_process(x: u32) -> u32 { udp_process(x) + 42 }\n").unwrap();
    git_commit_all(&repo, "change body");
    let inc_a = repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None })
        .expect("增量生成失败");
    let titles_a = doc_titles(&inc_a);
    assert!(
        titles_a.iter().any(|t| t.contains("net")),
        "实现级变化应重生成 net 社区文档，实际: {titles_a:?}"
    );
    assert!(
        !titles_a.iter().any(|t| t.contains("http")),
        "实现级变化不应重生成依赖方 http 社区文档，实际: {titles_a:?}"
    );
    assert_eq!(
        read_api(&repo),
        base_api,
        "函数体修改不影响 api.md（签名未变）"
    );

    // ---- 场景 B/D：接口级变化（签名修改） + T2 传播闭环 ----
    // 签名变更 → api.md 反映新签名 + 调用方 http 社区文档被重生成（依赖传播接线）
    std::fs::write(&tcp_mod, "pub fn tcp_process(x: u32, y: u32) -> u32 { udp_process(x) + y }\n").unwrap();
    git_commit_all(&repo, "change signature");
    let inc_b = repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None })
        .expect("增量生成失败");
    let new_api = read_api(&repo);
    assert!(
        new_api.contains("tcp_process(x: u32, y: u32)") && !new_api.contains("tcp_process(x: u32)"),
        "api.md 应反映新签名，实际: {new_api}"
    );
    let titles_b = doc_titles(&inc_b);
    assert!(
        titles_b.iter().any(|t| t.contains("http")),
        "签名变更应传播到调用方 http 社区并重生成其文档（T2 闭环），实际: {titles_b:?}"
    );

    // ---- 场景 C：无变更 → 增量跳过 ----
    let inc_c = repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None })
        .expect("无变更增量失败");
    assert!(
        inc_c.documents.is_empty(),
        "无变更时 documents 应为空，实际: {:?}",
        doc_titles(&inc_c)
    );


    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 E（边界回归）：删除**孤立模块的唯一文件**（无依赖方，删除后
/// 影响传播不命中任何现存文件）→ changed_insights 为空 → 旧实现返回空
/// documents → cleanup 差集把**全部**旧产物误删（包括无关社区）。
/// 断言：删除 standalone.rs 后 net/http 社区产物必须保留。
#[test]
fn test_incremental_git_delete_isolated_file_keeps_others() {
    let repo = std::env::temp_dir().join(format!("repo_wiki_git_delisol_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_git_repo(&repo).expect("构造 fixture 失败");

    // 追加孤立文件：仓库根独立文件，无任何调用/导入关系
    let isolated = repo.join("standalone.rs");
    std::fs::write(&isolated, "pub fn isolated_helper() -> u32 { 7 }\n").unwrap();

    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    // ---- 首次提交 + 全量生成（基线） ----
    git_commit_all(&repo, "init with standalone");
    repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Full)
        .expect("全量生成失败");
    let wiki_dir = repo.join(".repo-wiki").join("wiki").join("zh");
    let pages_before: Vec<String> = std::fs::read_dir(&wiki_dir)
        .map(|es| {
            es.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        pages_before.iter().any(|p| p.contains("net") || p.contains("http")),
        "基线应含 net/http 社区页: {pages_before:?}"
    );

    // ---- 删除孤立文件 → 增量 ----
    std::fs::remove_file(&isolated).expect("删除 standalone.rs 失败");
    git_commit_all(&repo, "delete standalone");
    repo_wiki::run_pipeline(
        &config_path,
        None,
        false,
        &root,
        &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None },
    )
    .expect("删除增量失败");

    // ---- 无关社区产物必须保留 ----
    let pages_after: Vec<String> = std::fs::read_dir(&wiki_dir)
        .map(|es| {
            es.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        pages_after.iter().any(|p| p.contains("net") || p.contains("http")),
        "删除孤立文件后 net/http 社区产物不得被清空，实际: {pages_after:?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// U06/D11 防回归：force=true 的增量模式应退化为全量重生成——
/// 修改一个文件后 force 增量，documents 应含全部模块文档（而非只变更集）
#[test]
fn test_force_incremental_regenerates_all() {
    let repo = std::env::temp_dir().join(format!("repo_wiki_git_force_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_git_repo(&repo).expect("构造 fixture 失败");

    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let tcp_mod = repo.join("src").join("net").join("tcp.rs");

    // 首次提交 + 全量生成（基线）
    git_commit_all(&repo, "init");
    repo_wiki::run_pipeline(&config_path, None, false, &root, &repo_wiki::GenerationMode::Full)
        .expect("全量生成失败");

    // 修改一个文件 → force 增量（force=true）
    std::fs::write(&tcp_mod, "pub fn tcp_process(x: u32) -> u32 { udp_process(x) + 42 }\n").unwrap();
    git_commit_all(&repo, "change body");
    let inc = repo_wiki::run_pipeline(
        &config_path,
        None,
        true,
        &root,
        &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None },
    )
    .expect("force 增量失败");

    // force 退化全量：net 与 http 两个社区文档都应重生成
    let titles = doc_titles(&inc);
    assert!(
        titles.iter().any(|t| t.contains("net")) && titles.iter().any(|t| t.contains("http")),
        "force 增量应全量重生成（含未变更的 http 社区），实际: {titles:?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
