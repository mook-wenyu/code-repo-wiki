#![cfg(test)]

use std::collections::HashMap;
use std::path::Path;

/// 验证 doc_fingerprints 默认空
#[test]
fn test_doc_fingerprints_default_empty() {
    use repo_wiki::incremental::state::GenerationState;
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        module_fingerprints: HashMap::new(),
        doc_fingerprints: HashMap::new(),
        generated_at: String::new(),
    };
    assert!(state.doc_fingerprints.is_empty());
}

/// 验证 record_doc_fingerprints 能正确记录文件指纹
#[test]
fn test_record_doc_fingerprints() {
    use repo_wiki::model::{WikiDocument, Reference, DocumentKind};
    use repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_doc_fp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki")).unwrap();

    // 创建一个已存在的 wiki 页面
    std::fs::write(dir.join("wiki").join("test.md"), "original content").unwrap();

    let doc = WikiDocument {
        title: "Test".into(),
        kind: DocumentKind::WikiPage,
        content: "irrelevant".into(),
        module_path: vec!["test".into()],
        references: vec![Reference {
            target_title: "other".into(),
            target_path: "wiki/other.md".into(),
            relation: "depends_on".into(),
        }],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let fps = GenerationState::record_doc_fingerprints(&[doc], &dir).unwrap();
    assert!(fps.contains_key(&dir.join("wiki").join("test.md").to_string_lossy().to_string()));
    assert_eq!(fps.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 验证手动修改文档后被检测到
#[test]
fn test_detect_manual_edit() {
    use repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_manual_edit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let file_path = dir.join("test.md");
    std::fs::write(&file_path, "original").unwrap();
    let original_fp = GenerationState::compute_file_fingerprint(&file_path).unwrap();

    let mut fps = HashMap::new();
    fps.insert(file_path.to_string_lossy().to_string(), original_fp);

    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: fps,  // is_file_changed 检查 file_fingerprints
        module_fingerprints: HashMap::new(),
        doc_fingerprints: HashMap::new(),
        generated_at: String::new(),
    };

    // 未修改 —— 不应标记为变更
    let is_changed = state.is_file_changed(&file_path).unwrap();
    assert!(!is_changed);

    // 手动修改
    std::fs::write(&file_path, "modified by user").unwrap();
    let is_changed = state.is_file_changed(&file_path).unwrap();
    assert!(is_changed);

    let _ = std::fs::remove_dir_all(&dir);
}
