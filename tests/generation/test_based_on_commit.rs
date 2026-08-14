//! v32 10.2：模块页「基于提交」基线行行为测试
//!
//! 契约：git 仓库中生成的 Wiki 页面头部含「> 基于提交: <8 位短哈希>」；
//! 非 git 仓库中该行完全省略（基线行是附加信息，不伪造）。

use std::fs;
use std::path::Path;

use crate::common::{mock_config, run_bin, unique_dir};

/// 构造临时仓库：src/a.rs + config.toml（mock LLM），可选 git init+提交
fn setup_repo(dir: &Path, with_git: bool) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src").join("a.rs"),
        "pub fn alpha_fn() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(dir.join("config.toml"), mock_config()).unwrap();
    if with_git {
        let repo = git2::Repository::init(dir).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("src/a.rs")).unwrap();
        index.add_path(Path::new("config.toml")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        // init 后 HEAD 是 symbolic ref（refs/heads/master 尚不存在），
        // 直接 commit(Some("HEAD")) 写 ref 会 Os NotFound——
        // 先取得提交 oid 再显式创建 ref 并 set_head
        let oid = repo.commit(None, &sig, &sig, "init", &tree, &[]).unwrap();
        repo.reference("refs/heads/master", oid, true, "init")
            .unwrap();
        repo.set_head("refs/heads/master").unwrap();
    }
}

#[test]
fn test_wiki_page_includes_based_on_commit_in_git_repo() {
    let dir = unique_dir("based_on_commit_git");
    setup_repo(&dir, true);
    let out = run_bin(&dir, &["generate"]);
    assert!(
        out.status.success(),
        "generate 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let page = fs::read_to_string(dir.join(".code-repo-wiki/wiki/zh/src.md")).unwrap();
    let lines: Vec<&str> = page.lines().filter(|l| l.contains("基于提交")).collect();
    assert_eq!(lines.len(), 1, "git 仓库页面应恰好一行基线: {page}");
    let hash = lines[0].trim().trim_start_matches("> 基于提交: ").trim();
    let line = lines[0].trim();
    assert_eq!(hash.len(), 8, "短哈希应为 8 位: {}", line);
    // 基线行位于「最后更新」行附近（头部）
    let page_lines: Vec<&str> = page.lines().collect();
    let idx = page_lines
        .iter()
        .position(|l| l.contains("基于提交"))
        .unwrap();
    assert!(idx < 8, "基线行应在页面头部（实际第 {} 行）", idx + 1);
}

#[test]
fn test_wiki_page_omits_based_on_commit_without_git() {
    let dir = unique_dir("based_on_commit_nogit");
    setup_repo(&dir, false);
    let out = run_bin(&dir, &["generate"]);
    assert!(
        out.status.success(),
        "generate 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let page = fs::read_to_string(dir.join(".code-repo-wiki/wiki/zh/src.md")).unwrap();
    assert!(
        !page.contains("基于提交"),
        "非 git 仓库不应有基线行: {page}"
    );
    // 页面本身仍正常生成（基线行缺失不影响内容）
    assert!(
        page.contains("最后更新"),
        "非 git 页面应正常生成（含最后更新行）: {page}"
    );
}
