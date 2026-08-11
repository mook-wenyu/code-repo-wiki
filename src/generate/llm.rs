use std::sync::Arc;
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
        Err(anyhow::anyhow!("当前 Provider 未实现流式调用（complete_stream）"))
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

/// 指数退避（full jitter，P2-8）：sleep = random(0, min(cap, base·2^attempt))。
///
/// 2026 共识（OpenAI Rate Limits 指南/DeepSeek 文档）：等值抖动（等宽区间加
/// 固定基数）在批量并行重试时同步化风险高，full jitter 期望值更低且防惊群；
/// 退避必须设 cap 防止无限增长。抖动用系统时钟纳秒取模实现，避免引入 rand。
/// 429 响应若携带 Retry-After 头，retry_with_backoff 会以该值为退避下限。
const BACKOFF_BASE_MS: u64 = 500;
/// 退避上限：单次重试等待不超过 8s（指数增长 500ms→1s→2s→4s→8s 封顶）
const BACKOFF_CAP_MS: u64 = 8_000;

fn backoff_delay(attempt: u32) -> Duration {
    // full jitter：上限 = min(cap, base·2^attempt)，随机落在 [0, 上限)
    let max_ms = BACKOFF_BASE_MS
        .checked_mul(1u64 << attempt.min(4))
        .unwrap_or(BACKOFF_CAP_MS)
        .min(BACKOFF_CAP_MS);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % max_ms)
        .unwrap_or(0);
    Duration::from_millis(jitter)
}

/// 可重试的 HTTP 状态码白名单：429 限流 + 5xx 服务端错误；
/// 其余 4xx 业务错误（400/401/403/404/422 等）不在白名单内，立即失败。
///
/// 429 与 503 同路径处理（P2-8 决策）：503（供应商过载）在 CLI 单进程场景
/// 下与 429 一样值得重试——本工具无跨请求状态，circuit breaker 是长服务
/// 架构（网关/常驻进程）的优化，引入会违背 KISS/YAGNI；重试预算由
/// MAX_RETRIES=3 封顶，日志记录每次重试原因（可观测性已覆盖）。
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
) -> anyhow::Result<reqwest::Response>
where
    F: Fn() -> Fut,
    // v47：输出类型从 reqwest::Result 放宽为 anyhow::Result——send 阶段
    // 现由 send_with_timeout 包裹（tokio 首字节超时→anyhow 错误），
    // 网络错误重试判定改为 downcast reqwest::Error（见 is_retryable_network）。
    Fut: std::future::Future<Output = anyhow::Result<reqwest::Response>> + Send,
{
    let mut last_error = None;
    // P2-8：服务端 Retry-After 给出的退避下限（429 携带时记录，5xx 不读该头）
    let mut next_retry_after: Option<Duration> = None;
    for attempt in 0..max_retries {
        if attempt > 0 {
            // 退避 = max(full_jitter 退避, 服务端 Retry-After 下限)；两者皆为
            // 单次延迟，不做累加（每次重试重新采样）
            let delay = next_retry_after
                .map(|r| backoff_delay(attempt - 1).max(r))
                .unwrap_or_else(|| backoff_delay(attempt - 1));
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
                // P2-8：429 响应若携带 Retry-After 头（OpenAI 必有、DeepSeek
                // 可能返回），以其秒数为退避下限——服务端给定的节流窗口
                // 优先于本地估算；5xx 不读该头（服务端不承诺恢复时间）。
                let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    resp.headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                } else {
                    None
                };
                let text = resp.text().await.unwrap_or_default();
                last_error = Some(anyhow::anyhow!("API 返回错误 ({}): {}", status, text));
                if let Some(secs) = retry_after.as_ref() {
                    // Retry-After 存在则作为下次退避的下限（u64 为 Copy）
                    next_retry_after = Some(Duration::from_secs(*secs));
                }
                tracing::warn!(
                    "LLM API 返回可重试状态 {}（第 {} 次尝试）{}: {}",
                    status,
                    attempt + 1,
                    retry_after.map(|s| format!("，Retry-After {}s", s)).unwrap_or_default(),
                    text.chars().take(2000).collect::<String>()
                );
            }
            Ok(resp) => return Ok(resp),
            Err(e) if is_retryable_network(&e) => {
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

/// 网络类错误才重试：仅 reqwest 真错误（连接失败/客户端超时）标记为
/// 可重试；send_with_timeout 的 tokio 首字节超时（anyhow 包装，90s 无
/// 首字节）不重试——那是端点黑洞信号，重试只会再等 90s。
fn is_retryable_network(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .is_some_and(|re| re.is_timeout() || re.is_connect())
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
///
/// 返回二元组 (chunks, finish_reason)：chunks 为提取的文本增量，
/// finish_reason 为终止原因（chat: choices[0].finish_reason；Anthropic:
/// message_delta 的 delta.stop_reason；Responses: response.incomplete 事件）。
/// P0-8 截断检测：调用方据终止原因（length/max_tokens/incomplete）显式
/// 报错，不再把被截断的残缺内容当完整产物消费。
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// P2-9：SSE 单行长度上限——异常端点/恶意响应可让 buf 无限增长拖垮内存
const MAX_SSE_LINE_BYTES: usize = 1_048_576; // 1MiB

/// send 阶段总超时（仅到首字节）
///
/// 流式主体由 collect_sse 的空闲超时保护（长生成不截断），但
/// `reqwest::RequestBuilder::send()` 只覆盖「发出请求 → 收到响应首字节」：
/// 若端点黑洞（TCP 连上但永不返回 HTTP 头），send 会无限挂起——
/// 实测出现过 16 小时僵尸进程（generate 卡死不退出、锁残留阻塞后续命令）。
/// 首字节等待超 90s 即判定端点不可达，交由上层重试/错误传播。
const SEND_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(90);

/// 带首字节超时的请求发送（防黑洞挂起；测试可注入更短超时）
///
/// 注意：不接收 `&Client`——`reqwest::RequestBuilder` 内部已持有 client
/// 引用，`req.send()` 无需外部 client（clippy unused_variables 实证）。
async fn send_with_timeout(
    req: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response> {
    tokio::time::timeout(timeout, req.send())
        .await
        .map_err(|_| anyhow::anyhow!("请求超时（{}s 未收到响应首字节，端点可能不可达）", timeout.as_secs()))?
        .map_err(Into::into)
}

async fn collect_sse(
    resp: reqwest::Response,
    line_prefix: &str,
    extract: impl Fn(&serde_json::Value) -> Option<String>,
    finish_extract: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<(Vec<String>, Option<String>)> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunks = Vec::new();
    let mut finish_reason: Option<String> = None;

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
        if buf.len() > MAX_SSE_LINE_BYTES {
            anyhow::bail!(
                "SSE 单行超过 {} 字节上限（疑似端点异常），中止消费",
                MAX_SSE_LINE_BYTES
            );
        }
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
            {
                if let Some(text) = extract(&val) {
                    chunks.push(text);
                }
                if let Some(reason) = finish_extract(&val) {
                    finish_reason = Some(reason);
                }
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
        {
            if let Some(text) = extract(&val) {
                chunks.push(text);
            }
            if let Some(reason) = finish_extract(&val) {
                finish_reason = Some(reason);
            }
        }
    }
    tracing::info!(
        "SSE 流消费完成: {} 个 chunk, {} 字符",
        chunks.len(),
        chunks.iter().map(|c| c.len()).sum::<usize>()
    );
    Ok((chunks, finish_reason))
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
    /// DeepSeek 系 thinking 模式（v50）：None=不发送；Some(true/false)=
    /// 发送 `thinking: {"type":"enabled"/"disabled"}`（仅 chat 协议——
    /// deepseek-v4 默认启用 thinking，批量生成慢约 5×，见 schema 注释）
    thinking: Option<bool>,
    /// DeepSeek 系 reasoning_effort（v50）："low"/"high"/"max"，与
    /// thinking 配套发送（官方映射：v4-flash low→low、high→high、max→max）
    reasoning_effort: Option<String>,
    call_count: std::sync::atomic::AtomicUsize,
    /// 并发信号量（P2-8）：批量并行调用时的并发上限，防触发服务端动态限流
    semaphore: Arc<tokio::sync::Semaphore>,
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

        // P2-11：thinking/reasoning_effort 仅 chat/completions 协议有效——
        // Responses 协议无对应参数（官方 Responses 用 output_config），
        // 用户按文档配置的 5× 提速开关在 Responses 下静默失效，必须显式警告
        if protocol == OpenAiProtocol::Responses
            && (config.thinking.is_some() || config.reasoning_effort.is_some())
        {
            tracing::warn!(
                "配置了 thinking/reasoning_effort，但当前 provider 使用 Responses 协议——该参数仅 chat/completions 协议生效（被静默忽略）"
            );
        }

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            base_url,
            protocol,
            max_retries: MAX_RETRIES,
            max_tokens: None,
            temperature: None,
            // v50：thinking 相关参数从配置透传（仅 chat 协议使用——
            // Responses 协议无对应参数，见 build_chat_body 注释）
            thinking: config.thinking,
            reasoning_effort: config.reasoning_effort.clone(),
            call_count: std::sync::atomic::AtomicUsize::new(0),
            // P2-8：并发上限来自配置（None=默认 16）；tokio Semaphore::new
            // 许可数上限约 2^61，u32 配置值远低于此（card.rs 已有先例）
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                {
                    let mc = config.max_concurrency.unwrap_or(16);
                    anyhow::ensure!(mc > 0, "max_concurrency 必须为正整数（当前 0）");
                    mc as usize
                }
            )),
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
        // v50：DeepSeek 系 thinking 模式/推理强度（仅 chat/completions 协议）
        //
        // 官方参数（2026-08-10 抓取 api-docs.deepseek.com/guides/thinking_mode）：
        //   thinking: {"type":"enabled"/"disabled"} + reasoning_effort: "low"/"high"/"max"
        // thinking 默认启用且 effort=high；批量文档/卡片生成（低推理任务）
        // 显式关闭可省约 5× 延迟（thinking 多 3.7× 输出 token——第三方实测）。
        // Responses 协议无此参数（其 effort 走 output_config——本项目未使用，
        // 见 build_responses_body 注释），Anthropic 协议无此参数。
        if let Some(thinking) = self.thinking {
            body["thinking"] = serde_json::json!({ "type": if thinking { "enabled" } else { "disabled" } });
        }
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = serde_json::json!(effort);
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
        // P2-8：并发信号量——超出上限的调用等待（信号量在并发调用间共享）
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;
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
        // P2-8：并发信号量——超出上限的调用等待（信号量在并发调用间共享）
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;
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

        // v47：send 包首字节超时（防端点黑洞挂起——实测 16h 僵尸进程）
        let resp = retry_with_backoff(self.max_retries, || {
            send_with_timeout(
                self.client.post(&url).bearer_auth(&self.api_key).json(&body),
                SEND_FIRST_BYTE_TIMEOUT,
            )
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("API 返回错误 ({}): {}", status, text);
        }

        // chat/completions SSE：data: 行内 choices[0].delta.content；
        // 终止原因 choices[0].finish_reason（"stop"/"length"）——P0-8 截断检测
        let (chunks, finish_reason) = collect_sse(
            resp,
            "data: ",
            |v| {
                v["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(|s| s.to_string())
            },
            |v| {
                v["choices"][0]["finish_reason"]
                    .as_str()
                    .map(|s| s.to_string())
            },
        )
        .await?;
        // P0-8：finish_reason=length 表示输出被 max_tokens 截断——显式报错，
        // 调用方（卡片/页面重试协议）按失败处理，不再把残缺内容当完整产物
        if finish_reason.as_deref() == Some("length") {
            anyhow::bail!("模型输出被 max_tokens 截断（finish_reason=length），请增大输出预算或简化输入后重试");
        }
        Ok(chunks)
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

        // v47：send 包首字节超时（防端点黑洞挂起）
        let resp = retry_with_backoff(self.max_retries, || {
            send_with_timeout(
                self.client.post(&url).bearer_auth(&self.api_key).json(&body),
                SEND_FIRST_BYTE_TIMEOUT,
            )
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
        // 事件的 delta 字段；截断检测用 type=response.incomplete 事件（reason 为
        // max_output_tokens 等）；response.completed 为正常终止——P0-8
        let (chunks, finish_reason) = collect_sse(
            resp,
            "data: ",
            |v| {
                if v["type"].as_str() == Some("response.output_text.delta") {
                    v["delta"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            },
            |v| {
                if v["type"].as_str() == Some("response.incomplete") {
                    Some("length".to_string())
                } else {
                    None
                }
            },
        )
        .await?;
        if chunks.is_empty() {
            anyhow::bail!("模型返回空响应（Responses 流无任何输出文本）");
        }
        // P0-8：response.incomplete 事件表示输出被 max_output_tokens 截断
        // （finish_extract 已映射为 "length"）——与 chat/anthropic 路径同语义，
        // 显式报错，不再把部分内容当完整产物
        if finish_reason.as_deref() == Some("length") {
            anyhow::bail!("模型输出被截断（response.incomplete），请增大 max_output_tokens 或简化输入后重试");
        }
        Ok(chunks)
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
    /// 并发信号量（P2-8）：批量并行调用时的并发上限，防触发服务端动态限流
    semaphore: Arc<tokio::sync::Semaphore>,
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
            // P2-8：并发上限来自配置（None=默认 16）
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                {
                    let mc = config.max_concurrency.unwrap_or(16);
                    anyhow::ensure!(mc > 0, "max_concurrency 必须为正整数（当前 0）");
                    mc as usize
                }
            )),
        })
    }
}

impl AnthropicProvider {
    /// 构建 messages API 请求体：system 消息分离到顶层字段、
    /// 非 system 消息进 messages、max_tokens 未配置时默认 8192（P2-14：4096 会截断长 wiki 页/评测输出）；
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
            // 显式覆盖优先，回退构造时的 max_tokens（Anthropic API 必填
            // max_tokens；默认 8192——4096 截断长 wiki 页/评测输出，P2-14）
            "max_tokens": max_tokens_override.or(self.max_tokens).unwrap_or(8192),
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

        // P2-8：并发信号量——超出上限的调用等待
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;

        let url = format!("{}/messages", self.base_url);
        let body = self.build_messages_body(messages, true, None);

        // v47：send 包首字节超时（防端点黑洞挂起）
        let resp = retry_with_backoff(self.max_retries, || {
            send_with_timeout(
                self.client
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body),
                SEND_FIRST_BYTE_TIMEOUT,
            )
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 返回错误 ({}): {}", status, text);
        }

        // Anthropic SSE：data: 行内 type=content_block_delta 事件的 delta.text；
        // 终止原因 type=message_delta 事件的 delta.stop_reason（"end_turn" 正常 /
        // "max_tokens" 截断）——P0-8
        let (chunks, finish_reason) = collect_sse(
            resp,
            "data: ",
            |v| {
                if v["type"] == "content_block_delta" {
                    v["delta"]["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            },
            |v| {
                if v["type"] == "message_delta" {
                    v["delta"]["stop_reason"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            },
        )
        .await?;
        // P0-8：stop_reason=max_tokens 表示输出被 max_tokens 截断——显式报错，
        // 不把残缺内容当完整产物
        if finish_reason.as_deref() == Some("max_tokens") {
            anyhow::bail!("模型输出被 max_tokens 截断（stop_reason=max_tokens），请增大 max_tokens 或简化输入后重试");
        }
        Ok(chunks)
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

        // P2-8：并发信号量——超出上限的调用等待
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("并发信号量已关闭"))?;

        let url = format!("{}/messages", self.base_url);
        let body = self.build_messages_body(messages, true, max_output_tokens);

        // v47：send 包首字节超时（防端点黑洞挂起）
        let resp = retry_with_backoff(self.max_retries, || {
            send_with_timeout(
                self.client
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body),
                SEND_FIRST_BYTE_TIMEOUT,
            )
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 返回错误 ({}): {}", status, text);
        }

        // Anthropic SSE：data: 行内 type=content_block_delta 事件的 delta.text；
        // 终止原因 type=message_delta 事件的 delta.stop_reason（"end_turn" 正常 /
        // "max_tokens" 截断）——P0-8
        let (chunks, finish_reason) = collect_sse(
            resp,
            "data: ",
            |v| {
                if v["type"] == "content_block_delta" {
                    v["delta"]["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            },
            |v| {
                if v["type"] == "message_delta" {
                    v["delta"]["stop_reason"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            },
        )
        .await?;
        // P0-8：stop_reason=max_tokens 表示输出被 max_tokens 截断——显式报错，
        // 不把残缺内容当完整产物（预算路径同语义）
        if finish_reason.as_deref() == Some("max_tokens") {
            anyhow::bail!("模型输出被 max_tokens 截断（stop_reason=max_tokens），请增大 max_tokens 或简化输入后重试");
        }
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        }
    }

    /// max_concurrency=0 是配置错误：Semaphore::new(0) 无许可会导致
    /// acquire 永久挂起，构造器必须显式拒绝（T04b）。
    #[test]
    fn test_openai_rejects_zero_concurrency() {
        let mut cfg = openai_config("http://127.0.0.1:1");
        cfg.max_concurrency = Some(0);
        let err = OpenAiProvider::new(&cfg, OpenAiProtocol::Responses)
            .err()
            .expect("max_concurrency=0 应被构造器拒绝")
            .to_string();
        assert!(err.contains("必须为正整数"), "错误信息应引导配置修正: {err}");
    }

    /// Anthropic 同款拒绝（T04b 三处统一）
    #[test]
    fn test_anthropic_rejects_zero_concurrency() {
        let mut cfg = openai_config("http://127.0.0.1:1");
        cfg.provider = LlmProviderType::Anthropic;
        cfg.max_concurrency = Some(0);
        let err = AnthropicProvider::new(&cfg)
            .err()
            .expect("max_concurrency=0 应被构造器拒绝")
            .to_string();
        assert!(err.contains("必须为正整数"), "错误信息应引导配置修正: {err}");
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

    #[test]
    fn test_build_chat_body_thinking_modes() {
        // v50：thinking 三态 + reasoning_effort 透传（deepseek 官方参数：
        // thinking: {"type":"enabled"/"disabled"} + reasoning_effort，
        // 2026-08-10 抓取 api-docs.deepseek.com/guides/thinking_mode）
        let base = OpenAiProvider::new(
            &openai_config("http://localhost:1"),
            OpenAiProtocol::Chat,
        )
        .unwrap();

        // None：不发送（保持 provider 默认——deepseek-v4 默认启用 thinking）
        let body = base.build_chat_body(&[], false, None);
        assert!(body.get("thinking").is_none(), "None 不应发送 thinking");
        assert!(
            body.get("reasoning_effort").is_none(),
            "None 不应发送 reasoning_effort"
        );

        // Some(false)：显式关闭（批量文档生成推荐——thinking 默认 high
        // 使延迟慢约 5×）
        let mut cfg = openai_config("http://localhost:1");
        cfg.thinking = Some(false);
        let provider = OpenAiProvider::new(&cfg, OpenAiProtocol::Chat).unwrap();
        let body = provider.build_chat_body(&[], false, None);
        assert_eq!(body["thinking"]["type"], "disabled");

        // Some(true) + effort：显式启用并指定推理强度
        let mut cfg = openai_config("http://localhost:1");
        cfg.thinking = Some(true);
        cfg.reasoning_effort = Some("high".into());
        let provider = OpenAiProvider::new(&cfg, OpenAiProtocol::Chat).unwrap();
        let body = provider.build_chat_body(&[], false, None);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
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

    /// P2-8：429 响应携带 Retry-After 头时，退避下限尊重服务端节流窗口
    /// （不早于 Retry-After 秒重试）。spawn_mock_server 响应头硬编码无自定义
    /// 能力，用裸 TcpListener 手动写原始响应（与慢流测试同款模式）。
    ///
    /// 连接 1 返回 429 + Retry-After: 1；连接 2 返回 200 SSE。两连接共享同一
    /// listener（for 循环持续 accept），按连接序号区分响应。
    #[tokio::test]
    async fn test_retry_respects_retry_after_header() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_server = attempts.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let attempts_server = attempts_server.clone();
                std::thread::spawn(move || {
                    let _req = read_request(&mut stream);
                    let n = attempts_server.fetch_add(1, Ordering::Relaxed);
                    if n == 0 {
                        // 第一次：429 + Retry-After: 1（服务端节流窗口 1s）
                        let resp = "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 1\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"rate\"}";
                        let _ = stream.write_all(resp.as_bytes());
                    } else {
                        // 第二次：200 + SSE 成功流
                        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"重试后成功\"}}]}\n\ndata: [DONE]\n\n";
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            sse.len(),
                            sse
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    let _ = stream.flush();
                });
            }
        });

        let provider = OpenAiProvider::new(&openai_config(&base_url), OpenAiProtocol::Chat).unwrap();
        let start = std::time::Instant::now();
        let reply = provider.complete(&[Message::user("你好")]).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(reply, "重试后成功");
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "应重试一次");
        assert!(
            elapsed >= std::time::Duration::from_secs(1),
            "Retry-After=1s 必须作为退避下限（重试等待不早于 1s）: {:?}",
            elapsed
        );
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
            let client = client.clone();
            let url = format!("{base_url}/t");
            async move { Ok(client.get(url).send().await?) }
        })
        .await
        .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_send_with_timeout_blackhole() {
        // v47：端点黑洞（TCP 连上但永不返回首字节）——send_with_timeout
        // 应在注入的短超时内返回 Err，而不是无限挂起（16h 僵尸进程前科：
        // generate 卡死、锁残留阻塞后续命令）。provider 生产路径使用
        // SEND_FIRST_BYTE_TIMEOUT=90s（首字节后流式空闲超时接管）。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // 黑洞：接受连接后不读不写，挂住连接（线程 10s 后自然退出；
        // 期间由 send 侧超时保护——若立即关闭连接会变成 EOF 而非超时，
        // 必须把连接挂住）
        std::thread::spawn(move || {
            // 接受一个连接（incoming().next() 即 accept），不读不写挂住
            if let Some(Ok(_stream)) = listener.incoming().next() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });

        let client = Client::new();
        let started = std::time::Instant::now();
        let result = send_with_timeout(
            client.get(format!("http://{addr}/t")),
            Duration::from_millis(500),
        )
        .await;
        assert!(result.is_err());
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(450) && elapsed < Duration::from_secs(3),
            "超时应在注入的 500ms 附近返回，实际 {elapsed:?}"
        );
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
        assert_eq!(body["max_tokens"].as_u64(), Some(8192), "max_tokens 未配置时默认 8192（P2-14：4096 截断长输出）");
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
                body: "data: {\"type\":\"response.output_text.delta\",\"delta\":\"正常\"}\n\ndata: {\"type\":\"response.completed\"}\n\n".into(),
            }
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
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

    /// P0-8 截断检测：chat 协议 finish_reason=length → 显式报错（不再把
    /// 截断的残缺内容当完整产物写盘）
    #[tokio::test]
    async fn test_stream_truncation_chat_detected() {
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"部分内容\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n".into(),
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Chat).unwrap();
        let result = provider.complete(&[Message::user("你好")]).await;

        assert!(result.is_err(), "finish_reason=length 必须报错而非静默返回残缺内容");
        let err = format!("{:?}", result.err().unwrap());
        assert!(err.contains("截断"), "错误消息应说明截断: {err}");
    }

    /// P0-8 截断检测：Anthropic stop_reason=max_tokens → 显式报错
    #[tokio::test]
    async fn test_stream_truncation_anthropic_detected() {
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"部分内容\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n".into(),
        });

        let config = LlmSection {
            provider: LlmProviderType::Anthropic,
            model: "claude-test".into(),
            base_url: Some(base_url),
            api_key: Some("sk-ant-test".into()),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        };
        let provider = AnthropicProvider::new(&config).unwrap();
        let result = provider.complete(&[Message::user("你好")]).await;

        assert!(result.is_err(), "stop_reason=max_tokens 必须报错");
        let err = format!("{:?}", result.err().unwrap());
        assert!(err.contains("截断"), "错误消息应说明截断: {err}");
    }

    /// P0-8 截断检测：finish_reason=stop 正常终止 → 返回完整内容
    #[tokio::test]
    async fn test_stream_normal_finish_stop_ok() {
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"正常内容\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Chat).unwrap();
        let reply = provider.complete(&[Message::user("你好")]).await.unwrap();
        assert_eq!(reply, "正常内容", "正常终止应返回完整内容");
    }

    /// P0-8 截断检测：Responses 协议 response.incomplete 事件 → 显式报错
    /// （此前事件被检测但从未消费，截断流静默当完整产物——reviewer 阻塞项）
    #[tokio::test]
    async fn test_stream_truncation_responses_detected() {
        let base_url = spawn_mock_server(move |_req| MockResponse {
            status: 200,
            body: "data: {\"type\":\"response.output_text.delta\",\"delta\":\"部分内容\"}\n\ndata: {\"type\":\"response.incomplete\",\"reason\":\"max_output_tokens\"}\n\n".into(),
        });

        let config = LlmSection {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".into(),
            base_url: Some(format!("{}/v1", base_url)),
            api_key: Some("test-key".into()),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        };
        let provider = OpenAiProvider::new(&config, OpenAiProtocol::Responses).unwrap();
        let result = provider.complete(&[Message::user("你好")]).await;

        assert!(result.is_err(), "response.incomplete 必须报错而非静默返回部分内容");
        let err = format!("{:?}", result.err().unwrap());
        assert!(err.contains("截断"), "错误消息应说明截断: {err}");
    }
}
