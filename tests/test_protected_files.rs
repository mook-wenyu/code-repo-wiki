#![cfg(test)]

use std::collections::HashMap;

/// 验证 doc_fingerprints 与 protected_docs 默认空
#[test]
fn test_doc_fingerprints_default_empty() {
    use repo_wiki::incremental::state::GenerationState;
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        module_fingerprints: HashMap::new(),
        doc_fingerprints: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
    };
    assert!(state.doc_fingerprints.is_empty());
    assert!(state.protected_docs.is_empty());
}

/// 验证 record_doc_fingerprints 能正确记录文件指纹
#[test]
fn test_record_doc_fingerprints() {
    use repo_wiki::model::{WikiDocument, Reference, DocumentKind};
    use repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_doc_fp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 创建一个已存在的 wiki 页面（写盘路径与 render_all 一致：wiki/zh/test.md）
    std::fs::write(dir.join("wiki").join("zh").join("test.md"), "original content").unwrap();

    let doc = WikiDocument {
        title: "Test".into(),
        kind: DocumentKind::WikiPage,
        content: "irrelevant".into(),
        language: "zh".into(),
        module_path: vec!["test".into()],
        references: vec![Reference {
            target_title: "other".into(),
            target_path: "wiki/other.md".into(),
            relation: "depends_on".into(),
        }],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let languages = vec!["zh".to_string()];
    let fps = GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
    assert!(fps.contains_key(&dir.join("wiki").join("zh").join("test.md").to_string_lossy().to_string()));
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
        protected_docs: Vec::new(),
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

/// detect_manually_modified：指纹匹配 → 空；内容被改 → 命中
#[test]
fn test_detect_manually_modified() {
    use repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_detect_modified_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let file_path = dir.join("doc.md");
    std::fs::write(&file_path, "generated content").unwrap();

    let mut doc_fps = HashMap::new();
    doc_fps.insert(
        file_path.to_string_lossy().to_string(),
        GenerationState::compute_file_fingerprint(&file_path).unwrap(),
    );
    // 同时记录一个已被删除的文件（不应视为人工修改）
    let missing = dir.join("deleted.md").to_string_lossy().to_string();
    doc_fps.insert(missing, "deadbeef".into());

    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        module_fingerprints: HashMap::new(),
        doc_fingerprints: doc_fps,
        protected_docs: Vec::new(),
        generated_at: String::new(),
    };

    // 未修改 → 空
    assert!(state.detect_manually_modified().is_empty());

    // 人工修改 → 命中
    std::fs::write(&file_path, "user edited").unwrap();
    let modified = state.detect_manually_modified();
    assert_eq!(modified, vec![file_path.to_string_lossy().to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// render_all 传保护集：被保护文档不写盘，其余文档正常写盘
#[test]
fn test_render_all_protected_skips() {
    use repo_wiki::config::schema::{WikiConfig, OutputSection, WikiSection};
    use repo_wiki::model::{WikiDocument, DocumentKind, KnowledgeGraph};

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_render_protected_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "foo".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["foo".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let config = WikiConfig {
        output: OutputSection { dir: dir.to_string_lossy().to_string(), ..Default::default() },
        wiki: WikiSection { language: "zh".into(), ..Default::default() },
        ..Default::default()
    };

    let protected_path = dir.join("wiki").join("zh").join("foo.md").to_string_lossy().to_string();
    let protected: std::collections::HashSet<String> = [protected_path].into_iter().collect();

    repo_wiki::output::render_all(&[doc], &[], &KnowledgeGraph::default(), &config, &protected).unwrap();

    // 被保护文档不写盘
    assert!(!dir.join("wiki").join("zh").join("foo.md").exists());
    // 其余文档正常写盘
    assert!(dir.join("wiki").join("zh").join("api.md").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// schema 文档（title 含 / 与 :，module_path 为空）的指纹路径与 render_all 写盘路径一致
#[test]
fn test_schema_doc_fingerprint_path_matches_render_all() {
    use repo_wiki::config::schema::{WikiConfig, OutputSection, WikiSection};
    use repo_wiki::incremental::state::GenerationState;
    use repo_wiki::model::{WikiDocument, DocumentKind, KnowledgeGraph};

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_fp_schema_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "Database Schema: src/db.rs".into(),
        kind: DocumentKind::DatabaseSchema,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec![],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let config = WikiConfig {
        output: OutputSection { dir: dir.to_string_lossy().to_string(), ..Default::default() },
        wiki: WikiSection { language: "zh".into(), ..Default::default() },
        ..Default::default()
    };

    // 先渲染写盘，再记录指纹
    repo_wiki::output::render_all(std::slice::from_ref(&doc), &[], &KnowledgeGraph::default(), &config, &std::collections::HashSet::new()).unwrap();

    // 写盘路径：title 中 / 与 : 替换为 -（与 wiki_file_name 一致）
    let written = dir.join("wiki").join("zh").join("Database Schema- src-db.rs.md");
    assert!(written.exists());
    assert!(!dir.join("wiki").join("zh").join("Database Schema: src/db.rs.md").exists());

    let languages = vec!["zh".to_string()];
    let fps = GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
    assert!(fps.contains_key(&written.to_string_lossy().to_string()));
    // 指纹必须与磁盘实际内容一致
    assert_eq!(
        fps.get(&written.to_string_lossy().to_string()).unwrap(),
        &GenerationState::compute_file_fingerprint(&written).unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn test_doc_fingerprint_path_matches_render_all() {
    use repo_wiki::config::schema::{WikiConfig, OutputSection, WikiSection};
    use repo_wiki::incremental::state::GenerationState;
    use repo_wiki::model::{WikiDocument, DocumentKind, KnowledgeGraph};

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_fp_path_match_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "bar".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["bar".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let config = WikiConfig {
        output: OutputSection { dir: dir.to_string_lossy().to_string(), ..Default::default() },
        wiki: WikiSection { language: "zh".into(), ..Default::default() },
        ..Default::default()
    };

    // 先渲染写盘，再记录指纹
    repo_wiki::output::render_all(std::slice::from_ref(&doc), &[], &KnowledgeGraph::default(), &config, &std::collections::HashSet::new()).unwrap();

    let languages = vec!["zh".to_string()];
    let fps = GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
    let written = dir.join("wiki").join("zh").join("bar.md");
    assert!(fps.contains_key(&written.to_string_lossy().to_string()));
    // 指纹必须与磁盘实际内容一致
    assert_eq!(
        fps.get(&written.to_string_lossy().to_string()).unwrap(),
        &GenerationState::compute_file_fingerprint(&written).unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 人工修改反向同步：修改页面后 update，对应卡片出现修改记录
/// （官方语义：人工修改的 .md 不被覆盖，且反向同步到对应知识卡片）
#[test]
fn test_manual_edit_recorded_in_card() {
    use repo_wiki::model::KnowledgeCard;

    let dir = std::env::temp_dir()
        .join(format!("repo_wiki_test_manual_card_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 人工修改的页面（文件名主体与卡片 module.replace("::","_") 一致）
    let page = dir.join("wiki").join("zh").join("src_testmodule.md");
    std::fs::write(&page, "人工修改后的内容").unwrap();

    let mut card = KnowledgeCard {
        module_name: "src::testmodule".into(),
        module_type: "module".into(),
        summary: "摘要".into(),
        key_entities: vec![],
        dependencies: vec![],
        dependents: vec![],
        design_patterns: vec![],
        todo_notes: vec![],
        related_files: vec![],
        coding_spec: None,
        tech_stack: vec![],
        architecture: None,
        pending_manual_edits: vec![],
    };

    // 注入前：无记录时卡片渲染不含该节（避免空节）
    let before = repo_wiki::output::markdown::render_knowledge_card(&card);
    assert!(!before.contains("人工修改待同步"), "注入前不应渲染人工修改待同步节");

    // 增量管道对检测到的人工修改调用反向同步注入
    repo_wiki::inject_manual_edits(
        std::slice::from_mut(&mut card),
        &[page.to_string_lossy().to_string()],
    );

    assert_eq!(
        card.pending_manual_edits.len(),
        1,
        "对应卡片应出现人工修改记录"
    );
    assert!(
        card.pending_manual_edits[0].contains("src_testmodule.md"),
        "记录应含修改页路径: {}",
        card.pending_manual_edits[0]
    );
    assert!(
        card.pending_manual_edits[0].contains("人工修改后的内容"),
        "记录应含修改页内容摘要: {}",
        card.pending_manual_edits[0]
    );

    // 渲染层：注入后的卡片 markdown 包含"人工修改待同步"节与记录
    let rendered = repo_wiki::output::markdown::render_knowledge_card(&card);
    assert!(rendered.contains("## 人工修改待同步"), "卡片渲染应包含人工修改待同步节");
    assert!(rendered.contains("src_testmodule.md"), "卡片渲染应包含修改页路径");
    assert!(rendered.contains("人工修改后的内容"), "卡片渲染应包含内容摘要");

    let _ = std::fs::remove_dir_all(&dir);
}
