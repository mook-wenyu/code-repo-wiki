#![cfg(test)]

//! DEFECT-A 防回归：增量 update 后未重生成模块不得从 llms.txt/_toc.md 缺位
//!
//! 背景（已审计坐实）：`run_generation_filtered` 增量非空路径只把「受影响
//! 模块 + 全局文档」放进 GenerationOutput，未受影响仍存在的模块没有从导出
//! 快照回填 → llms.txt/_toc.md/export_snapshot.json/generation_state 全以
//! 部分集合为文档集。llms.txt 是全站地图（llmstxt.org v2），任何一次生成
//! 含增量都必须是全模块集合。修复（generate/mod.rs::backfill_unchanged_modules）
//! 使增量路径返回「完整当前文档集」。
//!
//! 本文件覆盖三条路径：
//! - 主场景：改 net 模块 2 文件 → llms.txt/_toc/导出快照/状态仍含未改动
//!   模块 http，且 http 磁盘页字节零改写（回填=旧文档幂等重写，非重生成）。
//! - 删除模块：删未改动模块 http 全部文件 → llms.txt 不再含它、磁盘页被
//!   清理（不复活已删模块）。
//! - 快照缺失：删 export_snapshot.json → 增量不崩溃，本次集合照常返回。
//!
//! fixture 依赖关系与 test_incremental_git_e2e 同构：src/net/{tcp.rs,udp.rs}
//! 社区 net + src/http/{server.rs,client.rs} 社区 http（server 调 net 的
//! tcp_process），社区检测确定性产出两个独立模块。

use std::collections::HashMap;
use std::path::Path;

use code_repo_wiki::config::schema::WikiSection;
use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};

/// 构造带跨社区调用的临时 git 仓库：net 与 http 两个独立社区
fn build_repo(repo: &Path) -> anyhow::Result<()> {
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

/// git2 提交当前工作区全部文件，返回 commit id（Windows libgit2 竞态有界重试）
fn git_commit_all(repo: &Path, message: &str) -> String {
    code_repo_wiki::test_git::commit_all(repo, message)
}

/// wiki/zh 目录全部 .md 页：文件名 → 内容（磁盘零改写断言的数据源）
fn wiki_pages_snapshot(repo: &Path) -> HashMap<String, String> {
    let wiki_dir = repo.join(".code-repo-wiki").join("wiki").join("zh");
    let mut map = HashMap::new();
    if let Ok(es) = std::fs::read_dir(&wiki_dir) {
        for e in es.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md")
                && let (Some(name), Ok(content)) = (
                    p.file_name().map(|s| s.to_string_lossy().into_owned()),
                    std::fs::read_to_string(&p),
                )
            {
                map.insert(name, content);
            }
        }
    }
    map
}

/// DEFECT-A 主场景：改 net 模块 udp.rs（函数体级变更，签名不变；http 与
/// udp_process 无 Calls 关系 → 仅 net 受影响重生成）→ 增量 update →
/// 未改动模块 http 必须仍出现在 llms.txt/_toc.md/export_snapshot.json/
/// generation_state 指纹，且 http 磁盘页字节零改写。
#[test]
fn test_incremental_unchanged_module_stays_in_sitemap() {
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_backfill_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_repo(&repo).expect("构造 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let output_dir = repo.join(".code-repo-wiki");

    // ---- 首次提交 + 全量生成（基线：llms.txt/_toc 均含两模块） ----
    git_commit_all(&repo, "init");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    let base_llms =
        std::fs::read_to_string(output_dir.join("llms.txt")).expect("基线 llms.txt 应存在");
    assert!(
        base_llms.contains("src::http") && base_llms.contains("src::net"),
        "基线 llms.txt 应含两模块，实际:\n{base_llms}"
    );
    assert!(
        base_pages.contains_key("src_http.md"),
        "基线应有 http 模块页"
    );

    // ---- 修改 net 模块 udp.rs（多行 body 使 line_end 变化 → BodyChanged；
    // http 不调用 udp_process，与变更函数无 Calls 关系 → http 保持未受影响） ----
    std::fs::write(
        repo.join("src").join("net").join("udp.rs"),
        "pub fn udp_process(x: u32) -> u32 {\n    x + 24\n}\n",
    )
    .unwrap();
    git_commit_all(&repo, "modify net udp");
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

    // 1. llms.txt 仍含未改动模块 http（DEFECT-A 直接断言面）与受影响模块 net
    let llms = std::fs::read_to_string(output_dir.join("llms.txt")).expect("llms.txt 应存在");
    assert!(
        llms.contains("src::http"),
        "llms.txt 必须保留未改动模块 http，实际:\n{llms}"
    );
    assert!(
        llms.contains("src::net"),
        "llms.txt 应含受影响模块 net，实际:\n{llms}"
    );

    // 2. _toc.md 仍含 http
    let toc = std::fs::read_to_string(output_dir.join("_toc.md")).expect("_toc.md 应存在");
    assert!(
        toc.contains("src::http"),
        "_toc.md 必须保留未改动模块 http，实际:\n{toc}"
    );

    // 3. export_snapshot.json 仍含 http（导出快照是 export --skip-generate 的契约）
    let snapshot_text =
        std::fs::read_to_string(output_dir.join(".state").join("export_snapshot.json"))
            .expect("导出快照应存在");
    let snapshot: serde_json::Value =
        serde_json::from_str(&snapshot_text).expect("导出快照应可解析");
    // 精确断言 documents/cards 数组（不可用原始文本——net 卡片的 dependents
    // 含 http 是合法数据，文本匹配会假阳性）
    let docs_have_http = snapshot["documents"].as_array().is_some_and(|arr| {
        arr.iter()
            .any(|d| d["title"].as_str().is_some_and(|t| t.contains("http")))
    });
    let cards_have_http = snapshot["cards"].as_array().is_some_and(|arr| {
        arr.iter().any(|c| {
            c["module_name"]
                .as_str()
                .is_some_and(|m| m.contains("http"))
        })
    });
    assert!(
        docs_have_http || cards_have_http,
        "导出快照必须含未改动模块 http（documents={docs_have_http}, cards={cards_have_http}）"
    );

    // 4. generation_state 指纹含 http 页（record_doc_fingerprints 消费完整文档集，
    //    下次人工修改检测/增量 diff 以完整集为基线）
    let state_text =
        std::fs::read_to_string(output_dir.join(".state").join("generation_state.json"))
            .expect("generation_state.json 应存在");
    assert!(
        state_text.contains("src_http.md"),
        "generation_state 指纹必须含 http 页，实际:\n{state_text}"
    );

    // 5. http 磁盘页字节零改写（回填=旧文档幂等重写，非重生成）
    let after_pages = wiki_pages_snapshot(&repo);
    assert_eq!(
        after_pages.get("src_http.md"),
        base_pages.get("src_http.md"),
        "未改动模块 http 页必须零改写（回填非重生成）"
    );

    // 6. 返回值 documents = 完整当前文档集（含未改动模块）
    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    assert!(
        titles.iter().any(|t| t.contains("http")),
        "documents 应含未改动模块 http，实际: {titles:?}"
    );
    assert!(
        titles.iter().any(|t| t.contains("net")),
        "documents 应含受影响模块 net，实际: {titles:?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 删除模块场景：删未改动模块 http 全部文件 → 增量 update → llms.txt 不再
/// 含 http、磁盘 http 页被清理、导出快照不再含 http（不复活已删模块）。
#[test]
fn test_incremental_delete_module_removes_from_sitemap() {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_backfill_del_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_repo(&repo).expect("构造 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let output_dir = repo.join(".code-repo-wiki");

    git_commit_all(&repo, "init");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let base_llms =
        std::fs::read_to_string(output_dir.join("llms.txt")).expect("基线 llms.txt 应存在");
    assert!(base_llms.contains("src::http"), "基线 llms.txt 应含 http");

    // 删 http 模块全部文件（server.rs + client.rs）
    std::fs::remove_file(repo.join("src").join("http").join("server.rs")).unwrap();
    std::fs::remove_file(repo.join("src").join("http").join("client.rs")).unwrap();
    git_commit_all(&repo, "delete http module");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("删除模块增量失败");

    // 1. llms.txt 不再含已删模块 http（不复活）
    let llms = std::fs::read_to_string(output_dir.join("llms.txt")).expect("llms.txt 应存在");
    assert!(
        !llms.contains("src::http"),
        "llms.txt 不得含已删模块 http，实际:\n{llms}"
    );

    // 2. 磁盘 http 页被清理（cleanup 差集语义）
    let pages = wiki_pages_snapshot(&repo);
    assert!(
        !pages.contains_key("src_http.md"),
        "已删模块 http 页应被清理"
    );

    // 3. 存活模块 net 页保留
    assert!(
        pages.contains_key("src_net.md"),
        "存活模块 net 页必须保留，实际: {:?}",
        pages.keys().collect::<Vec<_>>()
    );

    // 4. 导出快照不再含 http 文档/卡片（增量后快照 = 当前完整文档集；
    //    不可用原始文本匹配——net 卡片的 dependents 含 http 是合法数据）
    let snapshot_text =
        std::fs::read_to_string(output_dir.join(".state").join("export_snapshot.json"))
            .expect("导出快照应存在");
    let snapshot: serde_json::Value =
        serde_json::from_str(&snapshot_text).expect("导出快照应可解析");
    let doc_has_http = snapshot["documents"].as_array().is_some_and(|arr| {
        arr.iter()
            .any(|d| d["title"].as_str().is_some_and(|t| t.contains("http")))
    });
    let card_has_http = snapshot["cards"].as_array().is_some_and(|arr| {
        arr.iter().any(|c| {
            c["module_name"]
                .as_str()
                .is_some_and(|m| m.contains("http"))
        })
    });
    assert!(
        !doc_has_http && !card_has_http,
        "导出快照不得含已删模块 http（documents={doc_has_http}, cards={card_has_http}）"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 快照缺失场景：删 export_snapshot.json → 增量 update → 不崩溃，本次
/// 集合照常返回（未受影响模块回填跳过，不影响主流程）。
#[test]
fn test_incremental_snapshot_missing_does_not_crash() {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_backfill_nosnap_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_repo(&repo).expect("构造 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let output_dir = repo.join(".code-repo-wiki");

    git_commit_all(&repo, "init");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");

    // 删导出快照（模拟异常/损坏）
    std::fs::remove_file(output_dir.join(".state").join("export_snapshot.json")).unwrap();

    // 真实变更走非空增量路径 → backfill_unchanged_modules 遇快照缺失跳过
    std::fs::write(
        repo.join("src").join("net").join("tcp.rs"),
        "pub fn tcp_process(x: u32) -> u32 {\n    udp_process(x) + 42\n}\n",
    )
    .unwrap();
    git_commit_all(&repo, "modify after snapshot loss");
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
    .expect("快照缺失时增量不得崩溃");

    // 本次集合照常返回：至少含受影响模块 net（快照缺失不回填未改动模块，
    // 但主流程不阻断）
    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    assert!(
        titles.iter().any(|t| t.contains("net")),
        "快照缺失时本次集合应照常返回（含受影响模块 net），实际: {titles:?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
