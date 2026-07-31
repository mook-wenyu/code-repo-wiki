//! 搜索与流水线性能基准测试
//!
//! 运行: cargo test --test bench_search -- --nocapture
//! (benches/ 目录的 #[test] 函数不会被默认 cargo test 编译,
//!  需要通过 --test 指定或运行 cargo bench)

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use repo_wiki::config::schema::{ScopeSection, WikiConfig};
use repo_wiki::ingest::parser::FileInsight;

/// 串行化依赖当前工作目录的基准（scan_and_parse 内部使用 current_dir）
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// 覆盖默认 scope 的配置：默认 include 只匹配 src/** 与 lib/**，基准仓库在临时目录根下
fn bench_config() -> WikiConfig {
    WikiConfig {
        scope: ScopeSection {
            include: vec![
                "**/*.rs".into(),
                "**/*.py".into(),
                "**/*.js".into(),
                "**/*.go".into(),
            ],
            exclude: vec![],
        },
        ..Default::default()
    }
}

/// 在临时目录构造 200 文件仓库（rust/python/js/go 各 50）并解析
///
/// 调用方需先 set_current_dir 到临时目录（scan_and_parse 以 cwd 为根）。
fn build_bench_repo(dir: &Path) -> Vec<FileInsight> {
    for i in 0..200 {
        let sub = dir.join(format!("mod{}", i % 10));
        std::fs::create_dir_all(&sub).unwrap();
        let (name, content) = match i % 4 {
            0 => (
                format!("f{i}.rs"),
                format!("pub fn f{i}(x: u32) -> u32 {{ x + {i} }}\n"),
            ),
            1 => (
                format!("f{i}.py"),
                format!("def f{i}(x):\n    return x + {i}\n"),
            ),
            2 => (
                format!("f{i}.js"),
                format!("export function f{i}(x) {{ return x + {i} }}\n"),
            ),
            _ => (
                format!("f{i}.go"),
                format!("package mod{}\n\nfunc F{i}(x int) int {{ return x + {i} }}\n", i % 10),
            ),
        };
        std::fs::write(sub.join(name), content).unwrap();
    }
    repo_wiki::ingest::scan_and_parse(&bench_config()).unwrap()
}

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

    let graph = repo_wiki::model::KnowledgeGraph::default();
    let insights = vec![];
    let _chunks: Vec<Chunk> = repo_wiki::generate::chunk::chunk_by_module(&insights, &graph.modules, &graph);

    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        let _chunks: Vec<Chunk> = repo_wiki::generate::chunk::chunk_by_module(&insights, &graph.modules, &graph);
    }
    let avg = start.elapsed() / iterations;
    eprintln!("bench_chunking: {iterations} runs, avg {avg:?} per run (empty graph)");
}

/// 200 文件临时仓库的扫描 + 解析耗时
#[test]
fn bench_scan_and_parse() {
    if std::env::var("CI").is_ok() {
        eprintln!("skip bench: CI");
        return;
    }
    let _guard = CWD_LOCK.lock().unwrap();

    let dir = std::env::temp_dir().join(format!("bench_repo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let start = Instant::now();
    let insights = build_bench_repo(&dir);
    let elapsed = start.elapsed();

    std::env::set_current_dir(old).unwrap();
    assert_eq!(insights.len(), 200);
    eprintln!("bench scan_and_parse: 200 files, {}ms", elapsed.as_millis());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 200 文件 insights 的图谱构建 + 模块检测耗时
#[test]
fn bench_graph_build() {
    if std::env::var("CI").is_ok() {
        eprintln!("skip bench: CI");
        return;
    }
    let _guard = CWD_LOCK.lock().unwrap();

    let dir = std::env::temp_dir().join(format!("bench_graph_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let insights = build_bench_repo(&dir);
    std::env::set_current_dir(old).unwrap();

    let start = Instant::now();
    let graph = repo_wiki::analysis::build_graph(&insights).unwrap();
    let modules = repo_wiki::analysis::detect_modules(&graph).unwrap();
    let elapsed = start.elapsed();

    // 200 个同构文件可能聚出任意数量模块，只断言图构建成功
    assert!(graph.graph.node_count() > 0);
    eprintln!("bench graph_build: 200 files, {} modules, {}ms", modules.len(), elapsed.as_millis());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 1000 实体 FTS5 批量索引耗时
#[test]
fn bench_index_batch() {
    if std::env::var("CI").is_ok() {
        eprintln!("skip bench: CI");
        return;
    }

    let text_path = std::env::temp_dir().join(format!("bench_index_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&text_path);
    let mut engine = repo_wiki::search::text::TextEngine::open(&text_path).unwrap();

    let items: Vec<(repo_wiki::model::CodeNode, String)> = (0..1000)
        .map(|i| {
            let node = repo_wiki::model::CodeNode {
                id: repo_wiki::model::NodeId::new(i),
                kind: repo_wiki::model::NodeKind::Function,
                name: format!("bench_fn_{}", i),
                file_path: Some("src/lib.rs".into()),
                line_range: Some((i * 5, i * 5 + 10)),
                doc_comment: None,
                signature: Some(format!("fn bench_fn_{}(arg: u32) -> bool", i)),
                module_path: vec!["crate".into(), "module".into()],
            };
            (node, format!("fn bench_fn_{}(arg: u32) -> bool {{ arg > 0 }}", i))
        })
        .collect();

    let start = Instant::now();
    engine.index_batch(&items).unwrap();
    let elapsed = start.elapsed();

    eprintln!("bench index_batch: 1000 entities, {}ms", elapsed.as_millis());
    let _ = std::fs::remove_file(&text_path);
}
