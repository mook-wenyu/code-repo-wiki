use std::sync::Arc;
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
    /// 并发信号量（P2-8）：embed_batch 分批发往 API 的整批并发上限（默认 4）
    semaphore: Arc<tokio::sync::Semaphore>,
    /// 批内并发 HTTP 请求数（batch_concurrency，默认 1）——单个 embed_batch
    /// 内同时发送的请求数。默认 1 串行批内请求，避免打满百炼 TPM 吞吐限流。
    batch_concurrency: usize,
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
            semaphore: Arc::new(tokio::sync::Semaphore::new({
                let mc = config.max_concurrency.unwrap_or(4);
                anyhow::ensure!(mc > 0, "max_concurrency 必须为正整数（当前 0）");
                mc as usize
            })),
            // batch_concurrency：批内并发（默认 1）。0 会让 buffer_unordered(0)
            // 直接 panic，构造期拦截（与 max_concurrency 同款守卫）。
            batch_concurrency: {
                let bc = config.batch_concurrency.unwrap_or(1);
                anyhow::ensure!(bc > 0, "batch_concurrency 必须为正整数（当前 0）");
                bc as usize
            },
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

        // audit-gen-08：单条预截断——embedding 模型输入有 token 上限
        // （text-embedding 系 8191 token），超长单条（如大函数体）会被
        // API 拒收或静默截断。用字符数做保守代理：中文 1 字≈1 token、
        // 英文约 4 字符≈1 token，8000 字符对两种语言都不超模型上下文；
        // 截断时显式告警（不静默丢弃语义）。
        const EMBED_MAX_INPUT_CHARS: usize = 8000;
        let mut truncated = 0usize;
        let prepared: Vec<String> = texts
            .iter()
            .map(|t| {
                if t.chars().count() > EMBED_MAX_INPUT_CHARS {
                    truncated += 1;
                    t.chars().take(EMBED_MAX_INPUT_CHARS).collect()
                } else {
                    t.clone()
                }
            })
            .collect();
        if truncated > 0 {
            tracing::warn!(
                "{} 条嵌入文本超过 {} 字符上限，已截断（防 API 拒收/静默截断）",
                truncated,
                EMBED_MAX_INPUT_CHARS
            );
        }
        let texts: Vec<&str> = prepared.iter().map(|s| s.as_str()).collect();

        // P2-8：并发信号量——整批嵌入占一个许可（批次间并发属 T01-e 范围）
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;

        // P2-13：批次间并发——embedding 请求是纯网络 IO（无共享状态），
        // 串行等待每批 RTT 让大仓批量嵌入分钟级阻塞。buffer_unordered 并发
        // 发送、按 chunk 索引收集，保持结果顺序与串行一致。
        //
        // 并发度取 batch_concurrency（默认 1）而非硬编码 4：阿里百炼
        // qwen3-text-embedding 默认 TPM=1,000,000/分钟，429 insufficient_quota
        // 是 TPS/TPM 每分钟吞吐限流（非资金配额）。批内 4 路并发 × 每请求 20 条
        // × 每条最多 8000 字符，大仓一次嵌入轻松打满 1M TPM；重试退避封顶 8s
        // 仍落同一分钟窗口 → 持续 429。默认 1 把单批吞吐压到 TPM 之下，整批
        // 并发仍由 max_concurrency 信号量控制；需要提速时手动调大
        // batch_concurrency（超限由 retry_with_backoff 的 429 退避兜底）。
        let chunks: Vec<&[&str]> = texts
            .chunks(crate::config::schema::EMBED_BATCH_SIZE)
            .collect();
        let mut results: Vec<Option<Result<Vec<Vec<f32>>>>> =
            (0..chunks.len()).map(|_| None).collect();
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
        .buffer_unordered(self.batch_concurrency);

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
                    let arr = item["embedding"].as_array().context("嵌入向量缺失")?;
                    // B6：元素必须全为数字——filter_map 静默丢弃非数字元素会
                    // 让向量降维而不报错（同批一致变短时维度校验也捕获不到），
                    // 模型输出异常必须显式失败而非产出残缺向量
                    arr.iter()
                        .map(|v| {
                            v.as_f64().map(|f| f as f32).with_context(
                                || "嵌入向量包含非数字元素（模型输出异常，拒绝静默丢弃）",
                            )
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
        self.rt.block_on(EmbeddingEngine::embed_batch(self, texts))
    }

    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
        EmbeddingEngine::cosine_similarity(a, b) as f64
    }
}

/// 按配置 provider 构造嵌入器（统一入口，替代调用点直接 new EmbeddingEngine）：
/// - Remote/Mock：HTTP 通道（EmbeddingEngine；Mock 为测试模板兼容，请求指向 mock server）
///
/// 本地 ONNX 推理路径已删除（v0.7.2，用户明确要求纯远程 API 嵌入）：
/// 无论 provider 为何值，一律走远程 EmbeddingEngine。provider 枚举只保留
/// Remote/Mock 两个变体，这里无需再按 provider 分支。
pub fn build_embedder(
    config: &EmbedSection,
    rt: tokio::runtime::Handle,
) -> Result<std::sync::Arc<dyn crate::analysis::feature::Embedder>> {
    Ok(std::sync::Arc::new(EmbeddingEngine::new(config, rt)?))
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

    /// max_concurrency=0 是配置错误：Semaphore::new(0) 永久挂起（T04b）
    #[test]
    fn test_embedding_engine_rejects_zero_concurrency() {
        let cfg = EmbedSection {
            max_concurrency: Some(0),
            ..Default::default()
        };
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let err = EmbeddingEngine::new(&cfg, handle)
            .err()
            .expect("max_concurrency=0 应被构造器拒绝")
            .to_string();
        assert!(
            err.contains("必须为正整数"),
            "错误信息应引导配置修正: {err}"
        );
    }

    /// batch_concurrency=0 是配置错误：buffer_unordered(0) 直接 panic（futures 约束），
    /// 构造期拦截（与 max_concurrency 同款守卫）。
    #[test]
    fn test_embedding_engine_rejects_zero_batch_concurrency() {
        let cfg = EmbedSection {
            batch_concurrency: Some(0),
            ..Default::default()
        };
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let err = EmbeddingEngine::new(&cfg, handle)
            .err()
            .expect("batch_concurrency=0 应被构造器拒绝")
            .to_string();
        assert!(
            err.contains("batch_concurrency"),
            "错误信息应点名 batch_concurrency: {err}"
        );
    }

    /// 批内并发默认 1：不配置 batch_concurrency 时引擎回落保守串行，
    /// 避免大仓一次嵌入打满百炼 TPM 吞吐限流（429 insufficient_quota）。
    #[test]
    fn test_embedding_engine_default_batch_concurrency_is_one() {
        let cfg = EmbedSection::default();
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let engine = EmbeddingEngine::new(&cfg, handle).expect("默认配置应可构造");
        assert_eq!(engine.batch_concurrency, 1);
    }

    /// 显式配置 batch_concurrency 生效：engine 按配置值约束批内并发。
    #[test]
    fn test_embedding_engine_reads_batch_concurrency() {
        let cfg = EmbedSection {
            batch_concurrency: Some(3),
            ..Default::default()
        };
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let engine = EmbeddingEngine::new(&cfg, handle).expect("batch_concurrency=3 应可构造");
        assert_eq!(engine.batch_concurrency, 3);
    }
}
