use serde::{Deserialize, Serialize};

/// v22 硬编码常量：以下配置项属「算法内部细节 / 无调优需求 / 必填负担」，
/// 从配置文件移除、以代码常量固定，减少用户配置心智负担（用户拍板：
/// 推荐 10 项全部硬编码）。如需调整须改代码重新编译。
pub const LLM_MAX_CONCURRENT: usize = 16;
/// None=模型默认，不随请求发送
pub const EMBED_BATCH_SIZE: usize = 20;
/// 索引目录，相对 output.dir
pub const SEARCH_INDEX_DIR: &str = ".search";
/// 默认搜索引擎：v36 起为 Hybrid（BM25 召回 + 向量语义 + RRF 融合 + 调用链
/// 补全 + 可选 rerank）。个人仓库场景下混合召回显著优于单一引擎，且
/// 无 embed key 时 hybrid 自动降级纯 text（search 层已验证），默认值
/// 不会让无 key 用户受损。
pub const SEARCH_DEFAULT_ENGINE: SearchEngineType = SearchEngineType::Hybrid;
pub const SEARCH_DEFAULT_TOP_K: usize = 10;
/// RRF 融合常数 k（控制排序权重衰减）
pub const SEARCH_RRF_K: f64 = 60.0;
/// BFS 传播变更影响的最大深度
pub const IMPACT_MAX_DEPTH: usize = 3;
/// v30 硬编码常量：傻瓜式全自动（用户拍板「彻底硬编码删字段」）——
/// 以下配置项从配置文件移除、以代码常量固定，用户零配置开箱即用。
pub const OUTPUT_DIR: &str = ".repo-wiki";

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
    /// v36 B1：重排模型配置（[rerank] 段）。缺段即用默认阵营
    ///（百炼 qwen3-rerank，与 embed 同栈同 Key），无 Key 时
    /// hybrid 自动跳过重排（见 lib.rs execute_search）。
    #[serde(default)]
    pub rerank: RerankSection,
    /// 运行时输出目录（serde(skip)：配置文件中不可写，由
    /// load_config_with_output 注入——CLI --output 覆盖或 root 化后的
    /// 绝对路径；None 时由 output_dir() 方法兜底硬编码常量）。
    /// 使用场景：bench 跑分把产物写到隔离目录，不污染真实 .repo-wiki。
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

/// 重排模型配置（v36 B1）：hybrid 融合后在 top-K 候选上做交叉编码器
/// 精排（bi-encoder 召回 + cross-encoder 重排是检索最佳实践的标准形态）。
/// 形状与 EmbedSection 同构（model/base_url/api_key/api_key_env），缺省
/// 阵营与 embed 同栈（百炼兼容端点 + BAILIAN_API_KEY）——用户已配置
/// embed 即可直接使用；无 Key 时 hybrid 跳过重排并告警（不降级搜索）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankSection {
    #[serde(default = "default_rerank_model")]
    pub model: String,
    #[serde(default = "default_rerank_base_url")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_rerank_api_key_env")]
    pub api_key_env: String,
}

fn default_rerank_model() -> String {
    "qwen3-rerank".to_string()
}

fn default_rerank_base_url() -> Option<String> {
    // 与 embed 默认同源（百炼兼容端点）——复用用户已验证的可用配置
    default_embed_base_url()
}

fn default_rerank_api_key_env() -> String {
    "BAILIAN_API_KEY".to_string()
}

impl Default for RerankSection {
    fn default() -> Self {
        Self {
            model: "qwen3-rerank".to_string(),
            base_url: default_embed_base_url(),
            api_key: None,
            api_key_env: "BAILIAN_API_KEY".to_string(),
        }
    }
}
