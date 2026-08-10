//! 搜索与流水线性能基准测试
//!
//! 运行: cargo test --bench bench_search -- --nocapture
//! (benches/ 目录默认不被 cargo test 编译；本文件在 Cargo.toml 中声明为
//!  [[bench]] harness = true 目标，故可通过 --bench 指定运行，或直接 cargo bench)

use std::path::Path;

use std::time::Instant;

use code_repo_wiki::ingest::parser::FileInsight;

/// bench 专用 MockEmbedder：按实体名前缀 f{m}_/g{m}_ 解析簇号，返回
/// 20 维 one-hot 向量（第 m 位 1.0）——同簇实体余弦=1（权重 0.75 ≥ γ=0.4
/// 成簇），跨簇余弦=0（权重 0.25 不合并）。
///
/// v52 T11 适配：单例过滤后纯结构聚类（Jaccard=0 → 权重 0.25 < 0.4）不再
/// 产生特征，注入 semantic 通道恢复 bench 的"跨簇调用应产生特征"语义
/// （与 tests/test_clustering.rs 的 MockEmbedder 同构）。
struct ClusterEmbedder;

impl code_repo_wiki::analysis::feature::Embedder for ClusterEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        // 输入形如 "f0_0 Function ..." / "g3_1 Function ..."：取前缀 f/g 后的簇号
        let mut cluster = 0usize;
        if let Some(rest) = text
            .strip_prefix('f')
            .or_else(|| text.strip_prefix('g'))
        {
            if let Some(digits) = rest.split('_').next() {
                cluster = digits.parse::<usize>().unwrap_or(0);
            }
        }
        let mut vec = vec![0.0f32; 20];
        if cluster < 20 {
            vec[cluster] = 1.0;
        }
        Ok(vec)
    }
    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
        let dot: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
        let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
    }
}

/// 在临时目录构造 200 文件仓库（rust/python/js/go 各 50）并解析
///
/// 调用方以 ProjectRoot::new(dir) 注入根（N8 清理：此前用 set_current_dir
/// 加 CWD_LOCK 依赖进程 cwd——scan_and_parse_at 已 root 参数化，cwd 依赖
/// 是历史残留，且 set_current_dir 与并行基准互相干扰）。
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
    code_repo_wiki::ingest::scan_and_parse_at(&code_repo_wiki::project::ProjectRoot::new(dir.to_path_buf())).unwrap().insights
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
    let mut engine = code_repo_wiki::search::text::TextEngine::open(&text_path).unwrap().0;
    for i in 0..500 {
        let node = code_repo_wiki::model::CodeNode {
            id: code_repo_wiki::model::NodeId::new(i),
            kind: code_repo_wiki::model::NodeKind::Function,
            name: format!("test_fn_{}", i),
            file_path: Some("src/lib.rs".into()),
            line_range: Some((i * 5, i * 5 + 10)),
            doc_comment: None,
            signature: Some(format!("fn test_fn_{}(arg: u32) -> bool", i)), visibility: None,
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
    use code_repo_wiki::generate::chunk::Chunk;

    let graph = code_repo_wiki::model::KnowledgeGraph::default();
    let insights = vec![];
    let _chunks: Vec<Chunk> = code_repo_wiki::generate::chunk::chunk_by_module(&insights, &graph.modules, &graph);

    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        let _chunks: Vec<Chunk> = code_repo_wiki::generate::chunk::chunk_by_module(&insights, &graph.modules, &graph);
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


    let dir = std::env::temp_dir().join(format!("bench_repo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let start = Instant::now();
    let insights = build_bench_repo(&dir);
    let elapsed = start.elapsed();

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

    let dir = std::env::temp_dir().join(format!("bench_graph_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let insights = build_bench_repo(&dir);

    let start = Instant::now();
    let graph = code_repo_wiki::analysis::build_graph(&insights).unwrap();
    let modules = code_repo_wiki::analysis::detect_modules(&graph).unwrap();
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
    let mut engine = code_repo_wiki::search::text::TextEngine::open(&text_path).unwrap().0;

    let items: Vec<(code_repo_wiki::model::CodeNode, String)> = (0..1000)
        .map(|i| {
            let node = code_repo_wiki::model::CodeNode {
                id: code_repo_wiki::model::NodeId::new(i),
                kind: code_repo_wiki::model::NodeKind::Function,
                name: format!("bench_fn_{}", i),
                file_path: Some("src/lib.rs".into()),
                line_range: Some((i * 5, i * 5 + 10)),
                doc_comment: None,
                signature: Some(format!("fn bench_fn_{}(arg: u32) -> bool", i)), visibility: None,
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

/// 社区检测 + 特征聚类基准（演进计划 T4.1）
///
/// 合成 20 簇 × 10 文件 = 200 文件的模块化仓库：簇内互调、每簇仅向
/// 下一簇发 1 条跨簇调用。验证：
/// 1. 正确性——社区检测应还原出 ~20 个模块（seed 固定，结果确定性）；
/// 2. 性能——模块检测与特征聚类（纯结构降级）耗时报告。
#[test]
fn bench_clustering_detection() {
    if std::env::var("CI").is_ok() {
        eprintln!("skip bench: CI");
        return;
    }

    let dir = std::env::temp_dir().join(format!("bench_cluster_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 20 簇 × 10 文件：每文件定义 f{m}_{i} 与 g{m}_{i}；
    // f 调用同簇全部其他文件的 g（簇内完全图——模拟真实仓库稠密的
    // 簇内协作，CPM 对纯环结构只能合并 2 节点，完全图才有正合并增量），
    // 每簇第 0 个文件的 g 额外调用下一簇第 0 个文件的 f（单条跨簇边）
    const CLUSTERS: usize = 20;
    const PER_CLUSTER: usize = 10;
    for m in 0..CLUSTERS {
        for i in 0..PER_CLUSTER {
            // f 调用同簇全部其他文件的 g（完全图）
            let mut body = format!("pub fn f{m}_{i}(x: u32) -> u32 {{");
            for j in 0..PER_CLUSTER {
                if j != i {
                    body.push_str(&format!(" g{m}_{j}(x) +"));
                }
            }
            body.push_str(" x }\n");
            // 每簇第 0 个文件的 g 指向下一簇（单条跨簇边），其余 g 为内部实现
            if i == 0 {
                let next_cluster = (m + 1) % CLUSTERS;
                body.push_str(&format!(
                    "pub fn g{m}_{i}(x: u32) -> u32 {{ f{next_cluster}_0(x) + x }}\n"
                ));
            } else {
                body.push_str(&format!("pub fn g{m}_{i}(x: u32) -> u32 {{ x }}\n"));
            }
            let sub = dir.join(format!("m{m}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("f{i}.rs")), body).unwrap();
        }
    }

    let insights = code_repo_wiki::ingest::scan_and_parse_at(&code_repo_wiki::project::ProjectRoot::new(dir.clone())).unwrap().insights;
    eprintln!(
        "debug: insights={} first_entities={:?} first_source={:?}",
        insights.len(),
        insights
            .first()
            .map(|i| i.entities.iter().map(|e| (e.name.clone(), e.kind.clone(), e.line_start, e.line_end)).collect::<Vec<_>>()),
        insights.first().map(|i| i.source.chars().take(120).collect::<String>())
    );

    // 图构建 + 模块检测
    let start = Instant::now();
    let graph = code_repo_wiki::analysis::build_graph(&insights).unwrap();
    let call_edges = {
        use petgraph::visit::{EdgeRef, IntoEdgeReferences};
        graph
            .graph
            .edge_references()
            .filter(|e| {
                graph
                    .graph
                    .edge_weight(e.id())
                    .map(|w| w.kind == code_repo_wiki::model::EdgeKind::Calls)
                    .unwrap_or(false)
            })
            .count()
    };
    eprintln!(
        "debug: nodes={} edges={} calls={} modules={}",
        graph.graph.node_count(),
        graph.graph.edge_count(),
        call_edges,
        graph.modules.len()
    );
    let modules = code_repo_wiki::analysis::detect_modules(&graph).unwrap();
    let detect_ms = start.elapsed().as_millis();

    // 正确性：20 簇跨簇边仅 20 条（弱连接），社区检测应还原接近 20 个模块；
    // 允许少量合并/拆分，但不允许塌缩成几个大模块（全并）或碎成 200 个（全拆）
    let n = modules.len();
    assert!(
        (10..=30).contains(&n),
        "社区检测应还原约 20 个模块，实际 {n} 个: {:?}",
        modules.iter().map(|m| &m.name).take(8).collect::<Vec<_>>()
    );
    // 命名确定性：公共目录前缀应含 m0/m1 等簇目录
    let names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("m0")), "簇目录应进入模块名: {names:?}");

    // 特征聚类（语义聚类，ClusterEmbedder 注入）
    let start = Instant::now();
    // v52 T11 适配：纯结构（None）下簇内 Jaccard=0 → 权重 0.25 < γ=0.4 不合并、
    // 单例过滤后为空；注入 ClusterEmbedder 走 semantic 通道（同簇 0.75 成簇）
    let features =
        code_repo_wiki::analysis::feature::detect_features(&graph, Some(&ClusterEmbedder)).unwrap();
    let feature_ms = start.elapsed().as_millis();
    // 每个跨簇调用边对应一个特征（至少 20 条跨簇调用应产生特征）
    assert!(
        features.len() >= 10,
        "跨簇调用应产生特征，实际 {} 个",
        features.len()
    );

    eprintln!(
        "bench clustering: {} files, {} modules in {}ms, {} features in {}ms",
        insights.len(),
        n,
        detect_ms,
        features.len(),
        feature_ms
    );
    let _ = std::fs::remove_dir_all(&dir);
}
