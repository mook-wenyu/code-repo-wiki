//! 语义搜索引擎——SQLite 持久化向量存储
//!
//! 向量数据存储在 SQLite BLOB 列中，搜索时加载到内存执行余弦相似度计算。
//! 支持并发读取（WAL 模式）。

use std::sync::Arc;
use std::path::Path;
use anyhow::{Context, Result};
use tokio::runtime::Runtime;

use crate::model::CodeNode;
use crate::generate::embed::EmbeddingEngine;
use super::store::SearchStore;

/// 语义搜索引擎
///
/// 内部委托 SearchStore（SQLite）完成向量持久化，
/// 搜索时从 SQLite 加载所有向量到内存，执行余弦相似度排序。
pub struct SemanticEngine {
    store: SearchStore,
    embedder: Arc<EmbeddingEngine>,
    rt: Arc<Runtime>,
}

impl SemanticEngine {
    /// 打开或创建持久化向量搜索数据库。
    pub fn open(path: impl AsRef<Path>, embedder: Arc<EmbeddingEngine>, rt: Arc<Runtime>) -> Result<Self> {
        let store = SearchStore::open(path)?;
        Ok(Self { store, embedder, rt })
    }

    /// 索引一个实体：生成 embedding 并持久化。
    pub fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        let text = format!(
            "{} {:?} {} {}",
            node.name, node.kind,
            node.signature.as_deref().unwrap_or(""), source_code
        );
        let vector = self.rt.block_on(self.embedder.embed(&text))
            .context("生成 embedding 失败")?;
        self.store.insert_vectors_batch(&[(node.clone(), vector)])
    }

    /// 批量索引多个实体：一次性生成所有 embedding 并持久化。
    ///
    /// 内部调用 `EmbeddingEngine::embed_batch` 批量获取向量，
    /// 避免逐条创建 tokio Runtime 的开销。
    pub fn index_batch(&mut self, items: &[(CodeNode, String)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // 组装批量嵌入文本
        let texts: Vec<String> = items.iter().map(|(node, source)| {
            format!(
                "{} {:?} {} {}",
                node.name, node.kind,
                node.signature.as_deref().unwrap_or(""), source
            )
        }).collect();

        // 一次性获取所有向量
        let vectors = self.rt.block_on(self.embedder.embed_batch(&texts))
            .context("批量生成 embedding 失败")?;

        // 组装 (node, vector) 对并写入 SQLite
        let pairs: Vec<(CodeNode, Vec<f32>)> = items.iter()
            .zip(vectors)
            .map(|((node, _), vector)| (node.clone(), vector))
            .collect();
        self.store.insert_vectors_batch(&pairs)
    }

    /// 搜索最相似的 k 个实体。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>> {
        let all_vectors = self.store.load_all_vectors()?;
        if all_vectors.is_empty() {
            return Ok(Vec::new());
        }

        let q_vec = self.rt.block_on(self.embedder.embed(query))?;

        let mut scores: Vec<(usize, f32)> = all_vectors.iter().enumerate()
            .map(|(i, (_, v))| (i, EmbeddingEngine::cosine_similarity(&q_vec, v)))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let results = scores.into_iter()
            .filter(|(_, s)| *s > 0.3)
            .take(limit)
            .map(|(i, s)| (all_vectors[i].0.clone(), s))
            .collect();
        Ok(results)
    }

    /// 删除指定文件路径关联的所有向量条目。
    pub fn remove_by_file(&mut self, file_path: &str) -> Result<usize> {
        self.store.delete_vectors_by_file(file_path)
    }

    /// 清空所有向量数据。
    pub fn clear(&mut self) -> Result<()> {
        self.store.clear_vectors()
    }

    /// 当前向量条目数。
    pub fn entry_count(&self) -> usize {
        self.store.vector_count().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::EmbedSection;
    use crate::model::{NodeId, NodeKind};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use tokio::runtime::Runtime;

    fn test_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new().unwrap())
    }

    fn mock_embedder() -> Arc<EmbeddingEngine> {
        let config = EmbedSection {
            enabled: false,
            provider: crate::config::schema::EmbedProviderType::OpenAI,
            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some("http://localhost:9999/v1".into()),
            batch_size: 10,
            dimension: Some(1536),
        };
        Arc::new(EmbeddingEngine::new(&config, test_runtime().handle().clone()).unwrap())
    }

    /// 构造指向本地 mock 的 Embedding 引擎（base_url 带 /v1 前缀）。
    /// rt 由调用方持有引用，保证 block_on 期间 runtime 存活。
    fn embedder_with_server(base_url: &str, rt: &Arc<Runtime>) -> Arc<EmbeddingEngine> {
        let config = EmbedSection {
            enabled: false,
            provider: crate::config::schema::EmbedProviderType::OpenAI,
            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some(format!("{}/v1", base_url)),
            batch_size: 10,
            dimension: None,
        };
        Arc::new(EmbeddingEngine::new(&config, rt.handle().clone()).unwrap())
    }

    fn tmp_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEM_COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = SEM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("semantic_fts_{}_{}.db", label, id));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn make_node(name: &str, file: &str) -> CodeNode {
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: name.into(),
            file_path: Some(file.into()),
            line_range: Some((1, 10)),
            doc_comment: None,
            signature: Some(format!("fn {}()", name)),
            module_path: vec![],
        }
    }

    // ============ 伪 Embedding mock server ============
    // 语义引擎的 Embedder 是具体类型 EmbeddingEngine（非 trait object），
    // 无法注入 FakeEmbedder，因此用本地 mock HTTP 返回确定性伪向量。

    /// 缓冲区中是否已出现完整请求头（含 \r\n\r\n 分隔符，其后可能还有请求体）
    fn header_complete(buf: &[u8]) -> bool {
        buf.windows(4).any(|w| w == b"\r\n\r\n")
    }

    /// 读取一个 HTTP 请求体（按 Content-Length 跨包缓冲读取）
    fn read_request_body(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        while !header_complete(&buf) {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_length = head
            .split("\r\n")
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        // 请求体从头部结束标记 \r\n\r\n 之后开始（head_end 指向标记起始，+4 偏移）
        const HEADER_SEP: usize = 4;
        while buf.len() < head_end + HEADER_SEP + content_length {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf[head_end + HEADER_SEP..head_end + HEADER_SEP + content_length]).to_string()
    }

    /// 关键词 → 确定性伪向量：
    /// - 已知关键词（alpha/beta/gamma）：固定向量，相似度受控
    ///   （alpha↔beta≈0.707、alpha↔gamma=-1.0），用于验证排序正确性；
    /// - 未知关键词：按首次出现顺序分配单位基向量（同词同向量、异词正交），
    ///   用于确定性验证 0.3 阈值过滤。
    fn pseudo_vector(keyword: &str, seen: &mut HashMap<String, usize>) -> Vec<f32> {
        match keyword {
            "alpha" => vec![1.0, 0.0, 0.0],
            "beta" => vec![std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2, 0.0],
            "gamma" => vec![-1.0, 0.0, 0.0],
            _ => {
                let next = seen.len();
                let idx = *seen.entry(keyword.to_string()).or_insert(next);
                let mut v = vec![0.0f32; 16];
                v[idx % 16] = 1.0;
                v
            }
        }
    }

    /// 启动本地伪 Embedding mock server（std 线程 + std::net，无 tokio net 依赖）：
    /// 解析请求体中的 input 数组，按首个单词分配伪向量，返回同序 embedding 列表。
    fn spawn_pseudo_embed_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let body = read_request_body(&mut stream);
                    // 解析 input 数组，为每个文本分配伪向量
                    let inputs: Vec<String> = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v["input"].as_array().map(|a| {
                            a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
                        }))
                        .unwrap_or_default();
                    let mut guard = seen.lock().unwrap();
                    let vectors: Vec<Vec<f32>> = inputs.iter()
                        .map(|t| pseudo_vector(t.split_whitespace().next().unwrap_or(""), &mut guard))
                        .collect();
                    drop(guard);

                    let payload = serde_json::json!({
                        "data": vectors.iter().map(|v| serde_json::json!({"embedding": v})).collect::<Vec<_>>()
                    }).to_string();
                    let raw = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(), payload
                    );
                    let _ = stream.write_all(raw.as_bytes());
                });
            }
        });

        base_url
    }

    #[test]
    fn test_semantic_new() {
        let engine = SemanticEngine::open(tmp_path("new"), mock_embedder(), test_runtime()).unwrap();
        assert_eq!(engine.entry_count(), 0);
    }

    #[test]
    fn test_search_empty() {
        let engine = SemanticEngine::open(tmp_path("empty"), mock_embedder(), test_runtime()).unwrap();
        assert!(engine.search("test", 10).unwrap().is_empty());
    }

    #[test]
    fn test_semantic_search_ranks_by_similarity() {
        // 索引 3 个实体，查询 alpha：相似度 alpha=1.0 > beta≈0.707 > gamma=-1.0，
        // gamma 被 0.3 阈值过滤，剩余结果按相似度降序
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("rank"), embedder, rt.clone()).unwrap();

        let items = vec![
            (make_node("alpha", "src/a.rs"), "fn alpha()".to_string()),
            (make_node("beta", "src/b.rs"), "fn beta()".to_string()),
            (make_node("gamma", "src/c.rs"), "fn gamma()".to_string()),
        ];
        engine.index_batch(&items).unwrap();

        let results = engine.search("alpha", 10).unwrap();
        // 最相似实体排第一，次相似排第二
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.name, "alpha");
        assert_eq!(results[1].0.name, "beta");
        assert!(results[0].1 > results[1].1);
    }

    #[test]
    fn test_semantic_search_filters_below_threshold() {
        // 索引与查询完全无关的实体：查询 "q" 与已索引关键词正交（相似度 0.0），
        // 0.3 阈值过滤后结果为空
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("thr"), embedder, rt.clone()).unwrap();

        let items = vec![
            (make_node("x1", "src/x1.rs"), "x1 unrelated".to_string()),
            (make_node("x2", "src/x2.rs"), "x2 unrelated".to_string()),
        ];
        engine.index_batch(&items).unwrap();

        let results = engine.search("q", 10).unwrap();
        assert!(results.is_empty());
    }
}
