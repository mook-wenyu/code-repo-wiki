#![cfg(test)]

//! 产物集合确定性快照评测（演进计划 T4.1）
//!
//! 两次全量生成（相同输入）的产物必须逐文件一致：
//! 1. 产物相对路径集合一致（排除 .state / 搜索索引 / _index.json 这类
//!    含时间戳的中间产物）
//! 2. 内容哈希一致（wiki 页归一化 "> 最后更新" 行——该行含秒级时间戳）
//! 3. AnalysisResult 结构等价（documents/cards 数量、模块名称排序）
//!
//! 反向验证：预写人工编辑页 → 第二次运行该文件哈希必须不同，
//! 证明断言工具本身能捕获差异（防测试恒真）。

use std::path::Path;

use code_repo_wiki::config::schema::WikiSection;
use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};

fn build_fixture_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src").join("a"))?;
    std::fs::create_dir_all(repo.join("src").join("b"))?;
    std::fs::write(
        repo.join("src").join("a").join("mod.rs"),
        "pub struct Alpha;\n\nimpl Alpha {\n    pub fn run(&self) -> u32 { 42 }\n}\n",
    )?;
    std::fs::write(
        repo.join("src").join("b").join("mod.rs"),
        "pub fn beta() -> &'static str { \"beta\" }\n",
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
    Ok(())
}

/// 递归收集产物相对路径集合（排除含时间戳/状态的中间产物）
fn artifact_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // .state 与 search 索引是运行时状态，不在确定性比较范围
                if p.file_name()
                    .map(|n| n == ".state" || n == ".search")
                    .unwrap_or(false)
                {
                    continue;
                }
                walk(&p, root, out);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string();
                // _index.json 含 generated_at 时间戳，排除
                if rel.ends_with("_index.json") {
                    continue;
                }
                out.push(rel);
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

/// 内容哈希：wiki 页归一化 "> 最后更新" 行（含秒级时间戳）后 SHA256
fn content_hash(path: &Path, is_wiki_page: bool) -> String {
    use sha2::{Digest, Sha256};
    let raw = std::fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let normalized = if is_wiki_page {
        text.lines()
            .map(|l| {
                if l.starts_with("> 最后更新") {
                    "> 最后更新: <timestamp>".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 两次全量生成产物逐文件一致 + 反向验证断言工具能捕获差异
#[test]
fn test_full_generate_artifact_set_deterministic() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_determinism_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_fixture_repo(&repo).expect("构造 fixture 失败");

    // root 显式注入替代进程级 cwd 切换
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    // 第一次全量生成
    let first = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("第一次生成失败");
    let out_dir = repo.join(".code-repo-wiki");
    let paths_a = artifact_paths(&out_dir);
    assert!(!paths_a.is_empty(), "产物集合不应为空");

    // 第一次生成后的内容哈希表（rel → hash）
    let hash_all = |paths: &[String]| -> std::collections::HashMap<String, String> {
        paths
            .iter()
            .map(|rel| {
                let p = out_dir.join(rel);
                // rel 在 Windows 上是反斜杠分隔，用 Path 组件判断目录
                let is_wiki = Path::new(rel).starts_with("wiki") && rel.ends_with(".md");
                (rel.clone(), content_hash(&p, is_wiki))
            })
            .collect()
    };
    let hashes_a = hash_all(&paths_a);

    // 人工修改一个 wiki 页（反向验证用：第二次运行该文件必须不同）
    let wiki_dir = out_dir.join("wiki").join("zh");
    let manual_target = std::fs::read_dir(&wiki_dir)
        .expect("wiki 目录应存在")
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().map(|x| x == "md").unwrap_or(false)
                && p.file_name()
                    .map(|n| n != "api.md" && n != "overview.md" && n != "architecture.md")
                    .unwrap_or(false)
        })
        .expect("应存在模块页");
    std::fs::write(&manual_target, "# 人工修改\n\n用户覆盖的内容\n").expect("写入人工修改失败");
    let manual_rel = manual_target
        .strip_prefix(&out_dir)
        .unwrap()
        .to_string_lossy()
        .to_string();
    // 人工修改的 wiki 页 → 同模块卡片会被反向同步注入"人工修改待同步"节
    //（预期功能：人工修改记录到卡片），因此该卡片也不在确定性比较范围
    let module_stem = manual_target
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let manual_card_rel = out_dir
        .join("cards")
        .join("zh")
        .join(format!("{module_stem}.md"))
        .strip_prefix(&out_dir)
        .unwrap()
        .to_string_lossy()
        .to_string();

    // 第二次全量生成（force=false 保留人工修改保护）
    let second = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("第二次生成失败");
    let paths_b = artifact_paths(&out_dir);
    let hashes_b = hash_all(&paths_b);

    // 1. 产物路径集合一致
    assert_eq!(paths_a, paths_b, "两次生成的产物路径集合必须一致");

    // 2. 内容哈希一致（人工修改页除外——受保护不被覆盖，哈希不同是预期）
    let mut differs = 0usize;
    for rel in &paths_a {
        let h1 = hashes_a.get(rel).expect("第一次哈希应存在");
        let h2 = hashes_b.get(rel).expect("第二次哈希应存在");
        if h1 != h2 {
            differs += 1;
        }
        if rel != &manual_rel && rel != &manual_card_rel {
            if h1 != h2 {
                eprintln!("DIFF: {rel}");
                let t1 = std::fs::read_to_string(out_dir.join(rel)).unwrap_or_default();
                eprintln!("---RUN1---\n{t1}");
            }
            assert_eq!(h1, h2, "产物内容必须确定性一致: {rel}");
        }
    }
    // 3. 结构等价：文档/卡片数量 + 模块名称排序一致
    assert_eq!(
        first.documents.len(),
        second.documents.len(),
        "文档数量应一致"
    );
    assert_eq!(first.cards.len(), second.cards.len(), "卡片数量应一致");
    let mut m1: Vec<String> = first.graph.modules.iter().map(|m| m.name.clone()).collect();
    let mut m2: Vec<String> = second
        .graph
        .modules
        .iter()
        .map(|m| m.name.clone())
        .collect();
    m1.sort();
    m2.sort();
    assert_eq!(m1, m2, "模块划分应确定性一致");

    // 4. 反向验证：人工修改页哈希必须不同（证明断言工具能捕获差异）
    assert_ne!(differs, 0, "反向验证失败：断言工具未捕获人工修改页的差异");
    // 人工修改页被保护不被覆盖，第二次生成后内容仍是用户写入的
    let manual_after = std::fs::read_to_string(&manual_target).unwrap_or_default();
    assert!(
        manual_after.contains("人工修改"),
        "人工修改页应保持用户内容"
    );
    // 且其哈希与第一次生成时的产物哈希不同（证明哈希表确实捕获了差异）
    let h_manual_first = hashes_a.get(&manual_rel).expect("人工页第一次哈希应存在");
    assert_ne!(
        content_hash(&manual_target, true),
        *h_manual_first,
        "人工修改后内容必须与首次生成不同"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
