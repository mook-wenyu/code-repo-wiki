use serde::{Deserialize, Serialize};

/// v22 硬编码常量：以下配置项属「算法内部细节 / 无调优需求 / 必填负担」，
/// 从配置文件移除、以代码常量固定，减少用户配置心智负担（用户拍板：
/// 推荐 10 项全部硬编码）。如需调整须改代码重新编译。
/// LLM 并发上限（v50：16 → 128）
///
/// 依据（2026-08-10 权威查证）：
/// - DeepSeek 官方限流=纯并发数（v4-flash 账户级 2500），无 RPM/TPM——
///   128 远低于上限；
/// - 服务端连续批处理（Orca/vLLM）的吞吐拐点约 128 并发：拐点后并发
///   只买延迟不买吞吐（NVIDIA NIM 官方基准：并发 100→250 吞吐 917→920
///   持平而 TTFT 8s→88s）——128 是拐点内最大化收益的取值；
/// - 超限由 llm.rs retry_with_backoff 的 429 全抖动退避兜底（失败计配额，
///   退避优于重放——OpenAI 官方限流指南）。
pub const LLM_MAX_CONCURRENT: usize = 128;
/// None=模型默认，不随请求发送
pub const EMBED_BATCH_SIZE: usize = 20;
/// 索引目录，相对 output.dir
pub const SEARCH_INDEX_DIR: &str = ".search";
/// 默认搜索引擎：v36 起为 Hybrid（BM25 召回 + 向量语义 + RRF 融合 + 调用链
/// 补全）。个人仓库场景下混合召回显著优于单一引擎，且
/// 无 embed key 时 hybrid 自动降级纯 text（search 层已验证），默认值
/// 不会让无 key 用户受损。
pub const SEARCH_DEFAULT_ENGINE: SearchEngineType = SearchEngineType::Hybrid;
pub const SEARCH_DEFAULT_TOP_K: usize = 10;
/// RRF 融合常数 k（控制排序权重衰减）
///
/// P2-7：60.0 是 SIGIR'09 原文默认（面向多路融合的共识投票）；本工具
/// 是两路（text BM25 + semantic 向量）融合，2025 检索共识建议两路场景
/// 20-40——k 越小排名头部权重越陡（强命中更突出）。取 40.0（区间中值，
/// 兼顾强命中突出与候选覆盖）。v30 哲学：算法细节硬编码，不设配置项。
pub const SEARCH_RRF_K: f64 = 40.0;
/// BFS 传播变更影响的最大深度
pub const IMPACT_MAX_DEPTH: usize = 3;
/// v30 硬编码常量：傻瓜式全自动（用户拍板「彻底硬编码删字段」）——
/// 以下配置项从配置文件移除、以代码常量固定，用户零配置开箱即用。
pub const OUTPUT_DIR: &str = ".code-repo-wiki";

/// 全局配置
///
/// v30 精简后的配置面：只保留「凭据 / 模型选择 / 主语言」三类
/// 用户真正需要决策的项；输出目录、扫描范围、增量策略、搜索/嵌入开关、
/// 计划文件等算法细节全部硬编码（见常量区与扫描器内置过滤）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiConfig {
    #[serde(default)]
    pub wiki: WikiSection,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub embed: EmbedSection,
    /// 运行时输出目录（serde(skip)：配置文件中不可写，由
    /// load_config_with_output 注入——CLI --output 覆盖或 root 化后的
    /// 绝对路径；None 时由 output_dir() 方法兜底硬编码常量）。
    /// 使用场景：bench 跑分把产物写到隔离目录，不污染真实 .code-repo-wiki。
    #[serde(skip)]
    pub output_dir: Option<std::path::PathBuf>,
}

impl WikiConfig {
    /// 输出目录解析：运行时注入优先（--output 覆盖 / root 化），
    /// 缺省回退硬编码常量 OUTPUT_DIR（相对当前工作目录）。
    pub fn output_dir(&self) -> &std::path::Path {
        match &self.output_dir {
            Some(p) => p,
            None => std::path::Path::new(crate::config::schema::OUTPUT_DIR),
        }
    }
}

/// Wiki 基本配置（v30：多语言扩展已删除——恒只生成主语言，避免维护
/// 多语言产物面；如需其他语言改 language 主键即可；缺键默认 zh）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSection {
    #[serde(default = "default_language")]
    pub language: String,
    /// v32 9.1：生成引导段（[wiki.guide]）——空=现行为零破坏。
    /// 缺段或缺键时全部回退空 Vec，不报错（傻瓜式零配置原则）。
    #[serde(default)]
    pub guide: WikiGuideSection,
}

/// 生成引导档位：
/// - `comprehensive`（默认）：全量生成，notes 引导注记全部注入；
/// - `concise`：精简引导——notes 每条截断至 160 字符、最多注入 3 条
///   （不丢模块/不丢页面，只精简「引导注记」本身；pages/priority 语义不变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GuideTier {
    #[default]
    Comprehensive,
    Concise,
}

/// v32 9.1 生成引导（[wiki.guide]）：
/// - `pages`：要生成的模块页路径前缀白名单（空=全部模块）。匹配按
///   模块路径前缀（如 "src/net" 匹配 "src/net/tcp.rs" 模块页）；未匹配
///   的模块不生成独立页，但仍保留 overview 汇总；全部为空匹配时报错。
/// - `priority`：模块页确定性排序列表（优先在前的路径前缀），用于把
///   核心模块排在文档前面；不在列表中的模块保持默认顺序。
/// - `notes`：注入模块页生成 prompt 的引导说明（逐条列出），引导 LLM
///   按项目约定撰写页面内容（如命名规范、必写小节、注意事项）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiGuideSection {
    #[serde(default)]
    pub pages: Vec<String>,
    #[serde(default)]
    pub priority: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// 生成引导档位（v32 T08b）：缺省 comprehensive=全量现行为零破坏；
    /// concise 只精简引导注记本身（不丢模块/不丢页面）。
    #[serde(default)]
    pub tier: GuideTier,
}

/// 按档位裁剪引导注记：comprehensive 原样返回；concise 每条截断至
/// 160 字符、最多 3 条（超出附加省略说明）。不改变 pages/priority 语义。
pub fn trim_guide_notes(tier: GuideTier, notes: &[String]) -> Vec<String> {
    match tier {
        GuideTier::Comprehensive => notes.to_vec(),
        GuideTier::Concise => {
            let mut out: Vec<String> = notes
                .iter()
                .take(3)
                .map(|n| {
                    let trimmed: String = n.chars().take(160).collect();
                    if trimmed.chars().count() < n.chars().count() {
                        format!("{}…", trimmed)
                    } else {
                        trimmed
                    }
                })
                .collect();
            if notes.len() > 3 {
                out.push(format!(
                    "（其余 {} 条引导注记已省略——concise 档位）",
                    notes.len() - 3
                ));
            }
            out
        }
    }
}

fn default_language() -> String {
    "zh".to_string()
}

impl Default for WikiSection {
    fn default() -> Self {
        Self {
            language: "zh".to_string(),
            guide: WikiGuideSection::default(),
        }
    }
}

/// LLM 提供商配置（v30：字段级 serde 默认=Default 阵营——缺键即用
/// 默认可用组合，项目级配置可只写想覆盖的键）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSection {
    #[serde(default = "default_llm_provider")]
    pub provider: LlmProviderType,
    #[serde(default = "default_llm_model")]
    pub model: String,
    #[serde(default = "default_llm_base_url")]
    pub base_url: Option<String>,
    /// 直接指定 API Key（优先级高于 api_key_env）
    #[serde(default)]
    pub api_key: Option<String>,
    /// 从环境变量读取 API Key（当 api_key 为 None 时使用）
    #[serde(default = "default_llm_api_key_env")]
    pub api_key_env: String,
    /// DeepSeek 系 thinking 模式开关（v50，可选）
    ///
    /// None = 不发送该参数（保持 provider 默认——deepseek-v4 官方默认
    /// **启用** thinking 且 effort=high，批量文档生成实测慢约 5×、输出
    /// token 多约 3.7×，是「LLM 太慢」的根因之一）；
    /// Some(true) = 发送 `thinking: {"type":"enabled"}`；
    /// Some(false) = 发送 `thinking: {"type":"disabled"}`。
    ///
    /// 仅 openai-compatible（chat/completions）路径生效；Responses 与
    /// Anthropic 协议不支持该参数（llm.rs 注释说明）。值域/映射以
    /// DeepSeek 官方 Thinking Mode 文档为准（2026-08-10 抓取核证）。
    #[serde(default)]
    pub thinking: Option<bool>,
    /// DeepSeek 系推理强度（v50，可选）："low" / "high" / "max"
    ///
    /// 与 thinking 配套发送 `reasoning_effort` 字段；缺省不发送。
    /// 官方映射：v4-flash low→low、high→high、max→max。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// LLM 调用并发上限（P2-8 配套，可选）
    ///
    /// 批量生成/评测并行调用 LLM 时的并发信号量上限。DeepSeek 官方
    /// 动态账户级并发（超限 429），官方实践建议 8-16 起步；None 时
    /// Provider 用内置默认（OpenAi/Anthropic 均为 16）。
    #[serde(default)]
    pub max_concurrency: Option<u32>,
}

fn default_llm_model() -> String {
    "deepseek-v4-flash".to_string()
}

fn default_llm_provider() -> LlmProviderType {
    LlmProviderType::OpenAiCompatible
}

fn default_llm_base_url() -> Option<String> {
    Some("https://opencode.ai/zen/go/v1".to_string())
}

fn default_llm_api_key_env() -> String {
    "OPENCODEGO2_API_KEY".to_string()
}

impl Default for LlmSection {
    fn default() -> Self {
        // v29 用户确认的实际可用配置：opencode 网关（openai-compatible 协议
        // chat/completions）——schema 缺省填充与模板（config.toml）
        // 严格同源，保证项目级 config.toml 缺 [llm] 段合并回退时仍可用。
        // 不得回退为旧阵营（openai 协议 + DeepSeek 官方端点 + DEEPSEEK_API_KEY）：
        // 那是初始示例，实际不可用（v28 t11 实测端点断裂）。
        Self {
            provider: LlmProviderType::OpenAiCompatible,
            model: "deepseek-v4-flash".to_string(),
            base_url: Some("https://opencode.ai/zen/go/v1".to_string()),
            api_key: None,
            api_key_env: "OPENCODEGO2_API_KEY".to_string(),
            // v50：默认 None——不发送 thinking 参数，保持 provider 默认
            // （deepseek-v4 默认启用 thinking）。用户可在 config.toml
            // 显式 `thinking = false` 关闭以获得约 5× 提速（见 schema 注释）。
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        }
    }
}

/// LLM Provider 类型（v17 t02 拆分：协议按 provider 类型显式绑定）
///
/// - `openai`：OpenAI **Responses API** 协议（base_url 可配——DeepSeek 等
///   支持 Responses 的服务通过 base_url 接入；默认官方端点）
/// - `openai-compatible`：**chat/completions** 协议（OpenAI 兼容端点：
///   阿里云/自建/无 /responses 的服务；v17 起 custom 并入此值）
/// - `anthropic`：Anthropic Messages API（不变）
/// - `mock`：本地模拟（测试/CI/无 Key 演示，不触网）
///
/// 拆分原因：Responses 与 chat/completions 的请求/响应/SSE 差异大，
/// 且不是所有 OpenAI 兼容端点都提供 /responses——按 provider 显式绑定
/// 协议，避免"无脑默认切换"破坏兼容端点。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LlmProviderType {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    #[serde(rename = "anthropic")]
    Anthropic,
    /// 本地模拟 Provider（测试/CI/无 API Key 演示），
    /// 返回固定文本，不发起任何网络请求。
    #[serde(rename = "mock")]
    Mock,
}

/// 嵌入提供方：远程 API 或本地 ONNX 推理
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedProvider {
    /// 远程 API（OpenAI 兼容 /v1/embeddings）
    #[default]
    Remote,
    /// 本地 ONNX 推理（fastembed，免 API key、免网络）
    Local,
    /// Mock 通道（测试用）：与 Remote 同路径——HTTP 请求指向本地 mock server
    /// 或失败降级；配置兼容 v50 前 `provider = "mock"` 的既有测试模板。
    Mock,
}

/// 嵌入模型配置（v30：enabled 开关已硬编码恒开启——无 Key 环境由
/// 运行时降级处理，见 lib.rs attach_features 与 build_search_index；
/// 字段级 serde 默认=Default 阵营，缺键即用默认可用组合）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSection {
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_embed_base_url")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_embed_api_key_env")]
    pub api_key_env: String,
    /// Embedding 调用并发上限（P2-8 配套，可选）
    ///
    /// embed_batch 分批发往 API 的并发信号量上限；None 时引擎用内置默认 4。
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// 嵌入提供方：remote（远程 API，默认）| local（fastembed 本地推理，免 API key）
    #[serde(default)]
    pub provider: EmbedProvider,
    /// 本地嵌入模型名（provider=local 时生效）；默认 bge-small-zh-v1.5
    #[serde(default = "default_embed_local_model")]
    pub local_model: String,
}

fn default_embed_model() -> String {
    "qwen3.7-text-embedding".to_string()
}

fn default_embed_base_url() -> Option<String> {
    Some(
        "https://llm-q0265e4he9m0qs23.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"
            .to_string(),
    )
}

fn default_embed_api_key_env() -> String {
    "BAILIAN_API_KEY".to_string()
}

fn default_embed_local_model() -> String {
    String::from("bge-small-zh-v1.5")
}

impl Default for EmbedSection {
    fn default() -> Self {
        // v29 用户确认的实际可用配置：阿里百炼兼容端点。schema 缺省与模板
        // 同源（model/base_url/api_key_env 三键），合并回退时嵌入仍可用。
        Self {
            model: "qwen3.7-text-embedding".to_string(),
            base_url: Some(
                "https://llm-q0265e4he9m0qs23.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"
                    .to_string(),
            ),
            api_key: None,
            api_key_env: "BAILIAN_API_KEY".to_string(),
            max_concurrency: None,
            provider: EmbedProvider::default(),
            local_model: default_embed_local_model(),
        }
    }
}

/// 搜索引擎类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SearchEngineType {
    /// BM25 全文搜索
    #[serde(rename = "text")]
    Text,
    /// 向量语义搜索
    #[serde(rename = "semantic")]
    Semantic,
    /// RRF 混合排序
    #[serde(rename = "hybrid")]
    Hybrid,
}
