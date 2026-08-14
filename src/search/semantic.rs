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

use crate::analysis::feature::Embedder;
use crate::model::CodeNode;
use crate::search::block::Block;
use crate::search::query_cache::QueryEmbedCache;
use crate::search::vecdb::VecDb;

/// 语义搜索抽象接口（SearchAgent 依赖抽象，可注入 mock）
///
/// 方法集与 SemanticEngine 公开面一致。**不要求 Send/Sync**：rusqlite
/// Connection 非 Sync（RefCell 内部），而 SearchAgent 是单线程调用
/// （lib.rs execute_search 同步执行）；若未来需要跨线程共享语义引擎，
/// 由调用方用 Mutex 包装（trait 不应为此牺牲可测试性）。
pub trait SemanticSearch {
    /// 索引单个实体（v0.7.2 起输入块——嵌入 block.text，非裸源码片段）
    fn index(&mut self, node: &CodeNode, block: &Block) -> Result<()>;
    /// 批量索引多个实体
    fn index_batch(&mut self, items: &[(CodeNode, Block)]) -> Result<()>;
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
    embedder: Arc<dyn Embedder>,
    /// 查询 embedding LRU 缓存（重复 query 免二次 API 调用）
    query_cache: Arc<QueryEmbedCache>,
    /// 缓存键命名空间：embedding 模型名（P1 缓存键并入模型名——MCP 长驻
    /// 进程 + config.toml 热改 [embed].model 后，旧模型的 query 向量若仍
    /// 命中会与新索引维度/语义不匹配；见 query_cache.rs 模块头一致性说明）
    model: String,
}

impl SemanticEngine {
    /// 打开或创建语义搜索数据库
    ///
    /// vec0 虚表延迟到首次插入时创建（维度首次探测）。
    /// `query_cache` 注入查询向量 LRU 缓存（进程级共享，见 query_cache.rs）。
    /// `model` 为当前 embedding 模型名，并入查询缓存键（模型变更后旧缓存
    /// 自然失效，见模块头 P1 说明）。
    pub fn open(
        path: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
        query_cache: Arc<QueryEmbedCache>,
        model: &str,
    ) -> Result<Self> {
        let db = VecDb::open(path)?;
        Ok(Self {
            db,
            embedder,
            query_cache,
            model: model.to_string(),
        })
    }

    // ============ 固有方法（薄封装，委托 trait 实现） ============
    // 库内调用点（lib.rs build_search_index/update_search_index_incremental）
    // 以具体类型调用，不走 trait object；这里直接转发到 trait impl，
    // 避免调用点改用 Box<dyn> 语法，同时保证两处行为一致。

    pub fn index(&mut self, node: &CodeNode, block: &Block) -> Result<()> {
        SemanticSearch::index(self, node, block)
    }

    pub fn index_batch(&mut self, items: &[(CodeNode, Block)]) -> Result<()> {
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
}

impl SemanticSearch for SemanticEngine {
    fn index(&mut self, node: &CodeNode, block: &Block) -> Result<()> {
        // v0.7.2：嵌入块文本（作用域前缀 + 签名 + doc + body，见 block.rs），
        // 不再用裸实体源码片段——块文本承载模块路径/作用域上下文，语义
        // 检索不再退化为词袋
        let vector = self
            .embedder
            .embed(&block.text)
            .context("生成 embedding 失败")?;
        let node_json = serde_json::to_string(node).context("序列化 CodeNode 失败")?;
        let file = node.file_path.as_deref().unwrap_or("").to_string();
        self.db
            .insert_batch(&[(file, node_json, block.id.clone(), vector)])
    }

    fn index_batch(&mut self, items: &[(CodeNode, Block)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // 组装批量嵌入文本（一次 API 调用，避免逐条创建 tokio Runtime 开销）
        let texts: Vec<String> = items.iter().map(|(_, block)| block.text.clone()).collect();
        let vectors = self
            .embedder
            .embed_batch(&texts)
            .context("批量生成 embedding 失败")?;

        // 组装 (file_path, node_json, block_id, vector) 四元组一次性入库
        let rows: Vec<(String, String, String, Vec<f32>)> = items
            .iter()
            .zip(vectors)
            .map(|((node, block), vector)| {
                // CodeNode 是纯数据模型（无自定义 serde 错误路径），
                // 序列化失败在类型层面不可达；unwrap_or_default 只为
                // 满足 map 闭包签名，空串行由搜索侧反序列化失败自然跳过
                let node_json = serde_json::to_string(node).unwrap_or_default();
                let file = node.file_path.as_deref().unwrap_or("").to_string();
                (file, node_json, block.id.clone(), vector)
            })
            .collect();
        self.db.insert_batch(&rows)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>> {
        // 空串/纯空白查询短路：无关键词可嵌入，直接空结果（与 text 引擎
        // 空词表退化为空语义一致，也避免对空串发 embedding 请求）
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        // 空库路径无命中时直接返回空，免对空库发 embedding 请求
        //（embedding API 有成本与延迟，空库查询无意义）；实际实现
        // load_all_vectors 收集时语义与此一致。
        // 计数失败（数据库损坏）向上传播，不静默当作空库跳过查询。
        if self.db.entry_count()? == 0 {
            return Ok(Vec::new());
        }
        // v0.7.2：查询向量走 LRU 缓存——重复/近重复 query 免二次 API 调用
        // （MCP server 常驻进程内语义检索命中率高，省网络 RTT 与计费）。
        // P1：缓存键并入模型名（NUL 分隔，query 为用户文本不含 NUL 无碰撞）——
        // 长驻进程热改 [embed].model 后旧模型向量不再被误命中。
        let cache_key = format!("{}\u{0}{}", self.model, query);
        let q_vec = self.query_cache.get_or_embed(&cache_key, |key| {
            // 剥离模型名前缀取回原始查询文本
            let original = key.split_once('\u{0}').map(|(_, q)| q).unwrap_or(key);
            self.embedder.embed(original)
        })?;
        let query_json = vec_to_json(&q_vec);
        // 阈值换算：相似度 0.3 ↔ 距离 0.7（vecdb 常量，见模块头）
        let rows = self.db.knn(
            &query_json,
            limit,
            crate::search::vecdb::MAX_COSINE_DISTANCE,
        )?;
        let mut results = Vec::with_capacity(rows.len());
        // P0-A：检索侧按块去重——旧索引（去重修复前构建）可能存同一块的
        // 多行重复向量（impl 块 N+1 份）；每块只返回一条代表，语义检索
        // 真正「块级返回」，不被单块方法刷屏。knn 按距离升序稳定返回，
        // 首条命中即该块的代表节点。
        let mut seen_blocks: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in rows {
            // 空 block_id（旧行/外部工具插入，P1 修复后为 ""）退化用 node_json
            // 作键——node_json 是行唯一键（knn 已按它去重），不会误合并不同行
            let dedupe_key = if row.block_id.is_empty() {
                row.node_json.clone()
            } else {
                row.block_id.clone()
            };
            if !seen_blocks.insert(dedupe_key) {
                continue;
            }
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
    use crate::config::schema::{EmbedProvider, EmbedSection};
    use crate::generate::embed::EmbeddingEngine;
    use crate::model::{NodeId, NodeKind};
    use crate::search::block::EntityRef;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use tokio::runtime::Runtime;

    fn test_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new().unwrap())
    }

    fn mock_embedder() -> Arc<dyn Embedder> {
        let config = EmbedSection {
            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some("http://localhost:9999/v1".into()),
            max_concurrency: None,
            batch_concurrency: None,
            provider: EmbedProvider::Remote,
        };
        Arc::new(EmbeddingEngine::new(&config, test_runtime().handle().clone()).unwrap())
            as Arc<dyn Embedder>
    }

    /// 构造指向本地 mock 的 Embedding 引擎（base_url 带 /v1 前缀）
    fn embedder_with_server(base_url: &str, rt: &Arc<Runtime>) -> Arc<dyn Embedder> {
        let config = EmbedSection {
            model: "text-embedding-3-small".into(),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: Some(format!("{}/v1", base_url)),
            max_concurrency: None,
            batch_concurrency: None,
            provider: EmbedProvider::Remote,
        };
        Arc::new(EmbeddingEngine::new(&config, rt.handle().clone()).unwrap()) as Arc<dyn Embedder>
    }

    /// 确定性伪嵌入器（无网络）：按文本 contains 关键词分配 3 维向量，
    /// 与伪向量服务同规则（alpha/beta/gamma 映射见 pseudo_vec）。注入
    /// `SemanticEngine::open` 即可（open 接受 `Arc<dyn Embedder>`），
    /// 供不依赖本地 HTTP 的测试使用。
    struct FakeEmbedder;

    impl Embedder for FakeEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(pseudo_vec(text))
        }
        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| pseudo_vec(t)).collect())
        }
        fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
            crate::generate::embed::EmbeddingEngine::cosine_similarity(a, b) as f64
        }
    }

    /// 文本 → 3 维确定性伪向量（与 HTTP 伪向量服务同一映射规则）
    fn pseudo_vec(text: &str) -> Vec<f32> {
        if text.contains("alpha") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("beta") {
            vec![
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
            ]
        } else {
            vec![-1.0, 0.0, 0.0]
        }
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
            visibility: None,
            module_path: vec![],
        }
    }

    /// 从 CodeNode 构造测试块：块文本以实体名开头——伪向量服务器按
    /// 首 whitespace token 选向量，保证与旧 index_text 的首 token 同语义
    fn make_block(node: &CodeNode, body: &str) -> Block {
        let (start, end) = node.line_range.unwrap_or((1, 1));
        let file = node.file_path.as_deref().unwrap_or("");
        Block {
            id: format!("{file}#{start}-{end}"),
            file_path: file.to_string(),
            language: "rust".into(),
            module_path: node.module_path.clone(),
            kind: node.kind.clone(),
            name: node.name.clone(),
            line_range: (start, end),
            signature: node.signature.clone().unwrap_or_default(),
            visibility: node.visibility.clone(),
            scope: vec![],
            doc_comment: node.doc_comment.clone(),
            text: format!("{} {body}", node.name),
            entity: EntityRef {
                name: node.name.clone(),
                file_path: file.to_string(),
                line_range: (start, end),
            },
        }
    }

    /// 打开引擎：统一注入独立 query cache（测试间互不污染 LRU）+ 模型标记
    fn open_engine(path: std::path::PathBuf, embedder: Arc<dyn Embedder>) -> SemanticEngine {
        SemanticEngine::open(
            path,
            embedder,
            Arc::new(QueryEmbedCache::new()),
            "test-model",
        )
        .unwrap()
    }

    /// 构造 (CodeNode, Block) 索引项
    fn make_item(node: &CodeNode, body: &str) -> (CodeNode, Block) {
        (node.clone(), make_block(node, body))
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
        let head_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(buf.len());
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
        String::from_utf8_lossy(&buf[head_end + HEADER_SEP..head_end + HEADER_SEP + content_length])
            .to_string()
    }

    /// 关键词 → 确定性伪向量（统一 3 维）：
    /// - 已知关键词（alpha/beta/gamma/delta）：文本**任意位置**含该词即命中
    ///   对应向量——块文本是「模块路径 + 签名 + 完整 body」拼接，独特 token
    ///   可能出现在 body 中段，用 contains 模拟真实 embedding 对全文敏感；
    /// - 未知文本：按首个单词分配 3 维单位基向量（同词同向量、异词正交），
    ///   用于确定性验证 0.3 阈值过滤。
    fn pseudo_vector(text: &str, seen: &mut HashMap<String, usize>) -> Vec<f32> {
        if text.contains("alpha") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("beta") {
            vec![
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
            ]
        } else if text.contains("gamma") {
            vec![-1.0, 0.0, 0.0]
        } else if text.contains("delta") {
            // T03 弱锚点修复：delta 与 alpha 相似度 0.707（过 0.3 阈值）——
            // 供 top_k 截断测试构造「3 条候选全过阈值」场景
            vec![0.5, 0.5, 0.0]
        } else {
            let word = text.split_whitespace().next().unwrap_or("");
            let next = seen.len();
            let idx = *seen.entry(word.to_string()).or_insert(next);
            let mut v = vec![0.0f32; 3];
            v[idx % 3] = 1.0;
            v
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
                        .and_then(|v| {
                            v["input"].as_array().map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                        })
                        .unwrap_or_default();
                    let mut guard = seen.lock().unwrap();
                    let vectors: Vec<Vec<f32>> = inputs
                        .iter()
                        .map(|t| pseudo_vector(t, &mut guard))
                        .collect();
                    drop(guard);

                    let payload = serde_json::json!({
                        "data": vectors.iter().map(|v| serde_json::json!({"embedding": v})).collect::<Vec<_>>()
                    }).to_string();
                    let raw = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(raw.as_bytes());
                });
            }
        });

        base_url
    }

    #[test]
    fn test_semantic_new() {
        let engine = open_engine(tmp_path("new"), mock_embedder());
        assert_eq!(engine.entry_count(), 0);
    }

    #[test]
    fn test_search_empty() {
        let engine = open_engine(tmp_path("empty"), mock_embedder());
        assert!(engine.search("test", 10).unwrap().is_empty());
    }

    #[test]
    fn test_semantic_search_ranks_by_similarity() {
        // 索引 3 个实体，查询 alpha：相似度 alpha=1.0 > beta≈0.707 > gamma=-1.0，
        // gamma 被 0.3 阈值过滤，剩余结果按相似度降序
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = open_engine(tmp_path("rank"), embedder);

        let items = vec![
            make_item(&make_node("alpha", "src/a.rs"), "fn alpha()"),
            make_item(&make_node("beta", "src/b.rs"), "fn beta()"),
            make_item(&make_node("gamma", "src/c.rs"), "fn gamma()"),
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
        let mut engine = open_engine(tmp_path("thr"), embedder);

        let items = vec![
            make_item(&make_node("x1", "src/x1.rs"), "x1 unrelated"),
            make_item(&make_node("x2", "src/x2.rs"), "x2 unrelated"),
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
        let mut engine = open_engine(tmp_path("topk"), embedder);

        // 三个实体都与查询 "alpha" 相似度 ≥0.3（alpha 1.0、beta 0.707、
        // delta 0.707）——候选 3 条 > limit=2，truncate 必须真实截断；
        // 若删除 semantic.rs search 的 truncate(limit)，此处返回 3 条断言失败
        let items = vec![
            make_item(&make_node("alpha", "src/a.rs"), "fn alpha()"),
            make_item(&make_node("beta", "src/b.rs"), "fn beta()"),
            make_item(&make_node("delta", "src/d.rs"), "fn delta()"),
        ];
        engine.index_batch(&items).unwrap();

        let results = engine.search("alpha", 2).unwrap();
        assert_eq!(results.len(), 2, "top_k=2 必须精确截断: {:?}", results);
    }

    #[test]
    fn test_semantic_remove_by_file() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = open_engine(tmp_path("rm"), embedder);

        let items = vec![
            make_item(&make_node("a1", "src/a.rs"), "fn a1()"),
            make_item(&make_node("b1", "src/b.rs"), "fn b1()"),
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
        let mut engine = open_engine(tmp_path("clr"), embedder);
        engine
            .index_batch(&[make_item(&make_node("a", "src/a.rs"), "fn a()")])
            .unwrap();
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
        let mut engine = open_engine(tmp_path("dim"), embedder);
        assert_eq!(
            engine.table_dimension().unwrap(),
            None,
            "空库（表未创建）应返回 None"
        );

        engine
            .index_batch(&[make_item(&make_node("a1", "src/a.rs"), "fn a1()")])
            .unwrap();
        assert_eq!(
            engine.table_dimension().unwrap(),
            Some(3),
            "伪向量统一 3 维"
        );
    }

    /// 空串/纯空白查询短路：不触发 embedding 请求，直接空结果
    ///（与 text 引擎空词表退化语义一致，v0.7.2 显式加短路）
    #[test]
    fn test_semantic_search_empty_query() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = open_engine(tmp_path("empty_q"), embedder);
        engine
            .index_batch(&[make_item(&make_node("alpha", "src/a.rs"), "fn alpha()")])
            .unwrap();

        assert!(engine.search("", 10).unwrap().is_empty(), "空串查询返回空");
        assert!(
            engine.search("   ", 10).unwrap().is_empty(),
            "纯空白查询返回空"
        );
        // 非空查询不受影响
        assert_eq!(engine.search("alpha", 10).unwrap().len(), 1);
    }

    /// 块级检索冒烟（v0.7.2）：长函数 body 中段 token 命中——
    /// 块文本含完整 body（结构感知分块），中段独特 token 进入嵌入文本，
    /// 查询该 token 能命中该块；旧实现裸签名索引（不含 body 中段）做不到
    #[test]
    fn test_block_level_middle_token_hit() {
        let base_url = spawn_pseudo_embed_server();
        let rt = test_runtime();
        let embedder = embedder_with_server(&base_url, &rt);
        let mut engine = open_engine(tmp_path("block_mid"), embedder);

        // 长函数：body 首尾是填充行，独特 token "alpha" 埋在正中
        let mut body = String::from("fn process() {\n");
        for _ in 0..40 {
            body.push_str("    let padding = 1;\n");
        }
        body.push_str("    alpha\n"); // 中段独特 token
        for _ in 0..40 {
            body.push_str("    let trailing = 2;\n");
        }
        body.push('}');

        let mut node = make_node("process", "src/process.rs");
        node.line_range = Some((1, 83));
        let block = Block {
            id: "src/process.rs#1-83".into(),
            file_path: "src/process.rs".into(),
            language: "rust".into(),
            module_path: vec!["src".into(), "process".into()],
            kind: NodeKind::Function,
            name: "process".into(),
            line_range: (1, 83),
            signature: "fn process()".into(),
            visibility: None,
            scope: vec![],
            doc_comment: None,
            text: format!("src::process::process Function\n{body}"),
            entity: EntityRef {
                name: "process".into(),
                file_path: "src/process.rs".into(),
                line_range: (1, 83),
            },
        };
        engine.index_batch(&[(node, block)]).unwrap();

        // 查询中段 token：伪向量按全文 contains 命中 alpha 向量
        let results = engine.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1, "中段 token 查询应命中块: {:?}", results);
        assert_eq!(results[0].0.name, "process");
    }

    /// P0-A：检索侧按块去重——同一块（block_id 相同）被重复索引（旧版按
    /// 实体重复嵌入，impl 块 N+1 份）时，搜索只返回一条代表，不刷屏
    #[test]
    fn test_semantic_search_dedupes_by_block() {
        let embedder = Arc::new(FakeEmbedder) as Arc<dyn Embedder>;
        let mut engine = open_engine(tmp_path("dedup"), embedder);

        // 同一 impl 块的「实体重复」场景：Impl 实体 + 两个方法实体都映射到
        // 同一块（block.id 相同、块文本相同），3 行向量入库
        let mut impl_node = make_node("Point", "src/impl.rs");
        impl_node.kind = NodeKind::Impl;
        impl_node.line_range = Some((1, 10));
        let block = make_block(&impl_node, "alpha shared body");
        let method1 = make_node("new", "src/impl.rs");
        let method2 = make_node("get", "src/impl.rs");

        engine
            .index_batch(&[
                (impl_node, block.clone()),
                (method1, block.clone()),
                (method2, block),
            ])
            .unwrap();
        assert_eq!(engine.entry_count(), 3, "向量库仍存 3 行（模拟旧索引）");

        let results = engine.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1, "同一块只返回一条代表: {:?}", results);
    }

    /// P0-A：空 block_id（P1 把 NULL 降级为空串）行的去重退化——不同实体
    /// 的 node_json 不同，按 node_json 作键不误合并（否则两条不同结果会被
    /// 错误折叠成一条）
    #[test]
    fn test_semantic_search_dedupes_falls_back_on_empty_block_id() {
        let embedder = Arc::new(FakeEmbedder) as Arc<dyn Embedder>;
        let mut engine = open_engine(tmp_path("dedup_empty"), embedder);

        let mut node1 = make_node("alpha", "src/a.rs");
        let mut block1 = make_block(&node1, "alpha body");
        block1.id = String::new(); // 模拟 NULL → 空串
        node1.line_range = Some((1, 5));
        block1.line_range = (1, 5);

        let mut node2 = make_node("beta", "src/b.rs");
        let mut block2 = make_block(&node2, "alpha body");
        block2.id = String::new();
        node2.line_range = Some((1, 5));
        block2.line_range = (1, 5);

        engine
            .index_batch(&[(node1, block1), (node2, block2)])
            .unwrap();

        let results = engine.search("alpha", 10).unwrap();
        // 两条不同实体（node_json 不同），空 block_id 不合并
        assert_eq!(
            results.len(),
            2,
            "空 block_id 按 node_json 去重不误合并: {:?}",
            results
        );
    }

    /// P1：查询缓存键并入模型名——同 query 在不同模型引擎下各嵌一次、
    /// 互不命中（MCP 长驻进程热改 [embed].model 后旧模型向量不污染新查询）
    #[test]
    fn test_query_cache_namespaced_by_model() {
        let cache = Arc::new(QueryEmbedCache::new());
        let mut engine_a = SemanticEngine::open(
            tmp_path("ns_a"),
            Arc::new(FakeEmbedder) as Arc<dyn Embedder>,
            cache.clone(),
            "model-a",
        )
        .unwrap();
        let mut engine_b = SemanticEngine::open(
            tmp_path("ns_b"),
            Arc::new(FakeEmbedder) as Arc<dyn Embedder>,
            cache.clone(),
            "model-b",
        )
        .unwrap();
        engine_a
            .index_batch(&[make_item(&make_node("alpha", "src/a.rs"), "fn alpha()")])
            .unwrap();
        engine_b
            .index_batch(&[make_item(&make_node("beta", "src/b.rs"), "fn beta()")])
            .unwrap();

        engine_a.search("alpha", 10).unwrap();
        engine_b.search("alpha", 10).unwrap();
        // 两模型各自缓存一条 alpha 查询向量（缓存键含模型名，互不命中）
        assert_eq!(cache.len(), 2, "同 query 不同模型应各缓存一条");
    }
}
