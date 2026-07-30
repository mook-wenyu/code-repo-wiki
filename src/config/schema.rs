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
}

/// Wiki 基本配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSection {
    pub template: WikiTemplate,
    pub language: String,
}

impl Default for WikiSection {
    fn default() -> Self {
        Self {
            template: WikiTemplate::Architecture,
            language: "zh".to_string(),
        }
    }
}

/// Wiki 模板类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WikiTemplate {
    #[serde(rename = "architecture")]
    Architecture,
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
        Self {
            provider: LlmProviderType::OpenAI,
            model: "gpt-4o".to_string(),
            base_url: None,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            max_concurrent: 4,
            max_tokens: None,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LlmProviderType {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "custom")]
    Custom,
}

/// 输出配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSection {
    pub dir: String,
    pub format: OutputFormat,
}

impl Default for OutputSection {
    fn default() -> Self {
        Self {
            dir: ".repo-wiki".to_string(),
            format: OutputFormat::Markdown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputFormat {
    #[serde(rename = "markdown")]
    Markdown,
}

/// 嵌入模型提供商类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum EmbedProviderType {
    #[default]
    #[serde(rename = "openai")]
    OpenAI,
}

/// 嵌入模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSection {
    /// 是否启用嵌入生成
    pub enabled: bool,
    pub provider: EmbedProviderType,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: String,
    /// 批处理大小（一次 API 调用中最多 embedding 的文本数）
    pub batch_size: usize,
    /// 向量维度（部分本地模型需手动指定）
    pub dimension: Option<usize>,
}

impl Default for EmbedSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: EmbedProviderType::OpenAI,
            model: "text-embedding-3-small".to_string(),
            base_url: None,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            batch_size: 20,
            dimension: None,
        }
    }
}

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
}

impl Default for SearchSection {
    fn default() -> Self {
        Self {
            enabled: true,
            index_dir: ".search".to_string(),
            default_engine: SearchEngineType::Text,
            default_top_k: 10,
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

/// 增量更新配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalSection {
    pub enabled: bool,
    pub strategy: IncrementalStrategy,
}

impl Default for IncrementalSection {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: IncrementalStrategy::GitDiff,
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
