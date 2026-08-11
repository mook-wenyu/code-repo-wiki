use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// 并发信号量（P2-8）：embed_batch 分批发往 API 的并发上限（默认 4）
    semaphore: Arc<tokio::sync::Semaphore>,
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
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                {
                    let mc = config.max_concurrency.unwrap_or(4);
                    anyhow::ensure!(mc > 0, "max_concurrency 必须为正整数（当前 0）");
                    mc as usize
                }
            )),
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

        // P2-8：并发信号量——整批嵌入占一个许可（批次间并发属 T01-e 范围）
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;

        // P2-13：批次间并发——embedding 请求是纯网络 IO（无共享状态），
        // 串行等待每批 RTT 让大仓批量嵌入分钟级阻塞。buffer_unordered(4)
        // 并发发送、按 chunk 索引收集，保持结果顺序与串行一致。
        const EMBED_CONCURRENCY: usize = 4;
        let chunks: Vec<&[String]> = texts.chunks(crate::config::schema::EMBED_BATCH_SIZE).collect();
        let mut results: Vec<Option<Result<Vec<Vec<f32>>>>> = (0..chunks.len()).map(|_| None).collect();
        let model = self.config.model.clone();

        use futures::StreamExt;
        let mut stream = futures::stream::iter(chunks.iter().enumerate().map(|(idx, chunk)| {
            let client = self.client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let model = model.clone();
            async move {
                let body = serde_json::json!({ "model": model, "input": chunk });

            // N16：embedding 请求接入统一重试骨架（与 LLM 通道一致：429/5xx/
            // 超时/连接失败按指数退避重试，其余 4xx 立即失败）。每轮重试重建
            // 请求（闭包捕获 body/url/key 的引用）。
            // v47：retry_with_backoff 输出放宽为 anyhow（send 阶段首字节超时
            // 保护）；此处直接包一层 Ok(...) 保持 anyhow 语义。
                let resp = crate::generate::llm::retry_with_backoff(
                    crate::generate::llm::MAX_RETRIES,
                    || {
                        let body = &body;
                        let url = &url;
                        let api_key = &api_key;
                        let client = &client;
                        async move {
                            client
                                .post(url)
                                .bearer_auth(api_key)
                                .json(body)
                                .send()
                                .await
                                .map_err(anyhow::Error::from)
                        }
                    },
                )
                .await
                .with_context(|| "Embedding API 请求失败")?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Embedding API 返回错误 ({}): {}", status, text);
                }
                Ok::<_, anyhow::Error>((idx, resp))
            }
        }))
        .buffer_unordered(EMBED_CONCURRENCY);

        // 按索引收集（buffer_unordered 完成序与提交序不同，必须重排）
        while let Some(item) = stream.next().await {
            let (idx, resp) = item?;
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
                        let arr = item["embedding"]
                            .as_array()
                            .context("嵌入向量缺失")?;
                        // B6：元素必须全为数字——filter_map 静默丢弃非数字元素会
                        // 让向量降维而不报错（同批一致变短时维度校验也捕获不到），
                        // 模型输出异常必须显式失败而非产出残缺向量
                        arr.iter()
                            .map(|v| {
                                v.as_f64()
                                    .map(|f| f as f32)
                                    .with_context(|| "嵌入向量包含非数字元素（模型输出异常，拒绝静默丢弃）")
                            })
                            .collect::<Result<Vec<f32>>>()
                    })
                    .collect::<Result<Vec<_>>>()?;

                // N5 修复：响应校验——data 条数必须与请求批次一致，且同批
                // 向量维度必须一致。此前只校验"字段存在"，条数不足时
                // 索引错位（下游 zip 静默丢弃多余/缺失）、维度不一致时
                // 向量库维度校验失败但错误发生在数据已被吞之后。
                if embeddings.len() != chunks[idx].len() {
                    anyhow::bail!(
                        "Embedding 响应数量不匹配：请求 {} 条，返回 {} 条",
                        chunks[idx].len(),
                        embeddings.len()
                    );
                }
                if let Some(first) = embeddings.first() {
                    let dim = first.len();
                    if let Some(bad) = embeddings.iter().find(|v| v.len() != dim) {
                        anyhow::bail!(
                            "Embedding 响应维度不一致：{} 维与 {} 维并存",
                            dim,
                            bad.len()
                        );
                    }
                }

            results[idx] = Some(Ok(embeddings));
        }

        // 顺序收集（原串行语义：索引升序合并；缺失即内部错误）
        let mut all_embeddings = Vec::with_capacity(texts.len());
        for r in results {
            all_embeddings.extend(r.ok_or_else(|| anyhow::anyhow!("嵌入批次结果缺失"))??);
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

/// 按配置 provider 构造嵌入器（统一入口，替代调用点直接 new EmbeddingEngine）：
/// - Remote/Mock：HTTP 通道（EmbeddingEngine；Mock 为测试模板兼容，请求指向 mock server）
/// - Local：本地 fastembed 通道（LocalEmbedder，免 API key）
pub fn build_embedder(
    config: &EmbedSection,
    rt: tokio::runtime::Handle,
) -> Result<std::sync::Arc<dyn crate::analysis::feature::Embedder>> {
    match config.provider {
        crate::config::schema::EmbedProvider::Local => {
            Ok(std::sync::Arc::new(LocalEmbedder::new(&config.local_model)?))
        }
        _ => Ok(std::sync::Arc::new(EmbeddingEngine::new(config, rt)?)),
    }
}

/// 本地嵌入器：基于 fastembed（ONNX Runtime）在本地生成向量，
/// 免 API key、无网络依赖（模型文件由 fastembed 首次使用时下载并缓存）。
/// 与 EmbeddingEngine（远程 API）共享 Embedder trait——调用方无感知切换。
pub struct LocalEmbedder {
    model: fastembed::EmbeddingModel,
}

impl LocalEmbedder {
    /// 按模型名构造本地嵌入器；未知模型名返回错误（不静默兜底）。
    pub fn new(model: &str) -> Result<Self> {
        let model = match model {
            "bge-small-zh-v1.5" => fastembed::EmbeddingModel::BGESmallZHV15,
            "bge-small-en-v1.5" => fastembed::EmbeddingModel::BGESmallENV15,
            "bge-m3" => fastembed::EmbeddingModel::BGEM3,
            "multilingual-e5-small" => fastembed::EmbeddingModel::MultilingualE5Small,
            _ => anyhow::bail!("不支持的本地嵌入模型: {model}（可选：bge-small-zh-v1.5/bge-small-en-v1.5/bge-m3/multilingual-e5-small）"),
        };
        Ok(Self { model })
    }
}

impl LocalEmbedder {
    /// 批次嵌入：fastembed 同步 API（无 tokio 依赖），维度不足 0 校验。
    /// 与 EmbeddingEngine::embed_batch 同契约：返回同序 Vec<Vec<f32>>。
    fn embed_batch_sync(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // TextInitOptions::new 按模型名构造；cache_dir 用系统默认缓存目录
        // （fastembed get_cache_dir：Linux/macOS ~/.cache/fastembed、Windows %LOCALAPPDATA%/fastembed）
        let options = fastembed::TextInitOptions::new(self.model.clone());
        let mut model = fastembed::TextEmbedding::try_new(options)
            .with_context(|| "初始化本地嵌入模型失败（首次运行需联网下载模型）")?;
        // embed 需要 &mut self（ONNX session 内部状态）；batch_size=None 全量嵌入
        let embeddings = model
            .embed(texts, None)
            .with_context(|| "本地嵌入失败")?;
        Ok(embeddings)
    }
}

impl Embedder for LocalEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_batch_sync(&[text.to_string()])?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("本地嵌入返回空结果"))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_batch_sync(texts)
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
