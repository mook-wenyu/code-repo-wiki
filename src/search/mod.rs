pub mod agent;
pub mod ast;
/// 结构感知代码块（语义索引最小单元）
pub mod block;
pub mod callgraph;
/// 结构感知分块器（AstChunker / FileChunker）
pub mod chunker;
pub mod hybrid;
/// 查询 embedding LRU 缓存（语义搜索 query 向量复用，MCP server 常驻场景）
pub mod query_cache;
pub mod semantic;
pub mod store;
pub mod text;
/// CJK 检索关键词切分（搜索与评测共用，见 tokenize.rs 模块头注释）
pub(crate) mod tokenize;
pub mod vecdb;

/// 语义检索的相似度阈值（单一权威常量）。
///
/// 余弦相似度 ≥ 0.3 视为命中（OpenAI 官方 cosine 参考线，v6 决策 4 定档）。
/// 存储层（vecdb）以余弦距离过滤 = `1 - 相似度`，对应距离上限
/// [`crate::search::vecdb::MAX_COSINE_DISTANCE`] = 1 - 0.3 = 0.7，
/// 由该常量派生，避免两处各自硬编码漂移（历史：semantic.rs 写 0.3、
/// vecdb.rs 写 0.7 靠口述一致，曾存在漂移风险）。
pub const MIN_COSINE_SIMILARITY: f64 = 0.3;
