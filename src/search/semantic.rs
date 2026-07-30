use std::path::Path;
use std::sync::Arc;
use anyhow::{Context, Result};

use crate::model::CodeNode;
use crate::generate::embed::EmbeddingEngine;

/// 语义搜索引擎——bincode 持久化向量存储
///
/// 向量数据通过 bincode 序列化到磁盘文件，进程重启后自动加载。
/// 搜索在内存中执行余弦相似度计算。
pub struct SemanticEngine {
    entries: Vec<(CodeNode, Vec<f32>)>,
    embedder: Arc<EmbeddingEngine>,
    persist_path: Option<std::path::PathBuf>,
}

impl SemanticEngine {
    /// 打开或创建持久化向量搜索数据库。
    pub fn open(path: impl AsRef<Path>, embedder: Arc<EmbeddingEngine>) -> Result<Self> {
        let persist_path = path.as_ref().to_path_buf();
        if persist_path.exists() {
            let data = std::fs::read(&persist_path)
                .context("读取持久化向量文件失败")?;
            let persisted: PersistedVectors = bincode::deserialize(&data)
                .context("反序列化持久化向量失败")?;
            return Ok(Self {
                entries: persisted.entries,
                embedder,
                persist_path: Some(persist_path),
            });
        }
        Ok(Self {
            entries: Vec::new(),
            embedder,
            persist_path: Some(persist_path),
        })
    }

    /// 纯内存模式（不持久化）
    pub fn new_in_memory(embedder: Arc<EmbeddingEngine>) -> Self {
        Self { entries: Vec::new(), embedder, persist_path: None }
    }

    /// 持久化到文件
    fn save(&self) -> Result<()> {
        if let Some(ref path) = self.persist_path {
            let data = PersistedVectors { entries: self.entries.clone() };
            let bytes = bincode::serialize(&data)
                .context("序列化持久化向量失败")?;
            std::fs::write(path, &bytes)
                .context("写入持久化向量文件失败")?;
        }
        Ok(())
    }

    /// 索引一个实体：生成 embedding 并持久化。
    pub fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        let text = format!(
            "{} {:?} {} {}",
            node.name, node.kind,
            node.signature.as_deref().unwrap_or(""), source_code
        );
        let rt = tokio::runtime::Runtime::new()
            .context("创建 tokio runtime 失败")?;
        let vector = rt.block_on(self.embedder.embed(&text))
            .context("生成 embedding 失败")?;
        self.entries.push((node.clone(), vector));
        self.save()?;
        Ok(())
    }

    /// 搜索最相似的 k 个实体。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f32)>> {
        if self.entries.is_empty() {
            return Ok(Vec::new());
        }
        let rt = tokio::runtime::Runtime::new()
            .context("创建 tokio runtime 失败")?;
        let q_vec = rt.block_on(self.embedder.embed(query))?;

        let mut scores: Vec<(usize, f32)> = self.entries.iter().enumerate()
            .map(|(i, (_, v))| (i, EmbeddingEngine::cosine_similarity(&q_vec, v)))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let results = scores.into_iter()
            .filter(|(_, s)| *s > 0.3)
            .take(limit)
            .map(|(i, s)| (self.entries[i].0.clone(), s))
            .collect();
        Ok(results)
    }

    /// 清空所有向量数据（同时删除持久化文件）
    pub fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        if let Some(ref path) = self.persist_path {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    pub fn entry_count(&self) -> usize { self.entries.len() }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct PersistedVectors {
    entries: Vec<(CodeNode, Vec<f32>)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::EmbedSection;

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

    fn tmp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("semantic_engine_test_{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_semantic_new() {
        let engine = SemanticEngine::open(tmp_path(), mock_embedder()).unwrap();
        assert_eq!(engine.entry_count(), 0);
    }

    #[test]
    fn test_search_empty() {
        let engine = SemanticEngine::open(tmp_path(), mock_embedder()).unwrap();
        assert!(engine.search("test", 10).unwrap().is_empty());
    }
}