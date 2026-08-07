//! 集成测试：验证完整 pipeline 端到端正确性（不依赖 LLM API）
//!
//! 使用 tests/fixtures/sample-repo/ 作为输入，
//! 验证扫描、图构建、搜索索引、增量删除等核心路径。

use std::path::Path;

use repo_wiki::config::schema::{ScopeSection, WikiConfig};
use repo_wiki::ingest::parser::ParserRegistry;
use repo_wiki::ingest::scanner::Scanner;
use repo_wiki::search::text::TextEngine;

/// fixture 仓库的 src 目录路径
fn fixture_src() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo")
        .join("src")
}

/// 构建仅覆盖 fixture src/ 的配置（不触发 LLM）
fn fixture_config() -> WikiConfig {
    WikiConfig {
        scope: ScopeSection {
            include: vec!["**/*.rs".to_string()],
            exclude: vec![],
        },
                ..Default::default()
    }
}

/// 扫描并解析 fixture 仓库，返回 FileInsight 列表
fn scan_fixture() -> Vec<repo_wiki::ingest::parser::FileInsight> {
    let root = fixture_src();
    let config = fixture_config();
    let scanner = Scanner::new(&root, &config.scope).unwrap();
    let files = scanner.scan().expect("扫描 fixture 目录失败");

    let registry = ParserRegistry::new();
    let mut insights = Vec::new();
    for file in &files {
        if let Some(processor) = registry.get_for_file(file)
            && let Ok(source) = std::fs::read_to_string(file)
            && let Ok(insight) = processor.parse(&source, file)
        {
            insights.push(insight);
        }
    }
    insights
}

// ==================== 测试用例 ====================

#[test]
fn test_scan_and_parse_fixture() {
    let insights = scan_fixture();

    // fixture 有 3 个 .rs 文件
    assert_eq!(insights.len(), 3, "应扫描到 3 个 Rust 文件");

    // 验证每个文件都有实体
    for insight in &insights {
        assert!(
            !insight.entities.is_empty(),
            "文件 {} 应包含至少一个实体",
            insight.path.display()
        );
    }

    // 验证已知实体存在
    let all_names: Vec<&str> = insights
        .iter()
        .flat_map(|i| i.entities.iter().map(|e| e.name.as_str()))
        .collect();
    assert!(all_names.contains(&"authenticate"), "应包含 authenticate 函数");
    assert!(all_names.contains(&"User"), "应包含 User 结构体");
    assert!(all_names.contains(&"SessionStore"), "应包含 SessionStore 结构体");
    assert!(all_names.contains(&"save_session"), "应包含 save_session 函数");
}

#[test]
fn test_build_graph_fixture() {
    let insights = scan_fixture();
    let graph = repo_wiki::analysis::build_graph(&insights).expect("构建图失败");

    // 图应包含节点（文件节点 + 实体节点）
    assert!(
        graph.graph.node_count() >= 10,
        "图应至少包含 10 个节点（3 文件 + 7+ 实体），实际: {}",
        graph.graph.node_count()
    );

    // 图应包含边（Contains 边）
    assert!(
        graph.graph.edge_count() >= 3,
        "图应至少包含 3 条边，实际: {}",
        graph.graph.edge_count()
    );
}

#[test]
fn test_search_index_build_and_query() {
    let insights = scan_fixture();
    let graph = repo_wiki::analysis::build_graph(&insights).expect("构建图失败");

    // 在临时目录中构建索引
    let tmp_dir = std::env::temp_dir().join(format!("repo_wiki_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("创建临时目录失败");
    let index_path = tmp_dir.join("text_index.bin");

    let items: Vec<(repo_wiki::model::CodeNode, String)> = graph
        .graph
        .node_indices()
        .filter_map(|idx| {
            let node = graph.graph.node_weight(idx)?;
            if matches!(
                node.kind,
                repo_wiki::model::NodeKind::Project
                    | repo_wiki::model::NodeKind::Module
                    | repo_wiki::model::NodeKind::File
            ) {
                return None;
            }
            let source = node
                .signature
                .clone()
                .unwrap_or_else(|| node.name.clone());
            Some((node.clone(), source))
        })
        .collect();

    let mut engine = TextEngine::open(&index_path).expect("打开索引失败");
    engine.index_batch(&items).expect("批量索引失败");

    // 验证索引非空
    assert!(engine.doc_count() > 0, "索引应包含文档");

    // 搜索已知符号
    let results = engine.search("authenticate", 5).expect("搜索失败");
    assert!(
        !results.is_empty(),
        "搜索 'authenticate' 应返回结果"
    );
    assert!(
        results[0].0.name.contains("authenticate"),
        "首个结果应为 authenticate"
    );

    // 搜索另一个符号
    let results2 = engine.search("SessionStore", 5).expect("搜索失败");
    assert!(!results2.is_empty(), "搜索 'SessionStore' 应返回结果");

    // 清理
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_incremental_remove_by_file() {
    let tmp_dir = std::env::temp_dir().join(format!("repo_wiki_incr_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("创建临时目录失败");
    let index_path = tmp_dir.join("text_index.bin");

    let mut engine = TextEngine::open(&index_path).expect("打开索引失败");

    // 索引两个不同文件的实体（使用完全不同的名称避免 token 重叠）
    let node_a = repo_wiki::model::CodeNode {
        id: repo_wiki::model::NodeId::new(0),
        kind: repo_wiki::model::NodeKind::Function,
        name: "alpha_handler".into(),
        file_path: Some("src/alpha.rs".into()),
        line_range: Some((1, 5)),
        doc_comment: None,
        signature: Some("fn alpha_handler()".into()), visibility: None,
        module_path: vec![],
    };
    let node_b = repo_wiki::model::CodeNode {
        id: repo_wiki::model::NodeId::new(1),
        kind: repo_wiki::model::NodeKind::Function,
        name: "beta_processor".into(),
        file_path: Some("src/beta.rs".into()),
        line_range: Some((1, 3)),
        doc_comment: None,
        signature: Some("fn beta_processor()".into()), visibility: None,
        module_path: vec![],
    };

    engine
        .index_batch(&[(node_a, "fn alpha_handler()".into()), (node_b, "fn beta_processor()".into())])
        .expect("索引失败");
    assert_eq!(engine.doc_count(), 2);

    // 删除 src/alpha.rs 的条目
    let removed = engine.remove_by_file("src/alpha.rs").expect("删除失败");
    assert_eq!(removed, 1, "应删除 1 条");
    assert_eq!(engine.doc_count(), 1, "剩余 1 条");

    // 搜索 alpha_handler 应无结果
    let results = engine.search("alpha_handler", 5).expect("搜索失败");
    assert!(results.is_empty(), "删除后搜索 alpha_handler 应无结果");

    // 搜索 beta_processor 仍有结果
    let results = engine.search("beta_processor", 5).expect("搜索失败");
    assert!(!results.is_empty(), "beta_processor 应仍存在");

    // 清理
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_config_roundtrip() {
    let config = WikiConfig::default();
    let toml_str = toml::to_string_pretty(&config).expect("序列化失败");
    let parsed: WikiConfig = toml::from_str(&toml_str).expect("反序列化失败");

    // v17 t05：schema 默认值统一到模板阵营（DeepSeek）
    assert_eq!(parsed.llm.model, "deepseek-v4-flash");
    assert_eq!(parsed.llm.api_key_env, "OPENCODEGO2_API_KEY");
    assert_eq!(parsed.wiki.language, "zh");
    assert_eq!(parsed.output_dir(), std::path::Path::new(".repo-wiki"));
        assert!(!parsed.embed.model.is_empty());
}
