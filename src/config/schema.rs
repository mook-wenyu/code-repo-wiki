use serde::{Deserialize, Serialize};

/// 全局配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiConfig {
    #[serde(default)]
    pub wiki: WikiSection,
    #[serde(default)]
    pub scope: ScopeSection,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub embed: EmbedSection,
    #[serde(default)]
    pub output: OutputSection,
    #[serde(default)]
    pub incremental: IncrementalSection,
    #[serde(default)]
    pub search: SearchSection,
    #[serde(default)]
    pub plan: PlanConfig,
}

/// Wiki 基本配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSection {
    pub language: String,
    #[serde(default)]
    pub expand_languages: Vec<String>,
}

impl Default for WikiSection {
    fn default() -> Self {
        Self {
            language: "zh".to_string(),
            expand_languages: Vec::new(),
        }
    }
}

/// 扫描范围配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeSection {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Default for ScopeSection {
    fn default() -> Self {
        Self {
            include: vec!["src/**".to_string(), "lib/**".to_string()],
            exclude: vec![
                "**/test/**".to_string(),
                "**/vendor/**".to_string(),
                "target/**".to_string(),
                "**/node_modules/**".to_string(),
            ],
        }
    }
}

/// LLM 提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSection {
    pub provider: LlmProviderType,
    pub model: String,
    pub base_url: Option<String>,
    /// 直接指定 API Key（优先级高于 api_key_env）
    pub api_key: Option<String>,
    /// 从环境变量读取 API Key（当 api_key 为 None 时使用）
    pub api_key_env: String,
    pub max_concurrent: usize,
    /// 输出最大 token 数（由模型上下文窗口决定）
    pub max_tokens: Option<u32>,
    /// 生成温度（0.0~2.0，默认由模型决定）
    pub temperature: Option<f32>,
}

impl Default for LlmSection {
    fn default() -> Self {
        // v17 t05：默认值统一到模板阵营（default-config.toml）——schema 缺省
        // 填充与模板一致，极简配置（缺 [llm] 段）用户落 DeepSeek 而非 OpenAI
        Self {
            provider: LlmProviderType::OpenAI,
            model: "deepseek-v4-flash".to_string(),
            base_url: Some("https://api.deepseek.com/v1".to_string()),
            api_key: None,
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            max_concurrent: 4,
            max_tokens: None,
            temperature: None,
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

/// 输出配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSection {
    pub dir: String,
}

impl Default for OutputSection {
    fn default() -> Self {
        Self {
            dir: ".repo-wiki".to_string(),
        }
    }
}

/// 嵌入模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSection {
    /// 是否启用嵌入生成
    pub enabled: bool,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: String,
    /// 批处理大小（一次 API 调用中最多 embedding 的文本数）
    pub batch_size: usize,
}

impl Default for EmbedSection {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "text-embedding-3-small".to_string(),
            base_url: None,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            batch_size: 20,
        }
    }
}

fn default_rrf_k() -> usize { 60 }

/// 搜索引擎配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSection {
    /// 是否在 generate 后自动构建搜索索引
    pub enabled: bool,
    /// 索引存储目录（相对于 output.dir）
    pub index_dir: String,
    /// 默认搜索引擎
    pub default_engine: SearchEngineType,
    /// 默认返回结果数
    pub default_top_k: usize,
    /// RRF 合并参数 k（控制排序权重衰减）
    #[serde(default = "default_rrf_k")]
    pub rrf_k: usize,
}

impl Default for SearchSection {
    fn default() -> Self {
        Self {
            enabled: true,
            index_dir: ".search".to_string(),
            default_engine: SearchEngineType::Text,
            default_top_k: 10,
            rrf_k: default_rrf_k(),
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

fn default_max_depth() -> usize { 3 }

/// 增量更新配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalSection {
    pub enabled: bool,
    pub strategy: IncrementalStrategy,
    /// BFS 传播变更影响的最大深度
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

impl Default for IncrementalSection {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: IncrementalStrategy::GitDiff,
            max_depth: default_max_depth(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IncrementalStrategy {
    #[serde(rename = "git-diff")]
    GitDiff,
    #[serde(rename = "file-watch")]
    FileWatch,
}

/// wiki_plan.yaml 前置干预配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanConfig {
    pub enabled: bool,
    #[serde(default = "default_plan_path")]
    pub path: String,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self { enabled: false, path: "wiki_plan.yaml".to_string() }
    }
}

fn default_plan_path() -> String { "wiki_plan.yaml".to_string() }
