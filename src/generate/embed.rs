use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

use crate::analysis::feature::Embedder;
use crate::config::schema::EmbedSection;

/// Embedding 引擎：将代码/文档文本转为向量表示。
///
/// 调用 OpenAI 兼容的嵌入 API（支持 text-embedding-3-small 等模型）。
pub struct EmbeddingEngine {
    client: reqwest::Client,
    config: EmbedSection,
    call_count: AtomicUsize,
    /// 全局 tokio Runtime 句柄（同步 Embedder 实现经其驱动 async 请求）
    rt: tokio::runtime::Handle,
}

impl EmbeddingEngine {
    /// 从配置创建 Embedding 引擎。
    ///
    /// 优先使用 `api_key` 字段，其次从环境变量读取。
    /// `rt` 传入全局 Runtime 句柄（语义索引与特征聚类共用）。
    pub fn new(config: &EmbedSection, rt: tokio::runtime::Handle) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("创建 Embedding HTTP 客户端失败")?;
        Ok(Self {
            client,
            config: config.clone(),
            call_count: AtomicUsize::new(0),
            rt,
        })
    }

    /// 解析 API Key，优先级：api_key > 环境变量 > 报错
    fn resolve_api_key(&self) -> Result<String> {
        self.config
            .api_key
            .clone()
            .or_else(|| std::env::var(&self.config.api_key_env).ok())
            .context(format!(
                "Embedding API Key 未设置（api_key 为空且环境变量 {} 未定义）",
                self.config.api_key_env
            ))
    }

    /// 获取 API base URL
    fn resolve_base_url(&self) -> String {
        self.config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
    }

    /// 批量嵌入：将多个文本转为向量。
    ///
    /// 按 `batch_size` 分批发往 API，自动合并结果。
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let api_key = self.resolve_api_key()?;
        let url = format!("{}/embeddings", self.resolve_base_url());

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.config.batch_size) {
            let body = serde_json::json!({
                "model": self.config.model,
                "input": chunk,
            });

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
                .with_context(|| "Embedding API 请求失败")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Embedding API 返回错误 ({}): {}", status, text);
            }

            self.call_count.fetch_add(1, Ordering::Relaxed);

            let data: serde_json::Value = resp
                .json()
                .await
                .context("解析 Embedding API 响应 JSON 失败")?;

            let embeddings = data["data"]
                .as_array()
                .context("Embedding 响应缺少 data 字段")?
                .iter()
                .map(|item| {
                    item["embedding"]
                        .as_array()
                        .context("嵌入向量缺失")
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_f64().map(|f| f as f32))
                                .collect::<Vec<f32>>()
                        })
                })
                .collect::<Result<Vec<_>>>()?;

            all_embeddings.extend(embeddings);
        }

        Ok(all_embeddings)
    }

    /// 单文本嵌入。
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text.to_string()]).await?;
        results.into_iter().next().context("Embedding 返回空结果")
    }

    /// 余弦相似度（-1~1）。
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
    }

    /// 已完成的 API 调用次数。
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }
}

/// 特征聚类用的 Embedder 实现（analysis::feature::Embedder）。
///
/// 同步方法经内部持有的 tokio Handle 驱动 async 请求。
impl Embedder for EmbeddingEngine {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.rt.block_on(EmbeddingEngine::embed(self, text))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.rt
            .block_on(EmbeddingEngine::embed_batch(self, texts))
    }

    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
        EmbeddingEngine::cosine_similarity(a, b) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let sim = EmbeddingEngine::cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = EmbeddingEngine::cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = EmbeddingEngine::cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 0.0];
        let sim = EmbeddingEngine::cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let sim = EmbeddingEngine::cosine_similarity(&[], &[]);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_mismatched_length() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0];
        let sim = EmbeddingEngine::cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }
}
