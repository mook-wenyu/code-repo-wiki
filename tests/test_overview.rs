#![cfg(test)]

use std::collections::HashMap;

use code_repo_wiki::config::schema::{WikiConfig, WikiSection};
use code_repo_wiki::generate::llm::MockProvider;
use code_repo_wiki::generate::wiki::WikiGenerator;
use code_repo_wiki::generate::{GenerationOutput, GenerationStats};
use code_repo_wiki::incremental::state::GenerationState;
use code_repo_wiki::model::{DocumentKind, KnowledgeGraph, WikiDocument};
use code_repo_wiki::output::render_all;

fn make_config(dir: &std::path::Path) -> WikiConfig {
    WikiConfig {
        output_dir: Some((dir.to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: "zh".into(),
            guide: Default::default(),
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
        based_on_commit: None,
        fingerprint: None,
    }
}

/// 用 mock LLM 生成 overview 文档
fn generate_overview_doc(config: &WikiConfig) -> WikiDocument {
    let provider = MockProvider::new();
    let generator = WikiGenerator::new(&provider, 0);
    let graph = KnowledgeGraph::default();
    let output = GenerationOutput {
        cards: vec![],
        documents: vec![],
        generation_stats: GenerationStats::default(),
        timings: Default::default(),
    };
    // 临时根目录（产物输出目录由 config 控制，root 仅用于描述缓存指纹）
    let root = code_repo_wiki::project::ProjectRoot::new(std::env::temp_dir().join(format!(
        "code_repo_wiki_test_overview_root_{}",
        std::process::id()
    )));
    futures::executor::block_on(generator.generate_overview(&output, &graph, config, &root))
        .unwrap()
}

/// overview 内容来自 overview prompt（mock LLM 输出）而非第一个模块页
#[test]
fn test_overview_independent() {
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_overview_indep_{}",
        std::process::id()
    ));
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
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_overview_prot_{}",
        std::process::id()
    ));
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
    let (fps, _modules) = GenerationState::record_doc_fingerprints(
        &[module_doc.clone(), overview],
        &[],
        &dir,
        &languages,
    )
    .unwrap();
    assert!(fps.contains_key(&path_str), "overview 指纹应被记录");
    let mut doc_fingerprints = HashMap::new();
    doc_fingerprints.extend(fps);
    let state = GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        doc_fingerprints,
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: String::new(),
        tool_version: None,
        failed_modules: vec![],
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

/// 概览→模块引用防回归：references 的 target_path 必须与模块页写盘文件名
/// （module_path.join("_") 规则，markdown::wiki_file_name）一致，否则 crossref
/// validate 报断链（此前修复:概览用 replace("::","/") 生成 wiki/zh/src/analysis.md
/// 而写盘是 src_analysis.md,导致真实产物全部断链）
#[test]
fn test_overview_module_refs_match_write_path() {
    use code_repo_wiki::model::KnowledgeCard;

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_overview_refs_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let config = make_config(&dir);

    // 构造两个模块卡片(模块名含 ::,验证 target_path 用 _ 而非 / 分隔)
    let card1 = KnowledgeCard {
        module_name: "src::net".into(),
        module_type: "module".into(),
        summary: "net".into(),
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

    let card2 = KnowledgeCard {
        module_name: "src::http".into(),
        module_type: "module".into(),
        summary: "http".into(),
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

    let provider = MockProvider::new();
    let generator = WikiGenerator::new(&provider, 0);
    let graph = KnowledgeGraph::default();
    let output = GenerationOutput {
        cards: vec![card1, card2],
        documents: vec![],
        generation_stats: GenerationStats::default(),
        timings: Default::default(),
    };
    let overview = futures::executor::block_on(generator.generate_overview(
        &output,
        &graph,
        &config,
        &code_repo_wiki::project::ProjectRoot::new(std::env::temp_dir().join(format!(
            "code_repo_wiki_test_overview_root2_{}",
            std::process::id()
        ))),
    ))
    .unwrap();

    // 每个引用 target_path = wiki/zh/<module.replace("::","_")>.md(与写盘一致)
    assert_eq!(overview.references.len(), 2, "应生成 2 个模块引用");
    let paths: Vec<&str> = overview
        .references
        .iter()
        .map(|r| r.target_path.as_str())
        .collect();
    assert!(
        paths.contains(&"wiki/zh/src_net.md"),
        "src::net 引用应为 wiki/zh/src_net.md, 实际: {paths:?}"
    );
    assert!(
        paths.contains(&"wiki/zh/src_http.md"),
        "src::http 引用应为 wiki/zh/src_http.md, 实际: {paths:?}"
    );
    // 不得再出现 "/" 分隔的旧错误路径(wiki/zh/src/net.md)
    assert!(
        !paths.iter().any(|p| p.contains("src/net.md")),
        "不应出现 / 分隔的旧错误路径: {paths:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
