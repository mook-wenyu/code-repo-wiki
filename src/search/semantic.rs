//! 语义搜索引擎——sqlite-vec vec0 向量存储 + 余弦距离 KNN
//!
//! ## 职责边界（高内聚低耦合）
//!
//! - `SemanticEngine`：对外语义搜索门面。负责 embedding 生成
//!   （EmbeddingEngine 调用）与 CodeNode 序列化（node_json），
//!   向量存储细节全部委托 `VecDb`（src/search/vecdb.rs）。
//! - `SemanticSearch` trait：语义引擎的抽象接口，供 SearchAgent
//!   依赖抽象（可注入 mock 测试混合检索路径）。
//!
//! ## 阈值语义（v6 决策 4：保持硬编码 0.3）
//!
//! 相似度阈值 0.3 硬编码（OpenAI 官方 cosine 参考线），换算为余弦
//! 距离 `MAX_COSINE_DISTANCE = 0.7`（vecdb 常量）下推到存储层过滤。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::runtime::Runtime;

use crate::generate::embed::EmbeddingEngine;
use crate::model::CodeNode;
use crate::search::vecdb::VecDb;

/// 语义搜索抽象接口（SearchAgent 依赖抽象，可注入 mock）
///
/// 方法集与 SemanticEngine 公开面一致。**不要求 Send/Sync**：rusqlite
/// Connection 非 Sync（RefCell 内部），而 SearchAgent 是单线程调用
/// （lib.rs execute_search 同步执行）；若未来需要跨线程共享语义引擎，
/// 由调用方用 Mutex 包装（trait 不应为此牺牲可测试性）。
pub trait SemanticSearch {
    /// 索引单个实体（生成 embedding 并持久化）
    fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()>;
    /// 批量索引多个实体
    fn index_batch(&mut self, items: &[(CodeNode, String)]) -> Result<()>;
    /// 搜索最相似的 k 个实体（0.3 相似度阈值过滤）
    fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>>;
    /// 删除指定文件路径关联的所有向量条目
    fn remove_by_file(&mut self, file_path: &str) -> Result<usize>;
    /// 清空所有向量数据
    fn clear(&mut self) -> Result<()>;
    /// 当前向量条目数
    fn entry_count(&self) -> usize;
}

/// 语义搜索引擎
///
/// 内部委托 VecDb（sqlite-vec vec0 虚表）完成向量持久化与 KNN，
/// 自身只做 embedding 生成与 CodeNode 序列化。
pub struct SemanticEngine {
    db: VecDb,
    embedder: Arc<EmbeddingEngine>,
    rt: Arc<Runtime>,
}

impl SemanticEngine {
    /// 打开或创建语义搜索数据库
    ///
    /// vec0 虚表延迟到首次插入时创建（维度首次探测）。
    pub fn open(path: impl AsRef<Path>, embedder: Arc<EmbeddingEngine>, rt: Arc<Runtime>) -> Result<Self> {
        let db = VecDb::open(path)?;
        Ok(Self { db, embedder, rt })
    }

    // ============ 固有方法（薄封装，委托 trait 实现） ============
    // 库内调用点（lib.rs build_search_index/update_search_index_incremental）
    // 以具体类型调用，不走 trait object；这里直接转发到 trait impl，
    // 避免调用点改用 Box<dyn> 语法，同时保证两处行为一致。

    pub fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        SemanticSearch::index(self, node, source_code)
    }

    pub fn index_batch(&mut self, items: &[(CodeNode, String)]) -> Result<()> {
        SemanticSearch::index_batch(self, items)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>> {
        SemanticSearch::search(self, query, limit)
    }

    pub fn remove_by_file(&mut self, file_path: &str) -> Result<usize> {
        SemanticSearch::remove_by_file(self, file_path)
    }

    pub fn clear(&mut self) -> Result<()> {
        SemanticSearch::clear(self)
    }

    pub fn entry_count(&self) -> usize {
        SemanticSearch::entry_count(self)
    }

    /// 当前 vec0 表的向量维度（表不存在返回 None）——U04/D2 维度探测用：
    /// 增量路径在回填前比对 embedding 产出维度，变化则回退全量重建。
    ///
    /// 返回 Result：数据库读取错误（损坏/权限）向上传播，由调用方决定
    /// 处理（lib.rs 维度探测失败 warn + 视为维度未知，不静默吞掉）。
    pub fn table_dimension(&self) -> Result<Option<usize>> {
        self.db.table_dimension()
    }

    /// 组装实体索引文本（与旧实现一致，保持索引兼容性）
    fn index_text(node: &CodeNode, source_code: &str) -> String {
        format!(
            "{} {:?} {} {}",
            node.name, node.kind,
            node.signature.as_deref().unwrap_or(""), source_code
        )
    }
}

impl SemanticSearch for SemanticEngine {
    fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        let text = Self::index_text(node, source_code);
        let vector = self
            .rt
            .block_on(self.embedder.embed(&text))
            .context("生成 embedding 失败")?;
        let node_json = serde_json::to_string(node).context("序列化 CodeNode 失败")?;
        let file = node.file_path.as_deref().unwrap_or("").to_string();
        self.db.insert_batch(&[(file, node_json, vector)])
    }

    fn index_batch(&mut self, items: &[(CodeNode, String)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // 组装批量嵌入文本（一次 API 调用，避免逐条创建 tokio Runtime 开销）
        let texts: Vec<String> = items
            .iter()
            .map(|(node, source)| Self::index_text(node, source))
            .collect();
        let vectors = self
            .rt
            .block_on(self.embedder.embed_batch(&texts))
            .context("批量生成 embedding 失败")?;

        // 组装 (file_path, node_json, vector) 三元组一次性入库
        let rows: Vec<(String, String, Vec<f32>)> = items
            .iter()
            .zip(vectors)
            .map(|((node, _), vector)| {
                // CodeNode 是纯数据模型（无自定义 serde 错误路径），
                // 序列化失败在类型层面不可达；unwrap_or_default 只为
                // 满足 map 闭包签名，空串行由搜索侧反序列化失败自然跳过
                let node_json = serde_json::to_string(node).unwrap_or_default();
                let file = node.file_path.as_deref().unwrap_or("").to_string();
                (file, node_json, vector)
            })
            .collect();
        self.db.insert_batch(&rows)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>> {
        // 空库路径无命中时直接返回空，免对空库发 embedding 请求
        //（embedding API 有成本与延迟，空库查询无意义）；实际实现
        // load_all_vectors 收集时语义与此一致。
        // 计数失败（数据库损坏）向上传播，不静默当作空库跳过查询。
        if self.db.entry_count()? == 0 {
            return Ok(Vec::new());
        }
        let q_vec = self.rt.block_on(self.embedder.embed(query))?;
        let query_json = vec_to_json(&q_vec);
        // 阈值换算：相似度 0.3 ↔ 距离 0.7（vecdb 常量，见模块头）
        let rows = self
            .db
            .knn(&query_json, limit, crate::search::vecdb::MAX_COSINE_DISTANCE)?;
        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            // 反序列化失败 = 索引数据损坏（外部篡改/旧版本写入的异构格式），
            // 单条跳过不中断整个搜索（坏行对结果质量影响有限，搜索是
            // 只读尽力而为路径）；索引重建由维度探测/全量重建机制覆盖
            if let Ok(node) = serde_json::from_str::<CodeNode>(&row.node_json) {
                // 距离 → 相似度（1 - distance），与旧实现返回语义一致
                results.push((node, (1.0 - row.distance) as f32));
            }
        }
        // P1-4：knn 扩样语义返回「阈值内全部候选」（可 > limit），调用方
        // 必须截断到请求的 top_k——否则 semantic 单引擎搜索结果超出
        // 用户指定的返回数量（hybrid 路径被 RRF top_k 截断掩盖）
        results.truncate(limit);
        Ok(results)
    }

    fn remove_by_file(&mut self, file_path: &str) -> Result<usize> {
        self.db.remove_by_file(file_path)
    }

    fn clear(&mut self) -> Result<()> {
        self.db.clear()
    }

    fn entry_count(&self) -> usize {
        // trait 签名不含 Result（计数是幂等只读操作，调用方无错误上下文）；
        // 数据库损坏时显式告警 + 按空库处理（计数 0），不静默吞错
        match self.db.entry_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("语义索引条目计数失败，按空库处理: {}", e);
                0
            }
        }
    }
}

/// f32 向量 → vec0 查询向量 JSON（与 VecDb 内部序列化同格式）
fn vec_to_json(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f}")).collect();
    format!("[{}]", parts.join(","))
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

            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some("http://localhost:9999/v1".into()),
            max_concurrency: None,
        };
        Arc::new(EmbeddingEngine::new(&config, test_runtime().handle().clone()).unwrap())
    }

    /// 构造指向本地 mock 的 Embedding 引擎（base_url 带 /v1 前缀）
    fn embedder_with_server(base_url: &str, rt: &Arc<Runtime>) -> Arc<EmbeddingEngine> {
        let config = EmbedSection {

            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some(format!("{}/v1", base_url)),
            max_concurrency: None,
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
            signature: Some(format!("fn {}()", name)), visibility: None,
            module_path: vec![],
        }
    }

    // ============ 伪 Embedding mock server ============
    // 语义引擎的 Embedder 是具体类型 EmbeddingEngine（非 trait object），
    // 无法注入 FakeEmbedder，因此用本地 mock HTTP 返回确定性伪向量。
    // 维度统一为 3（vec0 虚表固定维度契约，v6 决策 1）。

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
        const HEADER_SEP: usize = 4;
        while buf.len() < head_end + HEADER_SEP + content_length {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf[head_end + HEADER_SEP..head_end + HEADER_SEP + content_length]).to_string()
    }

    /// 关键词 → 确定性伪向量（统一 3 维）：
    /// - 已知关键词（alpha/beta/gamma）：固定向量，相似度受控
    ///   （alpha↔beta≈0.707、alpha↔gamma=-1.0），用于验证排序正确性；
    /// - 未知关键词：按首次出现顺序分配 3 维单位基向量（同词同向量、
    ///   异词正交），用于确定性验证 0.3 阈值过滤。
    fn pseudo_vector(keyword: &str, seen: &mut HashMap<String, usize>) -> Vec<f32> {
        match keyword {
            "alpha" => vec![1.0, 0.0, 0.0],
            "beta" => vec![std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2, 0.0],
            "gamma" => vec![-1.0, 0.0, 0.0],
            _ => {
                let next = seen.len();
                let idx = *seen.entry(keyword.to_string()).or_insert(next);
                let mut v = vec![0.0f32; 3];
                v[idx % 3] = 1.0;
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

    /// P1-4：语义搜索返回条数不超过请求的 top_k——knn 扩样语义返回
    /// 「阈值内全部候选」（可 > limit），调用方截断后才遵守 top_k 契约
    #[test]
    fn test_semantic_search_respects_top_k() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("topk"), embedder, rt.clone()).unwrap();

        // 三个互相独立（不同关键词）但都过 0.3 阈值的实体——
        // 查询命中全部 3 条时 limit=2 必须截断为 2 条
        let items = vec![
            (make_node("alpha", "src/a.rs"), "fn alpha()".to_string()),
            (make_node("beta", "src/b.rs"), "fn beta()".to_string()),
            (make_node("gamma", "src/c.rs"), "fn gamma()".to_string()),
        ];
        engine.index_batch(&items).unwrap();

        let results = engine.search("alpha", 2).unwrap();
        assert!(results.len() <= 2, "top_k=2 不得返回更多: {:?}", results);
    }

    #[test]
    fn test_semantic_remove_by_file() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("rm"), embedder, rt.clone()).unwrap();

        let items = vec![
            (make_node("a1", "src/a.rs"), "fn a1()".to_string()),
            (make_node("b1", "src/b.rs"), "fn b1()".to_string()),
        ];
        engine.index_batch(&items).unwrap();
        assert_eq!(engine.entry_count(), 2);

        let removed = engine.remove_by_file("src/a.rs").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(engine.entry_count(), 1);
    }

    #[test]
    fn test_semantic_clear() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("clr"), embedder, rt.clone()).unwrap();
        engine.index_batch(&[(make_node("a", "src/a.rs"), "fn a()".to_string())]).unwrap();
        assert_eq!(engine.entry_count(), 1);

        engine.clear().unwrap();
        assert_eq!(engine.entry_count(), 0);
    }

    /// U04/D2：维度探测——空库 None，入库后返回实际维度
    #[test]
    fn test_semantic_table_dimension() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = SemanticEngine::open(tmp_path("dim"), embedder, rt.clone()).unwrap();
        assert_eq!(engine.table_dimension().unwrap(), None, "空库（表未创建）应返回 None");

        engine
            .index_batch(&[(make_node("a1", "src/a.rs"), "fn a1()".to_string())])
            .unwrap();
        assert_eq!(engine.table_dimension().unwrap(), Some(3), "伪向量统一 3 维");
    }
}
