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
    /// API 根地址（含 /v1 前缀，与 OpenAiProvider 语义一致）；默认官方地址。
    /// 可自定义以接入网关/本地代理，同时使请求构建可被测试。
    base_url: String,
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
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());

        let client = Client::builder()
            .timeout(Duration::from_secs(180))
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
                .post(format!("{}/messages", self.base_url))
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
    use crate::config::schema::{LlmProviderType, LlmSection};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ============ 本地 mock HTTP server ============
    // 用 std 线程 + std::net 起阻塞式 mock，避免依赖 tokio net 特性，
    // 与 reqwest 的异步请求天然解耦（无 runtime 饥饿/死锁问题）。

    /// 捕获的 HTTP 请求（请求路径、请求头、请求体）
    struct MockRequest {
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// mock 服务器响应
    struct MockResponse {
        status: u16,
        body: String,
    }

    /// 缓冲区中是否已出现完整请求头（含 \r\n\r\n 分隔符，其后可能还有请求体）
    fn header_complete(buf: &[u8]) -> bool {
        buf.windows(4).any(|w| w == b"\r\n\r\n")
    }

    /// 读取一个完整 HTTP 请求：请求行 + 请求头 + Content-Length 指定的请求体
    fn read_request(stream: &mut TcpStream) -> MockRequest {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        while !header_complete(&buf) {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let path = lines.next().unwrap_or("").split_whitespace().nth(1).unwrap_or("").to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let content_length = headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        // 头部结束标记 \r\n\r\n 之后才是请求体（head_end 指向标记起始，
        // 体从 head_end + 4 开始；此前漏加 4 导致 body 前缀残留 \r\n\r\n）
        const HEADER_SEP: usize = 4;
        while buf.len() < head_end + HEADER_SEP + content_length {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let body =
            String::from_utf8_lossy(&buf[head_end + HEADER_SEP..head_end + HEADER_SEP + content_length])
                .to_string();
        MockRequest { path, headers, body }
    }

    /// 启动本地 mock HTTP server：每连接处理一个请求，由 handler 生成响应。
    /// 响应带 Connection: close，迫使 reqwest 每次请求都新建连接。
    /// 返回形如 http://127.0.0.1:<port> 的 base_url。
    fn spawn_mock_server(
        handler: impl Fn(MockRequest) -> MockResponse + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let req = read_request(&mut stream);
                    let resp = handler(req);
                    let reason = if resp.status == 200 { "OK" } else { "Error" };
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp.status, reason, resp.body.len(), resp.body
                    );
                    let _ = stream.write_all(raw.as_bytes());
                });
            }
        });
        base_url
    }

    /// 构造指向本地 mock 的 OpenAI 配置（base_url 带 /v1 前缀，与生产默认一致）
    fn openai_config(base_url: &str) -> LlmSection {
        LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "gpt-test".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "OPENAI_API_KEY".into(),
            max_concurrent: 4,
            max_tokens: Some(128),
            temperature: Some(0.5),
        }
    }

    // ============ 原有测试 ============

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

    // ============ 新增：OpenAI 请求构建、SSE 流式解析与重试 ============

    #[tokio::test]
    async fn test_openai_request_builds_correct_payload() {
        // mock 服务器：捕获请求并返回固定 choices[0].message.content
        let captured = Arc::new(Mutex::new(None::<MockRequest>));
        let captured_server = captured.clone();
        let base_url = spawn_mock_server(move |req| {
            *captured_server.lock().unwrap() = Some(req);
            MockResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"你好，这是 mock 回复"}}]}"#.into(),
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url)).unwrap();
        let messages = vec![Message::system("你是测试助手"), Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        // 回复来自 mock 返回的 choices[0].message.content
        assert_eq!(reply, "你好，这是 mock 回复");

        // 请求路径：base_url + /chat/completions
        let req = captured.lock().unwrap().take().expect("应收到一次请求");

        assert_eq!(req.path, "/v1/chat/completions");
        // Authorization: Bearer <api_key>
        let auth = req.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .expect("应携带 Authorization 头");
        assert_eq!(auth.1, "Bearer test-key");
        // JSON body：model / messages（含 system 角色）/ 可选参数
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是测试助手");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["max_tokens"].as_u64(), Some(128));
        assert_eq!(body["temperature"].as_f64(), Some(0.5));
    }

    #[tokio::test]
    async fn test_openai_stream_parses_sse() {
        // mock 返回 SSE 流：两条 delta 增量 + [DONE] 结束标记
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: sse.to_string(),
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url)).unwrap();
        let messages = vec![Message::user("你好")];
        let chunks = provider.complete_stream(&messages).await.unwrap();

        // 增量按到达顺序拼接
        assert_eq!(chunks, vec!["你", "好"]);
        assert_eq!(chunks.join(""), "你好");
    }

    #[tokio::test]
    async fn test_retry_on_server_error() {
        // 第一次返回 500、第二次返回 200；重试退避 500ms×2^(n-1)，本测试约耗时 0.5s
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            let n = attempts_server.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                MockResponse { status: 500, body: "internal error".into() }
            } else {
                MockResponse { status: 200, body: r#"{"choices":[{"message":{"content":"重试成功"}}]}"#.into() }
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url)).unwrap();
        let messages = vec![Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        assert_eq!(reply, "重试成功");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_anthropic_provider_construction() {
        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: None,
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            max_concurrent: 4,
            max_tokens: None,
            temperature: None,
        };
        let provider = AnthropicProvider::new(&config).unwrap();
        assert_eq!(provider.model_name(), "claude-test");
        assert_eq!(provider.call_count(), 0);
    }

    /// Anthropic 请求构建：base_url 可配置后（与 OpenAiProvider 对齐），
    /// 用本地 mock 断言 x-api-key / anthropic-version 头与请求体
    /// （system 消息分离到顶层字段、非 system 消息进 messages、max_tokens 默认 4096）
    #[tokio::test]
    async fn test_anthropic_request_builds_correct_payload() {
        let captured = Arc::new(Mutex::new(None::<MockRequest>));
        let captured_server = captured.clone();
        let base_url = spawn_mock_server(move |req| {
            *captured_server.lock().unwrap() = Some(req);
            MockResponse {
                status: 200,
                body: r#"{"content":[{"type":"text","text":"claude 回复"}]}"#.into(),
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: Some(base_url),
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            max_concurrent: 4,
            max_tokens: None,
            temperature: None,
        };
        let provider = AnthropicProvider::new(&config).unwrap();
        let messages = vec![
            Message::system("你是助手"),
            Message::user("你好"),
            Message::assistant("在的"),
        ];
        let reply = provider.complete(&messages).await.unwrap();
        assert_eq!(reply, "claude 回复");

        let req = captured.lock().unwrap().take().expect("应收到一次请求");
        assert_eq!(req.path, "/messages");
        let api_key_header = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-api-key"))
            .expect("应携带 x-api-key 头");
        assert_eq!(api_key_header.1, "sk-ant-test");
        let version_header = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
            .expect("应携带 anthropic-version 头");
        assert_eq!(version_header.1, "2023-06-01");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"].as_u64(), Some(4096), "max_tokens 未配置时默认 4096");
        // system 消息分离到顶层 system 字段
        assert_eq!(body["system"], "你是助手");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "非 system 消息才进 messages");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "你好");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "在的");
    }
}
