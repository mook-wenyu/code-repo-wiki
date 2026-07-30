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
        Arc::new(EmbeddingEngine::new(&config).unwrap())
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
}
