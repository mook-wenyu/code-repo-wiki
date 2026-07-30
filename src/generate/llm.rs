use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;

use crate::config::schema::LlmSection;

/// LLM 对话消息
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// LLM Provider 抽象 trait
///
/// Rust 2024 支持在 trait 中使用 async fn，无需 async-trait crate。
/// 注意：async fn in trait 不满足 dyn 安全性，请通过泛型或 Provider 枚举使用。
#[allow(async_fn_in_trait)]
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;
    async fn complete(&self, messages: &[Message]) -> Result<String>;
    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        let _ = messages;
        Err(anyhow::anyhow!("streaming not supported"))
    }
    /// 返回已完成的 LLM 调用次数
    fn call_count(&self) -> usize {
        0
    }
}

/// 统一的 Provider 枚举，包装所有 Provider 实现
///
/// 通过此枚举可以在需要动态分发时避免 dyn trait 的限制。
pub enum Provider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Mock(MockProvider),
}

impl LlmProvider for Provider {
    fn model_name(&self) -> &str {
        match self {
            Provider::OpenAi(p) => p.model_name(),
            Provider::Anthropic(p) => p.model_name(),
            Provider::Mock(p) => p.model_name(),
        }
    }

    async fn complete(&self, messages: &[Message]) -> Result<String> {
        match self {
            Provider::OpenAi(p) => p.complete(messages).await,
            Provider::Anthropic(p) => p.complete(messages).await,
            Provider::Mock(p) => p.complete(messages).await,
        }
    }

    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        match self {
            Provider::OpenAi(p) => p.complete_stream(messages).await,
            Provider::Anthropic(p) => p.complete_stream(messages).await,
            Provider::Mock(p) => p.complete_stream(messages).await,
        }
    }

    fn call_count(&self) -> usize {
        match self {
            Provider::OpenAi(p) => p.call_count(),
            Provider::Anthropic(p) => p.call_count(),
            Provider::Mock(p) => p.call_count(),
        }
    }
}

/// OpenAI 兼容 API 的 LLM Provider 实现
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    max_retries: u32,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    call_count: std::sync::atomic::AtomicUsize,
}

impl OpenAiProvider {
    /// 从配置创建 OpenAI Provider
    ///
    /// 优先使用 api_key 字段，其次从环境变量读取。支持自定义 base_url。
    pub fn new(config: &LlmSection) -> Result<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var(&config.api_key_env).ok())
            .with_context(|| format!("LLM API Key 未设置（api_key 为空且环境变量 {} 未定义）", config.api_key_env))?;
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            base_url,
            max_retries: 3,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl LlmProvider for OpenAiProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let url = format!("{}/chat/completions", self.base_url);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({"role": m.role, "content": m.content})
            }).collect::<Vec<_>>(),
            "stream": true,
        });
        if let Some(maxt) = self.max_tokens {
            body["max_tokens"] = serde_json::json!(maxt);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("API 返回错误 ({}): {}", status, text);
        }

        let mut chunks = Vec::new();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                if let Some(json_str) = line.strip_prefix("data: ")
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
                        && let Some(content) = val["choices"][0]["delta"]["content"].as_str() {
                            chunks.push(content.to_string());
                        }
            }
        }

        Ok(chunks)
    }

    async fn complete(&self, messages: &[Message]) -> Result<String> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let url = format!("{}/chat/completions", self.base_url);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({"role": m.role, "content": m.content})
            }).collect::<Vec<_>>(),
        });
        // 可选参数（仅当配置中指定时传入）
        if let Some(maxt) = self.max_tokens {
            body["max_tokens"] = serde_json::json!(maxt);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match attempt {
                0 => {}
                _ => {
                    let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    tokio::time::sleep(delay).await;
                }
            }

            match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        last_error = Some(anyhow::anyhow!(
                            "API 返回错误 ({}): {}",
                            status,
                            text
                        ));
                        continue;
                    }

                    let data: serde_json::Value = resp
                        .json()
                        .await
                        .context("解析 API 响应 JSON 失败")?;

                    let content = data["choices"][0]["message"]["content"]
                        .as_str()
                        .map(|s| s.to_string())
                        .with_context(|| {
                            format!(
                                "API 响应缺少 choices[0].message.content: {}",
                                serde_json::to_string(&data).unwrap_or_default()
                            )
                        })?;

                    return Ok(content);
                }
                Err(e) => {
                    last_error = Some(anyhow::anyhow!("请求失败: {}", e));
                }
            }
        }

        Err(anyhow::anyhow!(
            "LLM API 调用重试 {} 次后全部失败: {:?}",
            self.max_retries,
            last_error
        ))
    }

    fn call_count(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Anthropic Claude API LLM Provider 实现
///
/// 通过 Anthropic Messages API 调用 Claude 系列模型。
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    max_retries: u32,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    call_count: std::sync::atomic::AtomicUsize,
}

impl AnthropicProvider {
    /// 从配置创建 Anthropic Provider
    ///
    /// 优先使用 api_key 字段，其次从环境变量读取。
    pub fn new(config: &LlmSection) -> Result<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var(&config.api_key_env).ok())
            .with_context(|| format!("Anthropic API Key 未设置（api_key 为空且环境变量 {} 未定义）", config.api_key_env))?;

        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            max_retries: 3,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl LlmProvider for AnthropicProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, messages: &[Message]) -> Result<String> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // 分离 system 消息和用户/助手消息
        let system = messages.iter().find(|m| m.role == "system").map(|m| &m.content);
        let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != "system").collect();

        // 将非 system 消息转换为 Anthropic 格式
        let anthropic_messages: Vec<serde_json::Value> = non_system
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": if m.role == "user" { "user" } else { "assistant" },
                    "content": m.content
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens.unwrap_or(4096),
            "messages": anthropic_messages,
        });
        if let Some(s) = system {
            body["system"] = serde_json::json!(s);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            match self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        last_error = Some(anyhow::anyhow!(
                            "Anthropic API 返回错误 ({}): {}",
                            status,
                            text
                        ));
                        continue;
                    }

                    let data: serde_json::Value = resp
                        .json()
                        .await
                        .context("解析 Anthropic API 响应 JSON 失败")?;

                    let content = data["content"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|block| block["text"].as_str())
                        .map(|s| s.to_string())
                        .with_context(|| {
                            format!(
                                "API 响应缺少 content[0].text: {}",
                                serde_json::to_string(&data).unwrap_or_default()
                            )
                        })?;

                    return Ok(content);
                }
                Err(e) => {
                    last_error = Some(anyhow::anyhow!("请求失败: {}", e));
                }
            }
        }

        Err(anyhow::anyhow!(
            "Anthropic API 调用重试 {} 次后全部失败: {:?}",
            self.max_retries,
            last_error
        ))
    }

    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let system = messages.iter().find(|m| m.role == "system").map(|m| &m.content);
        let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != "system").collect();

        let anthropic_messages: Vec<serde_json::Value> = non_system
            .iter()
            .map(|m| {
                serde_json::json!({"role": if m.role == "user" { "user" } else { "assistant" }, "content": m.content})
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens.unwrap_or(4096),
            "messages": anthropic_messages,
            "stream": true,
        });
        if let Some(s) = system {
            body["system"] = serde_json::json!(s);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 返回错误 ({}): {}", status, text);
        }

        let mut chunks = Vec::new();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();
                if line.is_empty() {
                    continue;
                }
                if let Some(json_str) = line.strip_prefix("data: ")
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
                        && val["type"] == "content_block_delta"
                            && let Some(text) = val["delta"]["text"].as_str() {
                                chunks.push(text.to_string());
                            }
            }
        }

        Ok(chunks)
    }

    fn call_count(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Mock LLM Provider（用于测试和离线模式）
///
/// 不发起真实网络请求，返回固定的模拟响应。
pub struct MockProvider {
    call_count: std::sync::atomic::AtomicUsize,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmProvider for MockProvider {
    fn model_name(&self) -> &str {
        "mock-model"
    }

    async fn complete(&self, _messages: &[Message]) -> Result<String> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(
            r#"{"summary": "这是 Mock Provider 生成的模拟摘要", "key_entities": []}"#
                .to_string(),
        )
    }

    async fn complete_stream(&self, _messages: &[Message]) -> Result<Vec<String>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(vec!["模拟流式响应 chunk".to_string()])
    }

    fn call_count(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider() {
        let provider = MockProvider::new();
        assert_eq!(provider.model_name(), "mock-model");

        let messages = vec![Message::user("测试消息")];
        let result = provider.complete(&messages).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("模拟摘要"));
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn test_message_constructors() {
        let sys = Message::system("你好");
        assert_eq!(sys.role, "system");
        assert_eq!(sys.content, "你好");

        let user = Message::user("测试");
        assert_eq!(user.role, "user");

        let asst = Message::assistant("回复");
        assert_eq!(asst.role, "assistant");
    }
}
