use std::time::Duration;

use anyhow::{Context, Result};
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
///
/// 契约（t09 真流式接线）：**生产路径统一走流式**——`complete` 的默认
/// 实现 = `complete_stream` 收集并拼接 chunks，实现方只需实现
/// `complete_stream` 即获得完整语义；需要自定义非流式行为的实现
/// （如 Mock 返回固定 JSON）显式覆盖 `complete`。未实现 `complete_stream`
/// 的实现调用 `complete` 会得到显式错误，不静默退化。
#[allow(async_fn_in_trait)]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message]) -> Result<String> {
        let chunks = self.complete_stream(messages).await?;
        Ok(chunks.concat())
    }
    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        let _ = messages;
        Err(anyhow::anyhow!("streaming not supported"))
    }
    /// 带输出预算上限的完整（非流式）调用。
    ///
    /// 评测裁判（rubrics / TQS 叶子判定）等长结构化输出场景必须显式
    /// 传预算：推理型模型（如 deepseek-v4-flash）的 reasoning 会消耗
    /// 输出预算，预算不足时响应可能只有 reasoning 块没有 message
    /// （实测 v22 rubrics 首跑 3+3 轮全败，max=4000 复现只有
    /// reasoning、max=8192 才出现 message）。默认实现不带预算
    /// （等价 complete），需要预算的 Provider 自行覆盖。
    async fn complete_with_budget(
        &self,
        messages: &[Message],
        _max_output_tokens: Option<u32>,
    ) -> Result<String> {
        self.complete(messages).await
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

    async fn complete_with_budget(
        &self,
        messages: &[Message],
        max_output_tokens: Option<u32>,
    ) -> Result<String> {
        match self {
            Provider::OpenAi(p) => p.complete_with_budget(messages, max_output_tokens).await,
            Provider::Anthropic(p) => p.complete_with_budget(messages, max_output_tokens).await,
            Provider::Mock(p) => p.complete_with_budget(messages, max_output_tokens).await,
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

// ============ 共享骨架：重试 + SSE 解析（OpenAI 与 Anthropic 共用） ============

/// 统一的重试上限（总尝试次数），OpenAiProvider/AnthropicProvider 均从该常量取值
pub(crate) const MAX_RETRIES: u32 = 3;

/// 指数退避：500ms * 2^attempt + 随机抖动 0-250ms。
/// 抖动用系统时钟纳秒取模实现，避免为单点功能引入 rand 依赖。
fn backoff_delay(attempt: u32) -> Duration {
    let base = 500u64 * 2u64.pow(attempt);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % 251)
        .unwrap_or(0);
    Duration::from_millis(base + jitter)
}

/// 可重试的 HTTP 状态码白名单：429 限流 + 5xx 服务端错误；
/// 其余 4xx 业务错误（400/401/403/404/422 等）不在白名单内，立即失败。
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// 统一重试骨架：可重试错误（429/5xx/超时/连接失败）按指数退避重试，
/// 其余 4xx 立即失败。send_fn 每轮重新构建请求（请求构建仍是协议差异点，留在调用方）。
///
/// 可观测性（v16 B 组）：每次可重试失败记录 attempt/原因与下次退避延迟，
/// 错误响应体保留（截断 2000 字符防日志爆炸）；重试耗尽时汇总 last_error。
/// 此前本函数零日志——生产排查重试风暴只能看到最终错误，过程全黑。
pub(crate) async fn retry_with_backoff<F, Fut>(
    max_retries: u32,
    send_fn: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send,
{
    let mut last_error = None;
    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay = backoff_delay(attempt - 1);
            tracing::info!(
                "LLM 请求重试（第 {}/{} 次），退避 {}ms",
                attempt + 1,
                max_retries,
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
        }
        match send_fn().await {
            Ok(resp) if is_retryable_status(resp.status()) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "LLM API 返回可重试状态 {}（第 {} 次尝试）: {}",
                    status,
                    attempt + 1,
                    text.chars().take(2000).collect::<String>()
                );
                last_error = Some(anyhow::anyhow!("API 返回错误 ({}): {}", status, text));
            }
            Ok(resp) => return Ok(resp),
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!(
                    "LLM 请求超时/连接失败（第 {} 次尝试，将重试）: {}",
                    attempt + 1,
                    e
                );
                last_error = Some(anyhow::anyhow!("请求失败: {}", e));
            }
            Err(e) => return Err(anyhow::anyhow!("请求失败: {}", e)),
        }
    }
    tracing::error!("LLM API 调用重试 {} 次后全部失败: {:?}", max_retries, last_error);
    Err(anyhow::anyhow!(
        "LLM API 调用重试 {} 次后全部失败: {:?}",
        max_retries,
        last_error
    ))
}

/// 共享 SSE 行解析：按 \n 切分、跳过空行、剥离行前缀、解析 JSON，
/// 文本增量交给 extract 提取。OpenAI 与 Anthropic 的 SSE 行均为 `data: ` 前缀，
/// 真实差异在 JSON 字段路径（OpenAI: choices[0].delta.content；
/// Anthropic: type=content_block_delta 事件的 delta.text），故用提取闭包参数化。
/// `data: [DONE]` 等非 JSON 行解析失败自然跳过。
/// 仅测试使用（流式路径已内联同样的按行解析，此处保留整块解析供 mock 断言）
#[cfg(test)]
fn parse_sse_stream(
    bytes: &[u8],
    line_prefix: &str,
    extract: impl Fn(&serde_json::Value) -> Option<String>,
) -> Vec<String> {
    let mut chunks = Vec::new();
    for line in String::from_utf8_lossy(bytes).split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(json_str) = line.strip_prefix(line_prefix) else { continue };
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(text) = extract(&val)
        {
            chunks.push(text);
        }
    }
    chunks
}

/// 消费流式响应并走共享 SSE 解析（逐 chunk 流式，t09）
///
/// 原实现 `resp.bytes().await` 全量收包后统一解析：长生成（真实 LLM
/// 10min 超时前科）受 client 总超时（120s）**整体截断**，重试又从头
/// 再来——进度全部丢失。流式方案：`bytes_stream` 逐块读取，每次读块
/// 用**空闲超时**保护（60s 无数据才判超时）：只要模型还在产出就不会
/// 超时，真正停止产出才失败，长生成不再受总超时限制。
///
/// SSE 行解析复用 parse_sse_stream 的语义（`data: ` 前缀 + 提取闭包），
/// 逐行增量处理；跨 chunk 的残行保留到下一块。
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

async fn collect_sse(
    resp: reqwest::Response,
    line_prefix: &str,
    extract: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<Vec<String>> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunks = Vec::new();

    // v16 B 组：流式消费的可观测性——长生成场景（分钟级）若无日志，
    // 用户无法区分"模型还在产出"与"卡死无响应"。记录流开始与结束
    // 统计，空闲超时单独 warn（含已收 chunk 数，便于判断进度丢失量）。
    tracing::info!("SSE 流开始消费（空闲超时保护 {}s）", SSE_IDLE_TIMEOUT.as_secs());
    loop {
        let item = match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(item))) => item,
            Ok(Some(Err(e))) => return Err(e.into()),
            // 流正常结束：处理尾部残行后返回
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(
                    "SSE 流读取空闲超时（{}s 无数据，已收 {} 个 chunk，模型可能已停止产出）",
                    SSE_IDLE_TIMEOUT.as_secs(),
                    chunks.len()
                );
                anyhow::bail!(
                    "SSE 流读取空闲超时（{}s 无数据，模型可能已停止产出）",
                    SSE_IDLE_TIMEOUT.as_secs()
                )
            }
        };
        buf.extend_from_slice(&item);
        // 按行切分处理，保留未完成的尾部残行
        let mut consumed = 0usize;
        for (idx, b) in buf.iter().enumerate() {
            if *b != b'\n' {
                continue;
            }
            let line = String::from_utf8_lossy(&buf[consumed..idx]);
            let line = line.trim();
            if !line.is_empty()
                && let Some(json_str) = line.strip_prefix(line_prefix)
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
                && let Some(text) = extract(&val)
            {
                chunks.push(text);
            }
            consumed = idx + 1;
        }
        buf.drain(..consumed);
    }
    // 尾部残行（流结束时最后一行可能无换行符）
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        if let Some(json_str) = line.trim().strip_prefix(line_prefix)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(text) = extract(&val)
        {
            chunks.push(text);
        }
    }
    tracing::info!(
        "SSE 流消费完成: {} 个 chunk, {} 字符",
        chunks.len(),
        chunks.iter().map(|c| c.len()).sum::<usize>()
    );
    Ok(chunks)
}

/// OpenAI 协议形态（v17 t02 拆分：协议按 provider 类型显式绑定）
///
/// - `Responses`：OpenAI **Responses API**（POST /responses；DeepSeek 等
///   支持 Responses 的服务经 base_url 接入）
/// - `Chat`：**chat/completions**（OpenAI 兼容端点：阿里云/自建等）
///
/// 拆分原因：两协议的请求体（input/instructions vs messages[]）、响应
/// 解析（output.items vs choices[]）、SSE 事件（语义化事件 vs
/// choices[].delta）差异大，且不是所有兼容端点都提供 /responses。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiProtocol {
    Responses,
    Chat,
}

/// OpenAI 兼容 API 的 LLM Provider 实现（v17 t02 起支持双协议）
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    /// 协议形态（构造时按 provider 类型绑定，见 create_provider）
    protocol: OpenAiProtocol,
    max_retries: u32,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    call_count: std::sync::atomic::AtomicUsize,
}

impl OpenAiProvider {
    /// 从配置创建 OpenAI Provider（v17 起按 provider 类型绑定协议）
    ///
    /// 优先使用 api_key 字段，其次从环境变量读取。支持自定义 base_url。
    pub fn new(config: &LlmSection, protocol: OpenAiProtocol) -> Result<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var(&config.api_key_env).ok())
            .with_context(|| {
                // v17 t04：错误消息附加可操作引导——新用户只需设置环境变量
                // 或编辑配置文件的 [llm] 段，不猜
                format!(
                    "LLM API Key 未设置（api_key 为空且环境变量 {} 未定义）。请设置环境变量 {}，或编辑配置文件的 [llm] 段填入 api_key",
                    config.api_key_env, config.api_key_env
                )
            })?;
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        let client = Client::builder()
            // 不设总超时（t09 真流式）：长生成由流式路径的 SSE_IDLE_TIMEOUT
            // 空闲超时保护——模型持续产出即不超时，真正停止产出 60s 才失败；
            // 总超时会在长生成中途整体截断已产出内容（v12 实测 10min 超时前科）
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            base_url,
            protocol,
            max_retries: MAX_RETRIES,
            max_tokens: None,
            temperature: None,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl OpenAiProvider {
    /// 构建 chat/completions 请求体；stream 决定是否追加流式标记
    ///
    /// max_tokens_override 为显式预算覆盖（v22 起构造时为 None，交模型
    /// 默认）；评测裁判的长结构化输出经 complete_with_budget 传入。
    fn build_chat_body(
        &self,
        messages: &[Message],
        stream: bool,
        max_tokens_override: Option<u32>,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({"role": m.role, "content": m.content})
            }).collect::<Vec<_>>(),
        });
        if stream {
            body["stream"] = serde_json::json!(true);
        }
        // 可选参数：显式覆盖优先，回退构造时的 max_tokens（均为 None 时省略）
        if let Some(maxt) = max_tokens_override.or(self.max_tokens) {
            body["max_tokens"] = serde_json::json!(maxt);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        body
    }

    /// 构建 Responses API 请求体（v17 B4）
    ///
    /// 协议差异（t01 查证，OpenAI 官方迁移指南）：请求从 messages[]
    /// 改为 `input`（typed items 数组），system 消息分离到顶层
    /// `instructions` 字段；token 上限参数名从 max_tokens 改为
    /// `max_output_tokens`（DeepSeek 对不支持的参数静默忽略——参数名
    /// 写错会静默失效，必须按协议用正确名称）。
    fn build_responses_body(
        &self,
        messages: &[Message],
        stream: bool,
        max_output_tokens_override: Option<u32>,
    ) -> serde_json::Value {
        // system 消息 → 顶层 instructions；user/assistant → input items
        let system = messages.iter().find(|m| m.role == "system").map(|m| &m.content);
        let input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| {
                serde_json::json!({
                    "role": if m.role == "user" { "user" } else { "assistant" },
                    "content": serde_json::json!([{ "type": "input_text", "text": m.content }]),
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "model": self.model,
            "input": input,
        });
        if let Some(s) = system {
            body["instructions"] = serde_json::json!(s);
        }
        if stream {
            body["stream"] = serde_json::json!(true);
        }
        // 可选参数：显式覆盖优先，回退构造时的 max_tokens（均为 None 时省略）
        if let Some(maxt) = max_output_tokens_override.or(self.max_tokens) {
            body["max_output_tokens"] = serde_json::json!(maxt);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        body
    }
}

impl LlmProvider for OpenAiProvider {
    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.protocol {
            OpenAiProtocol::Chat => self.chat_complete_stream(messages, None).await,
            OpenAiProtocol::Responses => self.responses_complete_stream(messages, None).await,
        }
    }

    /// 带输出预算的完整调用（评测裁判用）：流式路径 + 显式预算
    /// （推理型模型 reasoning 吞预算，见 trait 文档）
    async fn complete_with_budget(
        &self,
        messages: &[Message],
        max_output_tokens: Option<u32>,
    ) -> Result<String> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let chunks = match self.protocol {
            OpenAiProtocol::Chat => self.chat_complete_stream(messages, max_output_tokens).await?,
            OpenAiProtocol::Responses => {
                self.responses_complete_stream(messages, max_output_tokens).await?
            }
        };
        Ok(chunks.concat())
    }

    // complete 走 trait 默认实现（complete_stream 收集拼接）——
    // 生产路径统一流式，无整读分支
    fn call_count(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl OpenAiProvider {
    /// chat/completions 协议路径（OpenAI 兼容端点；v17 起为
    /// openai-compatible provider 的协议）
    ///
    /// max_tokens_override：流式请求体中的显式预算（complete_with_budget
    /// 传入；常规路径 None）
    async fn chat_complete_stream(
        &self,
        messages: &[Message],
        max_tokens_override: Option<u32>,
    ) -> Result<Vec<String>> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_chat_body(messages, true, max_tokens_override);

        let resp = retry_with_backoff(self.max_retries, || {
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("API 返回错误 ({}): {}", status, text);
        }

        // chat/completions SSE：data: 行内 choices[0].delta.content
        collect_sse(resp, "data: ", |v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(|s| s.to_string())
        })
        .await
    }

    /// Responses API 协议路径（openai provider 的协议，v17 B4）
    ///
    /// 端点不支持信号（404/400，如服务未提供 /responses）→ 自动回退
    /// chat/completions 重发一次（t02 拍板；429/5xx 由 retry_with_backoff
    /// 处理，不触发回退——回退只针对"端点不支持"，不掩盖限流/服务端错误）。
    async fn responses_complete_stream(
        &self,
        messages: &[Message],
        max_output_tokens_override: Option<u32>,
    ) -> Result<Vec<String>> {
        let url = format!("{}/responses", self.base_url);
        let body = self.build_responses_body(messages, true, max_output_tokens_override);

        let resp = retry_with_backoff(self.max_retries, || {
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
        })
        .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::BAD_REQUEST
        {
            // 端点不支持（404）/参数被拒（400）：服务未实现 Responses 协议，
            // 回退 chat/completions 重发（仅一次——chat 失败按既有错误传播）
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "Responses 端点不支持 ({}: {})，自动回退 chat/completions 重发",
                status,
                text.chars().take(500).collect::<String>()
            );
            return self.chat_complete_stream(messages, max_output_tokens_override).await;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("API 返回错误 ({}): {}", status, text);
        }

        // Responses SSE：语义化事件流——data: 行内 type=response.output_text.delta
        // 事件的 delta 字段（无 [DONE] 终止符，以流结束为终止，collect_sse 兼容）
        collect_sse(resp, "data: ", |v| {
            if v["type"].as_str() == Some("response.output_text.delta") {
                v["delta"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .await
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
            .with_context(|| format!("Anthropic API Key 未设置（api_key 为空且环境变量 {} 未定义）。请设置环境变量 {}，或编辑配置文件的 [llm] 段填入 api_key", config.api_key_env, config.api_key_env))?;
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());

        let client = Client::builder()
            // 不设总超时（同 OpenAiProvider：长生成由流式路径 SSE_IDLE_TIMEOUT 保护）
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            base_url,
            max_retries: MAX_RETRIES,
            max_tokens: None,
            temperature: None,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl AnthropicProvider {
    /// 构建 messages API 请求体：system 消息分离到顶层字段、
    /// 非 system 消息进 messages、max_tokens 未配置时默认 4096；
    /// stream 决定是否追加流式标记
    fn build_messages_body(
        &self,
        messages: &[Message],
        stream: bool,
        max_tokens_override: Option<u32>,
    ) -> serde_json::Value {
        // 分离 system 消息与用户/助手消息
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
            // 显式覆盖优先，回退构造时的 max_tokens（默认 4096）
            "max_tokens": max_tokens_override.or(self.max_tokens).unwrap_or(4096),
            "messages": anthropic_messages,
        });
        if let Some(s) = system {
            body["system"] = serde_json::json!(s);
        }
        if let Some(temp) = self.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if stream {
            body["stream"] = serde_json::json!(true);
        }
        body
    }
}

impl LlmProvider for AnthropicProvider {
    async fn complete_stream(&self, messages: &[Message]) -> Result<Vec<String>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let url = format!("{}/messages", self.base_url);
        let body = self.build_messages_body(messages, true, None);

        let resp = retry_with_backoff(self.max_retries, || {
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 返回错误 ({}): {}", status, text);
        }

        // Anthropic SSE：data: 行内 type=content_block_delta 事件的 delta.text
        collect_sse(resp, "data: ", |v| {
            if v["type"] == "content_block_delta" {
                v["delta"]["text"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .await
    }

    // complete 走 trait 默认实现（complete_stream 收集拼接）——
    // 生产路径统一流式，无整读分支
    async fn complete_with_budget(
        &self,
        messages: &[Message],
        max_output_tokens: Option<u32>,
    ) -> Result<String> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let url = format!("{}/messages", self.base_url);
        let body = self.build_messages_body(messages, true, max_output_tokens);

        let resp = retry_with_backoff(self.max_retries, || {
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 返回错误 ({}): {}", status, text);
        }

        // Anthropic SSE：data: 行内 type=content_block_delta 事件的 delta.text
        let chunks = collect_sse(resp, "data: ", |v| {
            if v["type"] == "content_block_delta" {
                v["delta"]["text"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .await?;
        Ok(chunks.concat())
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
        }
    }

    // ============ 原有测试 ============

    #[tokio::test]
    async fn test_mock_provider() {
        let provider = MockProvider::new();

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
        // mock 服务器：捕获请求并返回 SSE 流（生产路径统一流式，
        // complete() 走 trait 默认实现收集流式 chunks 拼接）
        let captured = Arc::new(Mutex::new(None::<MockRequest>));
        let captured_server = captured.clone();
        let base_url = spawn_mock_server(move |req| {
            *captured_server.lock().unwrap() = Some(req);
            MockResponse {
                status: 200,
                body: r#"data: {"choices":[{"delta":{"content":"你好，这是 mock 回复"}}]}

data: [DONE]

"#
                .into(),
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::system("你是测试助手"), Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        // 回复来自流式 chunks 的拼接（delta.content）
        assert_eq!(reply, "你好，这是 mock 回复");

        // 请求路径：base_url + /chat/completions
        let req = captured.lock().unwrap().take().expect("应收到一次请求");

        assert_eq!(req.path, "/v1/chat/completions");
        // Authorization: Bearer <api_key>
        let auth = req.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .expect("应携带 Authorization 头");
        assert_eq!(auth.1, "Bearer test-key");
        // JSON body：model / messages（含 system 角色）/ stream:true（生产路径流式）/ 可选参数
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是测试助手");
        assert_eq!(body["messages"][1]["role"], "user");
        // v22 起 max_tokens/temperature 硬编码为 None（模型默认），断言不写入
        assert!(body.get("max_tokens").is_none(), "硬编码后不应写 max_tokens");
        assert!(body.get("temperature").is_none(), "硬编码后不应写 temperature");
        assert_eq!(
            body["stream"].as_bool(),
            Some(true),
            "生产路径必须请求流式响应（stream:true）"
        );
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

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let chunks = provider.complete_stream(&messages).await.unwrap();

        // 增量按到达顺序拼接
        assert_eq!(chunks, vec!["你", "好"]);
        assert_eq!(chunks.join(""), "你好");
    }

    /// A4：慢流响应（两段 SSE 之间间隔 300ms）不被总超时截断——
    /// client 不再设总超时，长生成由 60s 空闲超时保护，只要模型
    /// 持续产出就不会中途失败。响应分两次写入同一连接。
    #[tokio::test]
    async fn test_slow_stream_not_truncated_by_total_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                std::thread::spawn(move || {
                    let _req = read_request(&mut stream);
                    // 第一段立即写出，间隔 300ms 后再写第二段
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all("data: {\"choices\":[{\"delta\":{\"content\":\"第一段\"}}]}\n\n".as_bytes());
                    let _ = stream.flush();
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = stream.write_all("data: {\"choices\":[{\"delta\":{\"content\":\"第二段\"}}]}\n\n".as_bytes());
                    let _ = stream.write_all(b"data: [DONE]\n\n");
                    let _ = stream.flush();
                });
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        assert_eq!(reply, "第一段第二段", "慢流两段必须完整拼接（无总超时截断）");
    }

    #[tokio::test]
    async fn test_retry_on_server_error() {
        // 第一次返回 500、第二次返回 200；5xx 在可重试白名单内，
        // 退避 500ms×2^n + 抖动 0-250ms，本测试约耗时 0.5-0.75s
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            let n = attempts_server.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                MockResponse { status: 500, body: "internal error".into() }
            } else {
                // 成功响应为 SSE 流（流式路径的输入格式）
                MockResponse { status: 200, body: "data: {\"choices\":[{\"delta\":{\"content\":\"重试成功\"}}]}\n\ndata: [DONE]\n\n".into() }
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        assert_eq!(reply, "重试成功");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_retry_on_429() {
        // 第一次返回 429（限流）、第二次返回 200：429 在可重试白名单内，断言请求次数 = 2
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            let n = attempts_server.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                MockResponse { status: 429, body: "rate limited".into() }
            } else {
                MockResponse { status: 200, body: "data: {\"choices\":[{\"delta\":{\"content\":\"限流后成功\"}}]}\n\ndata: [DONE]\n\n".into() }
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let reply = provider.complete(&messages).await.unwrap();

        assert_eq!(reply, "限流后成功");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_no_retry_on_401() {
        // 401 不在可重试白名单内：立即失败，断言仅 1 次请求
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            attempts_server.fetch_add(1, Ordering::Relaxed);
            MockResponse { status: 401, body: "unauthorized".into() }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let result = provider.complete(&messages).await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_retry_exhausted_on_5xx() {
        // 永远返回 500：重试到上限（MAX_RETRIES=3 次尝试）后返回 Err
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            attempts_server.fetch_add(1, Ordering::Relaxed);
            MockResponse { status: 500, body: "internal error".into() }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let messages = vec![Message::user("你好")];
        let result = provider.complete(&messages).await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), MAX_RETRIES as usize);
    }

    #[tokio::test]
    async fn test_retry_on_timeout() {
        // 第一次响应慢于客户端超时（触发超时重试），第二次立即成功。
        // 直接测共享骨架：短超时 client + 慢响应 mock，断言请求次数 = 2
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let base_url = spawn_mock_server(move |_req| {
            let n = attempts_server.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            MockResponse { status: 200, body: "{}".into() }
        });

        let client = Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();

        let resp = retry_with_backoff(MAX_RETRIES, || {
            client.get(format!("{}/t", base_url)).send()
        })
        .await
        .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_parse_sse_openai_format() {
        // OpenAI 格式：data: 行内 choices[0].delta.content；[DONE] 行被跳过
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_stream(sse.as_bytes(), "data: ", |v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(|s| s.to_string())
        });
        assert_eq!(chunks, vec!["你", "好"]);
    }

    #[test]
    fn test_parse_sse_anthropic_format() {
        // Anthropic 格式：data: 行内 type=content_block_delta 事件的 delta.text，
        // 其余事件（message_start/message_stop）被跳过
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"你\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"好\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let chunks = parse_sse_stream(sse.as_bytes(), "data: ", |v| {
            if v["type"] == "content_block_delta" {
                v["delta"]["text"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        });
        assert_eq!(chunks, vec!["你", "好"]);
    }

    #[tokio::test]
    async fn test_anthropic_stream_parses_sse() {
        // Anthropic 流式：mock base_url 生效（修复 stream 硬编码 URL）+ SSE 解析
        let sse = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"克\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"劳\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: sse.to_string(),
        });

        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: Some(base_url),
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
        };
        let provider = AnthropicProvider::new(&config).unwrap();
        let messages = vec![Message::user("你好")];
        let chunks = provider.complete_stream(&messages).await.unwrap();

        assert_eq!(chunks, vec!["克", "劳"]);
    }

    #[test]
    fn test_anthropic_provider_construction() {
        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: None,
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
        };
        let provider = AnthropicProvider::new(&config).unwrap();
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
                // Anthropic 流式格式（complete 走流式默认实现后 mock 必须返回 SSE）
                body: concat!(
                    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"claude 回复\"}}\n\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                )
                .into(),
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: Some(base_url),
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
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

    // ============ v17 B4/B5：Responses 协议（openai provider） ============

    /// Responses 流式 SSE 解析：语义化事件（response.created →
    /// output_text.delta ×2 → response.completed），无 [DONE] 终止符，
    /// 流结束即终止（collect_sse 兼容）
    #[tokio::test]
    async fn test_responses_stream_parses_semantic_sse() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":0,\"delta\":\"你\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"好\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
        );
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: sse.to_string(),
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Responses).unwrap();
        let chunks = provider.complete_stream(&[Message::user("你好")]).await.unwrap();
        assert_eq!(chunks, vec!["你", "好"], "语义化事件应提取 delta 文本");
        assert_eq!(provider.call_count(), 1);
    }

    /// Responses 请求体：input/instructions/max_output_tokens（v17 B4 协议差异）
    #[tokio::test]
    async fn test_responses_request_builds_correct_payload() {
        let captured = Arc::new(Mutex::new(None::<MockRequest>));
        let captured_server = captured.clone();
        let base_url = spawn_mock_server(move |req| {
            *captured_server.lock().unwrap() = Some(req);
            MockResponse {
                status: 200,
                body: "data: {\"type\":\"response.completed\"}\n\n".into(),
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Responses).unwrap();
        let messages = vec![Message::system("你是助手"), Message::user("你好")];
        let _ = provider.complete_stream(&messages).await.unwrap();

        let req = captured.lock().unwrap().take().expect("应收到一次请求");
        assert_eq!(req.path, "/v1/responses", "Responses 协议应请求 /responses 端点");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "deepseek-v4-flash");
        // system 消息分离到顶层 instructions；user 进 input items
        assert_eq!(body["instructions"], "你是助手");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "非 system 消息才进 input");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "你好");
        // token 上限与温度参数：v22 起硬编码为 None（交给模型默认），
        // 测试断言"既不写 max_output_tokens 也不写 max_tokens/temperature"
        assert!(body.get("max_output_tokens").is_none(), "硬编码后不应写 max_output_tokens");
        assert!(body.get("max_tokens").is_none(), "Responses 不得用 max_tokens 参数名");
        assert!(body.get("temperature").is_none(), "硬编码后不应写 temperature");
        assert_eq!(body["stream"].as_bool(), Some(true));
    }

    /// v17 B5：Responses 端点不支持（404）→ 自动回退 chat/completions 重发成功
    #[tokio::test]
    async fn test_responses_falls_back_to_chat_on_404() {
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_server = requests.clone();
        let base_url = spawn_mock_server(move |req| {
            requests_server.lock().unwrap().push(req.path.clone());
            if req.path.ends_with("/responses") {
                MockResponse { status: 404, body: "not found".into() }
            } else {
                MockResponse {
                    status: 200,
                    body: "data: {\"choices\":[{\"delta\":{\"content\":\"回退成功\"}}]}\n\ndata: [DONE]\n\n".into(),
                }
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Responses).unwrap();
        let chunks = provider.complete_stream(&[Message::user("你好")]).await.unwrap();
        assert_eq!(chunks.join(""), "回退成功");
        let paths = requests.lock().unwrap();
        assert_eq!(paths.len(), 2, "应请求 responses + chat 两次");
        assert!(paths[0].ends_with("/responses"), "第一次应请求 responses: {:?}", paths);
        assert!(paths[1].ends_with("/chat/completions"), "回退应请求 chat/completions: {:?}", paths);
    }

    /// v22 修复：评测裁判完整调用带显式输出预算——请求体必须写入
    /// max_output_tokens=16384（reasoning 型模型预算不足时只有
    /// reasoning 块没有 message，见 BENCH_MAX_OUTPUT_TOKENS 文档）
    #[tokio::test]
    async fn test_complete_with_budget_sets_max_output_tokens() {
        let captured = Arc::new(Mutex::new(None::<MockRequest>));
        let captured_server = captured.clone();
        let base_url = spawn_mock_server(move |req| {
            *captured_server.lock().unwrap() = Some(req);
            MockResponse {
                status: 200,
                body: "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"rubrics\\\":[]}\"}\n\n"
                    .into(),
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Responses).unwrap();
        let out = provider
            .complete_with_budget(&[Message::user("你好")], Some(16384))
            .await
            .unwrap();
        assert_eq!(out, "{\"rubrics\":[]}", "带预算调用应返回完整文本");

        let req = captured.lock().unwrap().take().expect("应收到一次请求");
        assert_eq!(req.path, "/v1/responses");
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["max_output_tokens"].as_u64(), Some(16384), "预算应写入 max_output_tokens");
        assert_eq!(body["stream"].as_bool(), Some(true), "带预算路径仍走流式");
    }
}
