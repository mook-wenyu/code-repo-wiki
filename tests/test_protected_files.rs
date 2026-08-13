#![cfg(test)]

use std::collections::HashMap;

/// 验证 doc_fingerprints 与 protected_docs 默认空
#[test]
fn test_doc_fingerprints_default_empty() {
    use code_repo_wiki::incremental::state::GenerationState;
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints: HashMap::new(),
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
    };
    assert!(state.doc_fingerprints.is_empty());
    assert!(state.protected_docs.is_empty());
}

/// 验证 record_doc_fingerprints 能正确记录文件指纹
#[test]
fn test_record_doc_fingerprints() {
    use code_repo_wiki::incremental::state::GenerationState;
    use code_repo_wiki::model::{DocumentKind, Reference, WikiDocument};

    let dir =
        std::env::temp_dir().join(format!("code_repo_wiki_test_doc_fp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 创建一个已存在的 wiki 页面（写盘路径与 render_all 一致：wiki/zh/test.md）
    std::fs::write(
        dir.join("wiki").join("zh").join("test.md"),
        "original content",
    )
    .unwrap();

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
        based_on_commit: None,
        fingerprint: None,
    };

    let languages = vec!["zh".to_string()];
    let (fps, _modules) =
        GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
    assert!(
        fps.contains_key(
            &dir.join("wiki")
                .join("zh")
                .join("test.md")
                .to_string_lossy()
                .to_string()
        )
    );
    assert_eq!(fps.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 验证手动修改文档后被检测到
#[test]
fn test_detect_manual_edit() {
    use code_repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_manual_edit_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let file_path = dir.join("test.md");
    std::fs::write(&file_path, "original").unwrap();
    let original_fp = GenerationState::compute_file_fingerprint(&file_path).unwrap();

    let mut fps = HashMap::new();
    fps.insert(file_path.to_string_lossy().to_string(), original_fp);

    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: fps, // is_file_changed 检查 file_fingerprints
        doc_fingerprints: HashMap::new(),
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
    };

    // 未修改 —— 不应标记为变更
    let is_changed = state
        .is_file_changed(
            &code_repo_wiki::project::ProjectRoot::new(dir.clone()),
            &file_path,
        )
        .unwrap();
    assert!(!is_changed);

    // 手动修改
    std::fs::write(&file_path, "modified by user").unwrap();
    let is_changed = state
        .is_file_changed(
            &code_repo_wiki::project::ProjectRoot::new(dir.clone()),
            &file_path,
        )
        .unwrap();
    assert!(is_changed);

    let _ = std::fs::remove_dir_all(&dir);
}

/// detect_manually_modified：指纹匹配 → 空；内容被改 → 命中
#[test]
fn test_detect_manually_modified() {
    use code_repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_detect_modified_{}",
        std::process::id()
    ));
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
        doc_fingerprints: doc_fps,
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
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
    use code_repo_wiki::config::schema::{WikiConfig, WikiSection};
    use code_repo_wiki::model::{DocumentKind, KnowledgeGraph, WikiDocument};

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_render_protected_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "foo".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["foo".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        based_on_commit: None,
        fingerprint: None,
    };

    let config = WikiConfig {
        output_dir: Some((dir.to_string_lossy().to_string()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: Default::default(),
        },
        ..Default::default()
    };

    let protected_path = dir
        .join("wiki")
        .join("zh")
        .join("foo.md")
        .to_string_lossy()
        .to_string();
    let protected: std::collections::HashSet<String> = [protected_path].into_iter().collect();

    code_repo_wiki::output::render_all(
        &[doc],
        &[],
        &KnowledgeGraph::default(),
        &config,
        &protected,
    )
    .unwrap();

    // 被保护文档不写盘
    assert!(!dir.join("wiki").join("zh").join("foo.md").exists());
    // 其余文档正常写盘
    assert!(dir.join("wiki").join("zh").join("api.md").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 页面受人工修改保护时，关联卡片仍写盘（反向同步记录落盘的前提）：
/// 保护只跳过页面写盘，不连带跳过卡片的 pending_manual_edits 写入
#[test]
fn test_render_all_protected_page_still_writes_card() {
    use code_repo_wiki::config::schema::{WikiConfig, WikiSection};
    use code_repo_wiki::model::{DocumentKind, KnowledgeCard, KnowledgeGraph, WikiDocument};

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_protected_card_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "foo".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["foo".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        based_on_commit: None,
        fingerprint: None,
    };
    let card = KnowledgeCard {
        module_name: "foo".into(),
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
        design_rationale: None,
        pending_manual_edits: vec!["人工修改待同步: wiki/zh/foo.md 内容摘要: 用户改的".into()],
        features: Vec::new(),
    };

    let config = WikiConfig {
        output_dir: Some((dir.to_string_lossy().to_string()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: Default::default(),
        },
        ..Default::default()
    };

    let protected_path = dir
        .join("wiki")
        .join("zh")
        .join("foo.md")
        .to_string_lossy()
        .to_string();
    let protected: std::collections::HashSet<String> = [protected_path].into_iter().collect();

    code_repo_wiki::output::render_all(
        &[doc],
        &[card],
        &KnowledgeGraph::default(),
        &config,
        &protected,
    )
    .unwrap();

    // 页面被保护不写盘
    assert!(!dir.join("wiki").join("zh").join("foo.md").exists());
    // 关联卡片仍写盘且包含人工修改记录（反向同步落盘）
    let card_content =
        std::fs::read_to_string(dir.join("cards").join("zh").join("foo.md")).unwrap();
    assert!(
        card_content.contains("## 人工修改待同步"),
        "受保护页面的关联卡片应写盘并含记录"
    );
    assert!(
        card_content.contains("用户改的"),
        "卡片应包含人工修改内容摘要"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// schema 文档（title 含 / 与 :，module_path 为空）的指纹路径与 render_all 写盘路径一致
#[test]
fn test_schema_doc_fingerprint_path_matches_render_all() {
    use code_repo_wiki::config::schema::{WikiConfig, WikiSection};
    use code_repo_wiki::incremental::state::GenerationState;
    use code_repo_wiki::model::{DocumentKind, KnowledgeGraph, WikiDocument};

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_fp_schema_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "Database Schema: src/db.rs".into(),
        kind: DocumentKind::DatabaseSchema,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec![],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        based_on_commit: None,
        fingerprint: None,
    };

    let config = WikiConfig {
        output_dir: Some((dir.to_string_lossy().to_string()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: Default::default(),
        },
        ..Default::default()
    };

    // 先渲染写盘，再记录指纹
    code_repo_wiki::output::render_all(
        std::slice::from_ref(&doc),
        &[],
        &KnowledgeGraph::default(),
        &config,
        &std::collections::HashSet::new(),
    )
    .unwrap();

    // 写盘路径：title 中 / 与 : 替换为 -（与 wiki_file_name 一致）
    let written = dir
        .join("wiki")
        .join("zh")
        .join("Database Schema- src-db.rs.md");
    assert!(written.exists());
    assert!(
        !dir.join("wiki")
            .join("zh")
            .join("Database Schema: src/db.rs.md")
            .exists()
    );

    let languages = vec!["zh".to_string()];
    let (fps, _modules) =
        GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
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
    use code_repo_wiki::config::schema::{WikiConfig, WikiSection};
    use code_repo_wiki::incremental::state::GenerationState;
    use code_repo_wiki::model::{DocumentKind, KnowledgeGraph, WikiDocument};

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_fp_path_match_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let doc = WikiDocument {
        title: "bar".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["bar".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        based_on_commit: None,
        fingerprint: None,
    };

    let config = WikiConfig {
        output_dir: Some((dir.to_string_lossy().to_string()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: Default::default(),
        },
        ..Default::default()
    };

    // 先渲染写盘，再记录指纹
    code_repo_wiki::output::render_all(
        std::slice::from_ref(&doc),
        &[],
        &KnowledgeGraph::default(),
        &config,
        &std::collections::HashSet::new(),
    )
    .unwrap();

    let languages = vec!["zh".to_string()];
    let (fps, _modules) =
        GenerationState::record_doc_fingerprints(&[doc], &[], &dir, &languages).unwrap();
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
///
/// 链路：旧状态指纹比对（detect_manually_modified）+ 模块归属精确匹配
/// （doc_modules）→ collect_manual_edits 组装记录 → 生成前注入卡片。
#[test]
fn test_manual_edit_recorded_in_card() {
    use code_repo_wiki::incremental::state::GenerationState;
    use code_repo_wiki::model::KnowledgeCard;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_manual_card_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 人工修改的页面（模块归属 = src::testmodule）
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
        design_rationale: None,
        pending_manual_edits: vec![],
        features: Vec::new(),
    };

    // 注入前：无记录时卡片渲染不含该节（避免空节）
    let before = code_repo_wiki::output::markdown::render_knowledge_card(&card);
    assert!(
        !before.contains("人工修改待同步"),
        "注入前不应渲染人工修改待同步节"
    );

    // 构造旧状态：页面指纹与磁盘不一致（人工修改）+ 模块归属映射
    let page_str = page.to_string_lossy().to_string();
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints: {
            let mut m = HashMap::new();
            m.insert(page_str.clone(), "与磁盘内容不同的指纹".into());
            m
        },
        doc_modules: {
            let mut m = HashMap::new();
            m.insert(page_str.clone(), "src::testmodule".into());
            m
        },
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
    };

    // 增量管道：检测到的人工修改组装为模块级记录（精确匹配模块名）
    let edits = code_repo_wiki::collect_manual_edits(Some(&state));
    let notes = edits
        .get("src::testmodule")
        .expect("应命中 src::testmodule 的记录");
    assert_eq!(notes.len(), 1, "对应模块应出现一条人工修改记录");
    assert!(
        notes[0].contains("src_testmodule.md"),
        "记录应含修改页路径: {}",
        notes[0]
    );
    assert!(
        notes[0].contains("人工修改后的内容"),
        "记录应含修改页内容摘要: {}",
        notes[0]
    );

    // 生成前注入卡片（CardGenerator 合并恢复的旧记录与本次记录）
    card.pending_manual_edits = notes.clone();

    // 渲染层：注入后的卡片 markdown 包含"人工修改待同步"节与记录
    let rendered = code_repo_wiki::output::markdown::render_knowledge_card(&card);
    assert!(
        rendered.contains("## 人工修改待同步"),
        "卡片渲染应包含人工修改待同步节"
    );
    assert!(
        rendered.contains("src_testmodule.md"),
        "卡片渲染应包含修改页路径"
    );
    assert!(
        rendered.contains("人工修改后的内容"),
        "卡片渲染应包含内容摘要"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 无代码变更路径的反向同步：update 跳过生成时，人工修改记录直接落卡
/// （生成路径与磁盘直写路径两条腿，保证任意更新形态下反向同步不丢）
#[test]
fn test_manual_edit_synced_to_card_without_code_change() {
    use code_repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_manual_sync_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cards").join("zh")).unwrap();
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 预置卡片文件与人工修改的页面
    let card_path = dir.join("cards").join("zh").join("src_testmodule.md");
    std::fs::write(&card_path, "# src::testmodule\n\n## 摘要\n原有内容").unwrap();
    let page = dir.join("wiki").join("zh").join("src_testmodule.md");
    std::fs::write(&page, "人工修改后的内容").unwrap();

    // 构造旧状态：页面被人工修改（指纹不匹配）+ 模块归属
    let page_str = page.to_string_lossy().to_string();
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints: {
            let mut m = HashMap::new();
            m.insert(page_str.clone(), "与磁盘内容不同的指纹".into());
            m
        },
        doc_modules: {
            let mut m = HashMap::new();
            m.insert(page_str, "src::testmodule".into());
            m
        },
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
    };

    // 配置：卡片写盘路径（主语言 zh）与产物目录一致
    let config = code_repo_wiki::config::schema::WikiConfig {
        output_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };

    let synced = code_repo_wiki::sync_manual_edits_to_cards(&config, &state).unwrap();
    assert_eq!(synced, 1, "应同步一张卡片");

    // 卡片文件出现"人工修改待同步"节与记录
    let content = std::fs::read_to_string(&card_path).unwrap();
    assert!(
        content.contains("## 人工修改待同步"),
        "卡片应包含人工修改待同步节"
    );
    assert!(
        content.contains("src_testmodule.md"),
        "卡片应包含修改页路径"
    );
    assert!(content.contains("人工修改后的内容"), "卡片应包含内容摘要");

    // 幂等：再次同步不重复追加
    let synced2 = code_repo_wiki::sync_manual_edits_to_cards(&config, &state).unwrap();
    assert_eq!(synced2, 0, "记录已存在时不应重复同步");
    let content2 = std::fs::read_to_string(&card_path).unwrap();
    assert_eq!(content2, content, "幂等同步不应改变卡片内容");

    let _ = std::fs::remove_dir_all(&dir);
}

/// P3-4（A8 防回归）：反向同步目标卡片缺失（被删除）时显式告警并跳过，
/// 不得以空内容 + push_str 的方式凭空重建卡片（原实现 unwrap_or_default
/// 会把被删卡片"复活"成只含人工修改节的残片）
#[test]
fn test_manual_edit_sync_skips_missing_card() {
    use code_repo_wiki::incremental::state::GenerationState;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_manual_sync_missing_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("wiki").join("zh")).unwrap();

    // 人工修改的页面存在（指纹不匹配），但目标卡片文件不存在
    let page = dir.join("wiki").join("zh").join("src_testmodule.md");
    std::fs::write(&page, "人工修改后的内容").unwrap();

    let page_str = page.to_string_lossy().to_string();
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints: {
            let mut m = HashMap::new();
            m.insert(page_str.clone(), "与磁盘内容不同的指纹".into());
            m
        },
        doc_modules: {
            let mut m = HashMap::new();
            m.insert(page_str, "src::testmodule".into());
            m
        },
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
    };

    let config = code_repo_wiki::config::schema::WikiConfig {
        output_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };

    let synced = code_repo_wiki::sync_manual_edits_to_cards(&config, &state).unwrap();
    assert_eq!(synced, 0, "卡片缺失时应跳过，不得凭空创建");

    let card_path = dir.join("cards").join("zh").join("src_testmodule.md");
    assert!(
        !card_path.exists(),
        "被删卡片不应被反向同步重建（读失败告警 + 跳过）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
