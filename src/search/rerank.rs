//! 重排器（v36 B1/B2）：hybrid 融合后的交叉编码器精排。
//!
//! 检索管线标准形态 = bi-encoder 召回（text BM25 + semantic 向量）
//! + RRF 融合 + cross-encoder 精排。RRF 只用排名信息，跨引擎的
//! 相关性强度被抹平；重排器直接计算「查询 × 候选文档」的相关性
//! 分数，对融合后的 top-K 候选做最终排序。
//!
//! 实现走百炼兼容端点（/rerank，OpenAI 兼容的 rerank 协议）：
//! 与 embed 同栈同 Key（BAILIAN_API_KEY），用户配置 embed 后即可
//! 直接使用。无 Key / 调用失败时由调用方跳过重排（保持原顺序并
//! 告警）——重排是精排增强，不降低检索可用性。

use anyhow::{Context, Result};

use crate::config::schema::RerankSection;

/// 重排器：对候选文档按「与查询的相关性」降序重排
pub struct Reranker {
    client: reqwest::Client,
    config: RerankSection,
}

impl Reranker {
    /// 从配置创建重排器。构造不校验 Key（延迟到 rerank 调用时）：
    /// 无 Key 环境可正常构造，调用失败由调用方跳过重排。
    pub fn new(config: &RerankSection, _rt: tokio::runtime::Handle) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("创建重排 HTTP 客户端失败")?;
        Ok(Self {
            client,
            config: config.clone(),
        })
    }

    /// 解析 API Key，优先级：api_key > 环境变量 > 报错
    fn resolve_api_key(&self) -> Result<String> {
        self.config
            .api_key
            .clone()
            .or_else(|| std::env::var(&self.config.api_key_env).ok())
            .context(format!(
                "重排 API Key 未设置（api_key 为空且环境变量 {} 未定义），跳过重排",
                self.config.api_key_env
            ))
    }

    /// 重排：返回按相关性降序的 documents 下标序列。
    ///
    /// `top_n` 限制服务端返回的候选数（服务端只精排 top_n 个，控制
    /// 成本与延迟）；调用方取返回下标即可得重排后的顺序。
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<usize>> {
        let api_key = self.resolve_api_key()?;
        let base_url = self
            .config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let url = format!("{}/rerank", base_url.trim_end_matches('/'));

        let body = serde_json::json!({
            "model": self.config.model,
            "query": query,
            "documents": documents,
            // 服务端 top_n：只精排前 top_n 个候选（候选数>top_n 时
            // 未返回的下标视为排在末尾——调用方无需感知）
            "top_n": top_n,
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .context("重排请求失败")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // 截断响应体（错误详情可能很长），保留状态码供排查
            let snippet: String = text.chars().take(200).collect();
            anyhow::bail!("重排接口返回 HTTP {status}: {snippet}");
        }
        let payload: serde_json::Value = resp.json().await.context("重排响应解析失败")?;

        // 结果数组按 relevance_score 降序（服务端契约）；缺失/畸形时
        // 报错（调用方跳过重排，不静默吞掉契约破坏）
        let results = payload
            .get("results")
            .and_then(|r| r.as_array())
            .context("重排响应缺少 results 数组")?;
        let mut pairs: Vec<(usize, f64)> = Vec::with_capacity(results.len());
        for item in results {
            let index = item
                .get("index")
                .and_then(|i| i.as_u64())
                .context("重排结果缺少 index")? as usize;
            let score = item
                .get("relevance_score")
                .and_then(|s| s.as_f64())
                .context("重排结果缺少 relevance_score")?;
            pairs.push((index, score));
        }
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(pairs.into_iter().map(|(i, _)| i).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 简易单线程 mock 服务器：按请求体 echo 一次 /rerank 响应。
    /// 只服务一个请求后退出（测试专用，行为最小化）。
    fn spawn_mock_rerank() -> (String, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);
        let key = "test-rerank-key".to_string();
        let key_for_server = key.clone();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            // 单块读取：请求（含 body）远小于 4096，一次读够；
            // 无需解析 Content-Length（测试专用 mock，行为最小化）
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            // 校验 Authorization 头携带 key（契约：bearer 传递）
            let head = String::from_utf8_lossy(&buf[..n]);
            assert!(head.contains(&format!("Bearer {}", key_for_server)), "mock 校验 key 失败");
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                {\"results\":[{\"index\":1,\"relevance_score\":0.9},\
                {\"index\":0,\"relevance_score\":0.7},\
                {\"index\":2,\"relevance_score\":0.2}]}";
            stream.write_all(response.as_bytes()).unwrap();
        });

        (base, key)
    }

    #[test]
    fn test_rerank_orders_by_relevance() {
        let (base, key) = spawn_mock_rerank();
        let config = RerankSection {
            model: "qwen3-rerank".to_string(),
            base_url: Some(base),
            api_key: Some(key),
            api_key_env: "NONE".to_string(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let reranker = Reranker::new(&config, rt.handle().clone()).unwrap();

        let docs = vec!["文档A".to_string(), "文档B".to_string(), "文档C".to_string()];
        let order = rt.block_on(reranker.rerank("查询", &docs, 3)).unwrap();
        // mock 返回 [1, 0, 2] 按分数降序——重排器必须原样传递服务端顺序
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn test_rerank_missing_key_errors() {
        let config = RerankSection {
            model: "qwen3-rerank".to_string(),
            base_url: Some("http://127.0.0.1:1".to_string()),
            api_key: None,
            api_key_env: "REPO_WIKI_NONE_EXISTENT_ENV_VAR_xyz".to_string(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let reranker = Reranker::new(&config, rt.handle().clone()).unwrap();
        let err = rt.block_on(reranker.rerank("q", &["d".to_string()], 1)).unwrap_err();
        assert!(err.to_string().contains("Key 未设置"), "缺 Key 必须报明确错误: {err}");
    }
}
