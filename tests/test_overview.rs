#![cfg(test)]

use std::collections::HashMap;

use repo_wiki::config::schema::{OutputSection, WikiConfig, WikiSection};
use repo_wiki::generate::llm::MockProvider;
use repo_wiki::generate::wiki::WikiGenerator;
use repo_wiki::generate::{GenerationOutput, GenerationStats};
use repo_wiki::incremental::state::GenerationState;
use repo_wiki::model::{DocumentKind, KnowledgeGraph, WikiDocument};
use repo_wiki::output::render_all;

fn make_config(dir: &std::path::Path) -> WikiConfig {
    WikiConfig {
        output: OutputSection {
            dir: dir.to_string_lossy().into_owned(),
            ..Default::default()
        },
        wiki: WikiSection {
            language: "zh".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 第一个模块页（content 与 LLM mock 输出完全不同，用于区分来源）
fn make_module_doc() -> WikiDocument {
    WikiDocument {
        title: "FirstModule".into(),
        kind: DocumentKind::WikiPage,
        content: "第一个模块页的正文，与概览无关".into(),
        language: "zh".into(),
        module_path: vec!["first".into()],
        references: vec![],
        last_updated: "2025-01-01T00:00:00Z".into(),
        fingerprint: None,
    }
}

/// 用 mock LLM 生成 overview 文档
fn generate_overview_doc(config: &WikiConfig) -> WikiDocument {
    let provider = MockProvider::new();
    let generator = WikiGenerator::new(&provider, None);
    let graph = KnowledgeGraph::default();
    let output = GenerationOutput {
        cards: vec![],
        documents: vec![],
        generation_stats: GenerationStats::default(),
    };
    futures::executor::block_on(generator.generate_overview(&output, &graph, config)).unwrap()
}

/// overview 内容来自 overview prompt（mock LLM 输出）而非第一个模块页
#[test]
fn test_overview_independent() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_test_overview_indep_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = make_config(&dir);

    let overview = generate_overview_doc(&config);
    assert_eq!(overview.kind, DocumentKind::ProjectOverview);
    assert_eq!(overview.title, "项目概览");
    // mock LLM 返回固定文本"模拟摘要"，证明内容来自 LLM 调用而非模块页拼接
    assert!(overview.content.contains("模拟摘要"));

    let module_doc = make_module_doc();
    render_all(
        &[module_doc.clone(), overview.clone()],
        &[],
        &KnowledgeGraph::default(),
        &config,
        &std::collections::HashSet::new(),
    )
    .unwrap();

    // 写盘路径：wiki/zh/overview.md（ProjectOverview 特判）
    let written = dir.join("wiki").join("zh").join("overview.md");
    assert!(written.exists(), "overview 应写盘到 wiki/zh/overview.md");
    let content = std::fs::read_to_string(&written).unwrap();
    assert!(content.contains("模拟摘要"), "overview 内容应来自 LLM 输出");
    assert!(
        !content.contains("第一个模块页的正文"),
        "overview 不应是第一个模块页内容的拼接"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// overview 受 doc_fingerprints 保护：人工修改后 update（带保护集重渲染）不覆盖
#[test]
fn test_overview_protected() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_test_overview_prot_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = make_config(&dir);

    let module_doc = make_module_doc();
    let overview = generate_overview_doc(&config);
    let empty = std::collections::HashSet::new();
    render_all(
        &[module_doc.clone(), overview.clone()],
        &[],
        &KnowledgeGraph::default(),
        &config,
        &empty,
    )
    .unwrap();

    let overview_path = dir.join("wiki").join("zh").join("overview.md");
    let path_str = overview_path.to_string_lossy().to_string();

    // 记录生成指纹（与 update 流程一致：render_all 后 record_doc_fingerprints）
    let languages = vec!["zh".to_string()];
    let (fps, _modules) = GenerationState::record_doc_fingerprints(&[module_doc.clone(), overview], &[], &dir, &languages).unwrap();
    assert!(fps.contains_key(&path_str), "overview 指纹应被记录");
    let mut doc_fingerprints = HashMap::new();
    doc_fingerprints.extend(fps);
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        module_fingerprints: HashMap::new(),
        doc_fingerprints,
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
    };

    // 人工修改 overview
    std::fs::write(&overview_path, "人工编辑的概览内容").unwrap();
    let modified = state.detect_manually_modified();
    assert!(modified.contains(&path_str), "人工修改应被检测到");

    // update 带保护集重渲染：不覆盖人工版
    let protected: std::collections::HashSet<String> = [path_str].into_iter().collect();
    render_all(
        &[module_doc, generate_overview_doc(&config)],
        &[],
        &KnowledgeGraph::default(),
        &config,
        &protected,
    )
    .unwrap();
    let kept = std::fs::read_to_string(&overview_path).unwrap();
    assert_eq!(kept, "人工编辑的概览内容", "受保护的 overview 不应被覆盖");

    let _ = std::fs::remove_dir_all(&dir);
}
