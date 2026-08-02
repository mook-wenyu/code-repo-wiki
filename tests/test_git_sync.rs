#![cfg(test)]

use std::collections::HashMap;
use std::path::Path;

use repo_wiki::incremental::state::GenerationState;

/// 构造带已落盘产物（wiki/zh/foo.md）的临时输出目录，返回 (目录, 产物路径字符串)
fn fixture(tag: &str, content: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("repo_wiki_sync_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("wiki").join("zh").join("foo.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    (dir, path.to_string_lossy().to_string())
}

fn save_state(dir: &Path, doc_fps: HashMap<String, String>, protected: Vec<String>) {
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints: doc_fps,
        doc_modules: HashMap::new(),
        protected_docs: protected,
        generated_at: String::new(),
    };
    state.save(&dir.join(".state")).unwrap();
}

fn load_state(dir: &Path) -> GenerationState {
    GenerationState::load(&dir.join(".state")).unwrap()
}

/// 修改产物 .md 后 sync：指纹更新为工作区当前内容，文件内容保留
#[test]
fn test_sync_updates_fingerprint() {
    let (dir, path_str) = fixture("update", "生成时内容");
    let mut fps = HashMap::new();
    fps.insert(
        path_str.clone(),
        GenerationState::compute_file_fingerprint(Path::new(&path_str)).unwrap(),
    );
    save_state(&dir, fps, vec![]);

    // 模拟 Git 目录中直接编辑产物
    std::fs::write(&path_str, "人工编辑后的内容").unwrap();

    repo_wiki::commands::sync_from_git(&dir).unwrap();

    let state = load_state(&dir);
    assert_eq!(
        state.doc_fingerprints.get(&path_str).unwrap(),
        &GenerationState::compute_file_fingerprint(Path::new(&path_str)).unwrap(),
        "sync 后指纹应更新为工作区当前内容"
    );
    assert_eq!(
        std::fs::read_to_string(&path_str).unwrap(),
        "人工编辑后的内容",
        "sync 不应改写工作区内容"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 受保护页面 sync 不改变指纹（保留人工版语义）
#[test]
fn test_sync_skips_protected() {
    let (dir, path_str) = fixture("protected", "生成时内容");
    let mut fps = HashMap::new();
    fps.insert(
        path_str.clone(),
        GenerationState::compute_file_fingerprint(Path::new(&path_str)).unwrap(),
    );
    let old_fp = fps.get(&path_str).unwrap().clone();
    save_state(&dir, fps, vec![path_str.clone()]);

    // 修改受保护页面后 sync
    std::fs::write(&path_str, "人工修改后的受保护内容").unwrap();

    repo_wiki::commands::sync_from_git(&dir).unwrap();

    let state = load_state(&dir);
    assert_eq!(
        state.doc_fingerprints.get(&path_str).unwrap(),
        &old_fp,
        "受保护页面的指纹不应被 sync 更新"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 无既有状态时 sync：所有产物视为新文件，指纹全部记录
#[test]
fn test_sync_without_state_records_new_fingerprints() {
    let (dir, path_str) = fixture("fresh", "全新内容");

    repo_wiki::commands::sync_from_git(&dir).unwrap();

    let state = load_state(&dir);
    assert_eq!(
        state.doc_fingerprints.get(&path_str).unwrap(),
        &GenerationState::compute_file_fingerprint(Path::new(&path_str)).unwrap(),
        "无状态时 sync 应记录产物指纹"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 状态文件存在但损坏（非法 JSON）时 sync 显式报错：
/// 静默重置会丢失 protected_docs，使人工修改保护失效
#[test]
fn test_sync_corrupted_state_errors_explicitly() {
    let (dir, _) = fixture("corrupt", "内容");
    let state_dir = dir.join(".state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("generation_state.json"), "{ not valid json !").unwrap();

    let err = repo_wiki::commands::sync_from_git(&dir).unwrap_err();
    assert!(
        err.to_string().contains("状态文件损坏"),
        "损坏状态应显式报错而非静默重置: {}",
        err
    );

    let _ = std::fs::remove_dir_all(&dir);
}
