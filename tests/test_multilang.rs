#![cfg(test)]

/// 验证 expand_languages 默认空
#[test]
fn test_single_lang_default() {
    let config = repo_wiki::config::schema::WikiConfig::default();
    assert!(config.wiki.expand_languages.is_empty());
}

/// 验证多语言配置能正常设置和序列化
#[test]
fn test_multi_lang_config_roundtrip() {
    let toml_str = r#"
[wiki]
template = "architecture"
language = "zh"
expand_languages = ["en", "ja"]

[scope]
include = ["src/**"]
exclude = []

[output]
dir = ".repo-wiki"
format = "markdown"

[llm]
provider = "openai"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"
max_concurrent = 4
"#;
    let config: repo_wiki::config::schema::WikiConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.wiki.language, "zh");
    assert_eq!(config.wiki.expand_languages, vec!["en", "ja"]);
}

/// 验证 write_document 支持语言参数（独立写盘到对应语言目录）
#[test]
fn test_write_document_language_param() {
    use repo_wiki::model::{KnowledgeCard, WikiDocument, Reference, DocumentKind};
    let doc = WikiDocument {
        title: "Test".into(),
        kind: DocumentKind::WikiPage,
        content: "content".into(),
        language: "zh".into(),
        module_path: vec!["crate".into(), "test".into()],
        references: vec![Reference {
            target_title: "other".into(),
            target_path: "wiki/zh/other.md".into(),
            relation: "depends_on".into(),
        }],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };
    let card = KnowledgeCard {
        module_name: "crate::test".into(),
        module_type: "module".into(),
        summary: "test".into(),
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
        features: Vec::new(),
    };

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_multilang_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 写入中文版
    repo_wiki::output::markdown::write_document(&doc, &[&card], &dir, "zh").unwrap();
    assert!(dir.join("wiki").join("zh").join("crate_test.md").exists());
    assert!(dir.join("cards").join("zh").join("crate_test.md").exists());

    // 写入英文版
    repo_wiki::output::markdown::write_document(&doc, &[&card], &dir, "en").unwrap();
    assert!(dir.join("wiki").join("en").join("crate_test.md").exists());
    assert!(dir.join("cards").join("en").join("crate_test.md").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 验证 render_all 按文档自身语言分组写入（多语言独立生成，不再按语言循环复制）
#[test]
fn test_render_all_multi_lang_dirs() {
    use repo_wiki::model::*;
    use repo_wiki::config::schema::*;

    let config = WikiConfig {
        wiki: WikiSection {
            language: "zh".into(),
            expand_languages: vec![
            ],
        },
        output: OutputSection::default(),
        ..Default::default()
    };
    let graph = KnowledgeGraph::default();
    let make_doc = |language: &str| WikiDocument {
        title: "Core".into(),
        kind: DocumentKind::WikiPage,
        content: "core content".into(),
        language: language.into(),
        module_path: vec!["core".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    };

    let dir = std::env::temp_dir().join(format!("repo_wiki_test_multilang_render_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 模拟 output.dir
    let multi_config = WikiConfig {
        output: OutputSection {
            dir: dir.to_string_lossy().to_string(),
        },
        ..config
    };

    // 中文版与英文版独立生成，各自写入自己的语言目录
    let docs = vec![make_doc("zh"), make_doc("en")];
    repo_wiki::output::render_all(&docs, &[], &graph, &multi_config, &std::collections::HashSet::new()).unwrap();
    assert!(dir.join("wiki").join("zh").join("core.md").exists());
    assert!(dir.join("wiki").join("en").join("core.md").exists());
    assert!(dir.join("cards").join("zh").exists());
    assert!(dir.join("cards").join("en").exists());

    // api.md 内容与语言无关，只写主语言一份（en 是 expand_languages 扩展语言）
    assert!(dir.join("wiki").join("zh").join("api.md").exists());
    assert!(!dir.join("wiki").join("en").join("api.md").exists());

    let _ = std::fs::remove_dir_all(&dir);
}
