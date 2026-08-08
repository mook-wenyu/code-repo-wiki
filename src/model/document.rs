use serde::{Deserialize, Serialize};

/// 文档类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DocumentKind {
    /// Knowledge Card（给 AI Agent 的结构化摘要）
    KnowledgeCard,
    /// Wiki Page（给人类的叙述性文档）
    WikiPage,
    /// 架构概览
    ArchitectureOverview,
    /// 项目概览（overview.md，独立于模块页生成）
    ///
    /// 决策：DocumentKind 是纯枚举（无 architecture 等可复用字段），
    /// 且 output::wiki_page_path 按 kind 特判文件名（架构概览→architecture.md），
    /// 因此新增独立变体而非复用 ArchitectureOverview，避免概览写错文件名。
    ProjectOverview,
    /// 目录
    TableOfContents,
    /// API 参考（按模块分组列出公开实体）
    ApiReference,
    /// 数据库 Schema 文档（基于 SQL 建表语句生成）
    DatabaseSchema,
}

/// Wiki 文档
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiDocument {
    pub title: String,
    pub kind: DocumentKind,
    pub content: String,
    /// 文档语言（多语言独立生成时写入对应语言目录）
    pub language: String,
    pub module_path: Vec<String>,
    /// 交叉引用链接
    pub references: Vec<Reference>,
    /// 最后更新时间（ISO 8601）
    pub last_updated: String,
    /// 基于的 git 提交短哈希（v32 10.2 基线行）
    ///
    /// 取值 = 生成时 HEAD 提交短哈希（前 8 位）；非 git 仓库或无 HEAD
    /// 时为 None（渲染端省略「基于提交」行）。HEAD 是**非易变信号**——
    /// 同一提交下多次生成值不变，不破坏 test_determinism 的内容级哈希；
    /// 与 llms_txt.rs「内容禁止注入易变时间戳/基线」契约的取舍：时间戳
    /// 每次生成都变（必须归一化），提交哈希只在代码变更时变（恰是页面
    /// 内容应当变化的时刻）。仅供人工核对产物对应的源码版本。
    pub based_on_commit: Option<String>,
    /// 源文件指纹（用于增量更新检测）
    pub fingerprint: Option<String>,
}

/// 交叉引用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub target_title: String,
    pub target_path: String,
    pub relation: String,
}

/// Knowledge Card（给 AI Agent 的结构化格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeCard {
    pub module_name: String,
    pub module_type: String,
    pub summary: String,
    pub key_entities: Vec<EntitySummary>,
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
    pub design_patterns: Vec<String>,
    pub todo_notes: Vec<String>,
    /// 关联的源文件路径（由 chunk 直接填充，不经过 LLM）
    #[serde(default)]
    pub related_files: Vec<String>,
    /// 编码规范（LLM 生成的描述性字段）
    #[serde(default)]
    pub coding_spec: Option<String>,
    /// 技术栈（LLM 生成的描述性字段）
    #[serde(default)]
    pub tech_stack: Vec<String>,
    /// 架构说明（LLM 生成的描述性字段）
    #[serde(default)]
    pub architecture: Option<String>,
    /// 人工修改反向同步记录：被人工编辑的文档路径 + 内容摘要，
    /// 下次生成时作为 LLM 输入提示（"有人工修改待同步"）
    #[serde(default)]
    pub pending_manual_edits: Vec<String>,
    /// 本模块涉及的实体级特征名（演进计划 T3.3：特征追溯，
    /// 由生成管道从 graph.features 与模块实体的交集回填，不经过 LLM）
    #[serde(default)]
    pub features: Vec<String>,
}

/// 实体摘要（用于 Knowledge Card）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySummary {
    pub name: String,
    pub kind: String,
    pub visibility: String,
    pub doc: Option<String>,
    /// 反向链接：源码定位 "文件路径:起始行-结束行"（演进计划 T3.3，
    /// 由生成管道从 chunk 实体回填，不经过 LLM）
    #[serde(default)]
    pub source: Option<String>,
}
