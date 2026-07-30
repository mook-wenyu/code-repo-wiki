//! 搜索性能基准测试
//!
//! 运行: cargo test --test bench_search -- --nocapture
//! (benches/ 目录的 #[test] 函数不会被默认 cargo test 编译,
//!  需要通过 --test 指定或运行 cargo bench)

use std::time::Instant;

/// 500 条索引下 BM25 搜索平均耗时
#[test]
fn bench_text_search() {
    if std::env::var("CI").is_ok() {
        eprintln!("skip bench: CI");
        return;
    }

    let text_path = std::env::temp_dir().join(format!("bench_text_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&text_path);

    // 构建 500 条索引
    let mut engine = repo_wiki::search::text::TextEngine::open(&text_path).unwrap();
    for i in 0..500 {
        let node = repo_wiki::model::CodeNode {
            id: repo_wiki::model::NodeId::new(i),
            kind: repo_wiki::model::NodeKind::Function,
            name: format!("test_fn_{}", i),
            file_path: Some("src/lib.rs".into()),
            line_range: Some((i * 5, i * 5 + 10)),
            doc_comment: None,
            signature: Some(format!("fn test_fn_{}(arg: u32) -> bool", i)),
            module_path: vec!["crate".into(), "module".into()],
        };
        engine.index(&node, &format!("fn test_fn_{}(arg: u32) -> bool {{ arg > 0 }}", i)).unwrap();
    }

    // 预热
    let _ = engine.search("test_fn_250", 10);

    // 测量 10 次搜索平均耗时
    let start = Instant::now();
    let iterations = 10;
    for i in 0..iterations {
        let results = engine.search(&format!("test_fn_{}", i * 50), 10).unwrap();
        assert!(!results.is_empty());
    }
    let avg = start.elapsed() / iterations;
    eprintln!("bench_text_search: 500 entries, 10 searches, avg {avg:?} per search");
}

/// chunk_by_module 分组性能
#[test]
fn bench_chunking() {
    use repo_wiki::generate::chunk::Chunk;

    let graph = repo_wiki::model::KnowledgeGraph::new();
    let insights = vec![];
    let _chunks: Vec<Chunk> = repo_wiki::generate::chunk::chunk_by_module(&graph, &insights).unwrap();

    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        let _chunks: Vec<Chunk> = repo_wiki::generate::chunk::chunk_by_module(&graph, &insights).unwrap();
    }
    let avg = start.elapsed() / iterations;
    eprintln!("bench_chunking: {iterations} runs, avg {avg:?} per run (empty graph)");
}
