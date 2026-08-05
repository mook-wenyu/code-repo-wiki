#![cfg(test)]

//! G3: 阅读指南 index.md 生成测试
//!
//! 单元测试：LLM 失败 → 确定性降级（同输入两次输出一致 + 入度降序/同入度
//! 名称字典序）、失败重试 1 次、成功路径产物含模块链接与正确元数据。
//! 集成测试：expand_languages 时 index.md 只出现在主语言目录
//! （本地 mock LLM server，零网络）。

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::copy_dir;

use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableDiGraph};

use repo_wiki::config::schema::WikiConfig;
use repo_wiki::generate::index::{fallback_index_guide, generate_index_guide};
use repo_wiki::generate::llm::{LlmProvider, Message};
use repo_wiki::model::{
    CodeEdge, CodeNode, DocumentKind, EdgeKind, KnowledgeGraph, ModuleCluster, NodeId, NodeKind,
};

/// 恒失败的 LLM provider（模拟 LLM 通道不可用）
struct FailingProvider;

impl LlmProvider for FailingProvider {
    async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("LLM 通道不可用"))
    }
}

/// 第一次失败、第二次成功的 provider（验证"失败重试 1 次"语义）
struct RetryOnceProvider {
    calls: AtomicUsize,
}

impl LlmProvider for RetryOnceProvider {
    async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            Err(anyhow::anyhow!("瞬时失败"))
        } else {
            Ok("# 阅读指南\n\n- [core](wiki/zh/core.md)\n".into())
        }
    }
}

/// 构造测试图（模块名 = 节点 module_path 单段）：
/// - core: 被 beta、gamma 依赖（入度 2）
/// - alpha: 被 gamma 依赖（入度 1）
/// - beta:  被 gamma 依赖（入度 1，与 alpha 同入度 → 断言按名称字典序）
/// - gamma: 依赖 alpha/beta/core（入度 0）
/// - zeta:  无依赖无依赖方（入度 0）
///
/// 期望排序：core, alpha, beta, gamma, zeta
fn make_graph() -> KnowledgeGraph {
    let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
    let nodes: Vec<(String, NodeIndex)> = ["core", "alpha", "beta", "gamma", "zeta"]
        .iter()
        .map(|m| {
            (
                m.to_string(),
                g.add_node(CodeNode {
                    id: NodeId::new(0),
                    kind: NodeKind::Function,
                    name: format!("{m}_fn"),
                    file_path: None,
                    line_range: None,
                    doc_comment: None,
                    signature: None, visibility: None,
                    module_path: vec![m.to_string()],
                }),
            )
        })
        .collect();
    let index = |name: &str| nodes.iter().find(|(n, _)| n == name).unwrap().1;
    let mut add_edge = |src: NodeIndex, dst: NodeIndex| {
        g.add_edge(
            src,
            dst,
            CodeEdge {
                id: EdgeIndex::new(0),
                kind: EdgeKind::Calls,
                source: src,
                target: dst,
                weight: 1.0,
                location: None,
            },
        );
    };
    add_edge(index("gamma"), index("alpha"));
    add_edge(index("gamma"), index("beta"));
    add_edge(index("gamma"), index("core"));
    add_edge(index("beta"), index("core"));
    KnowledgeGraph {
        graph: g,
        modules: nodes
            .iter()
            .map(|(name, idx)| ModuleCluster {
                name: name.clone(),
                node_ids: vec![*idx],
                cohesion: 0.0,
                coupling: 0.0,
                description: None,
            })
            .collect(),
        features: Vec::new(),
    }
}

/// 从降级骨架中提取链接行（按出现顺序）
fn link_lines(content: &str) -> Vec<&str> {
    content.lines().filter(|l| l.starts_with("- [")).collect()
}

/// ① LLM 失败 → 确定性降级：同输入两次输出一致，且按入度降序、
/// 同入度按名称字典序输出模块链接
#[tokio::test]
async fn test_llm_failure_falls_back_deterministically() {
    let graph = make_graph();
    let config = WikiConfig::default();

    let doc1 = generate_index_guide(&FailingProvider, &graph, &[], &config).await;
    let doc2 = generate_index_guide(&FailingProvider, &graph, &[], &config).await;
    assert_eq!(doc1.content, doc2.content, "降级骨架必须确定性一致");

    let links = link_lines(&doc1.content);
    assert_eq!(
        links,
        vec![
            "- [core](wiki/zh/core.md) — 入度 2, 被 beta, gamma 依赖",
            "- [alpha](wiki/zh/alpha.md) — 入度 1, 被 gamma 依赖",
            "- [beta](wiki/zh/beta.md) — 入度 1, 被 gamma 依赖",
            "- [gamma](wiki/zh/gamma.md) — 入度 0",
            "- [zeta](wiki/zh/zeta.md) — 入度 0",
        ],
        "应按入度降序、同入度按名称字典序输出"
    );
}

/// LLM 失败重试 1 次：首次失败、第二次成功 → 采用第二次输出且恰好调用 2 次
#[tokio::test]
async fn test_llm_failure_retries_once_then_succeeds() {
    let graph = make_graph();
    let config = WikiConfig::default();
    let provider = RetryOnceProvider {
        calls: AtomicUsize::new(0),
    };

    let doc = generate_index_guide(&provider, &graph, &[], &config).await;
    assert_eq!(doc.content, "# 阅读指南\n\n- [core](wiki/zh/core.md)\n");
    assert_eq!(provider.calls.load(Ordering::Relaxed), 2, "应恰好调用 2 次（1 次失败 + 1 次重试）");
}

/// 正常路径（LLM 成功）：产物直接采用 LLM 输出（含模块链接），
/// 元数据 title="index"、kind=TableOfContents、语言=主语言
#[tokio::test]
async fn test_success_path_uses_llm_content_with_links() {
    let graph = make_graph();
    let config = WikiConfig::default();
    let provider = RetryOnceProvider {
        calls: AtomicUsize::new(0),
    };

    let doc = generate_index_guide(&provider, &graph, &[], &config).await;
    assert!(doc.content.contains("wiki/zh/core.md"), "正常路径产物应含模块链接");
    assert_eq!(doc.title, "index");
    assert_eq!(doc.kind, DocumentKind::TableOfContents);
    assert_eq!(doc.language, "zh");
    assert_eq!(doc.module_path, Vec::<String>::new());
}

/// 降级骨架直接调用：链接列表与 references 指向模块页（写盘命名规则一致）
#[test]
fn test_fallback_sorted_by_in_degree_then_name() {
    let graph = make_graph();
    let config = WikiConfig::default();

    let doc = fallback_index_guide(&graph, &config);
    let links = link_lines(&doc.content);
    assert!(links[0].starts_with("- [core](wiki/zh/core.md)"), "入度最高者应排首位");
    assert!(
        links[1] < links[2],
        "同入度模块应按名称字典序: {} < {}",
        links[1],
        links[2]
    );
    assert_eq!(doc.references.len(), 5, "references 应覆盖全部模块");
    assert!(doc.references.iter().all(|r| r.target_path.starts_with("wiki/zh/")), "references 应指向主语言模块页");
}

/// ② 仅主语言：expand_languages 配置时，index.md 只出现在主语言目录
/// ③ 正常路径（mock LLM 返回内容）下产物含模块链接
#[test]
fn test_index_guide_primary_language_only_and_links() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = std::env::temp_dir().join(format!(
        "repo_wiki_test_index_guide_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    let root = repo_wiki::project::ProjectRoot::new(work_dir.clone());

    // 本地 mock LLM server：返回固定内容（含模块链接，无 path:line 形态，
    // 不触发模块页引用校验），生成调用成功且零重试延迟
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r##"{"choices":[{"message":{"content":"# 阅读指南\n\n- [模块](wiki/zh/src_auth.md)\n"}}]}"##;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });

    // expand_languages=["en"]：主语言 zh + 扩展语言 en，index.md 必须只写 zh。
    // output.dir 用绝对路径（独立 out 目录）：相对路径相对测试进程 cwd 解析，
    // 会污染仓库根；渲染路径 = output.dir/wiki/{lang}/index.md
    let out_dir = work_dir.join("out");
    let config = format!(
        r#"
[scope]
include = ["**/*.rs"]
exclude = []

[wiki]
language = "zh"
expand_languages = ["en"]

[output]
dir = "{out_dir}"
format = "markdown"

[llm]
provider = "openai-compatible"
model = "gpt-4o"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock"
api_key_env = "OPENAI_API_KEY"
max_concurrent = 1

[incremental]
enabled = false
strategy = "git-diff"

[search]
enabled = false
index_dir = ".search"
default_engine = "text"
default_top_k = 10
"#,
        out_dir = out_dir.to_string_lossy().replace('\\', "/"),
        port = port,
    );
    std::fs::write(work_dir.join("config.toml"), config).unwrap();

    let result = repo_wiki::run_pipeline(
        &work_dir.join("config.toml"),
        None,
        true,
        &root,
        &repo_wiki::GenerationMode::Full,
    );
    assert!(result.is_ok(), "流水线应成功: {:?}", result.err());

    let zh_index = out_dir.join("wiki").join("zh").join("index.md");
    assert!(zh_index.exists(), "index.md 应写入主语言目录 wiki/zh/");
    let content = std::fs::read_to_string(&zh_index).unwrap();
    assert!(content.contains("wiki/zh/"), "正常路径产物应含模块链接，实际: {content}");
    assert!(
        !out_dir.join("wiki").join("en").join("index.md").exists(),
        "index.md 只写主语言，扩展语言目录不得出现"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// U04/D8：阅读指南 LLM 输出坏 mermaid 时降级为 text 块（不重试），
/// 页面照常产出且坏图不出现在产物中
#[tokio::test]
async fn test_index_guide_degrades_bad_mermaid() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BadMermaidProvider {
        calls: AtomicUsize,
    }
    impl LlmProvider for BadMermaidProvider {
        async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("# 阅读指南\n\n```mermaid\nflowchart LR\nA[unterminated\n```\n".to_string())
        }
    }

    let graph = make_graph();
    let config = WikiConfig::default();
    let provider = BadMermaidProvider { calls: AtomicUsize::new(0) };

    let doc = generate_index_guide(&provider, &graph, &[], &config).await;
    assert!(!doc.content.contains("```mermaid"), "坏图不应以 mermaid 块出现");
    assert!(doc.content.contains("```text"), "坏块应降级为 text fence");
    assert!(
        doc.content.contains("repo-wiki: mermaid parse failed"),
        "应含降级标记注释"
    );
    assert_eq!(provider.calls.load(Ordering::Relaxed), 1, "坏图不应触发重试（只降级）");
}
