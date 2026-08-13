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
