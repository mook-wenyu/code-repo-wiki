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
    /// 目录
    TableOfContents,
    /// 模块文档
    ModuleDoc,
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
}

/// 实体摘要（用于 Knowledge Card）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySummary {
    pub name: String,
    pub kind: String,
    pub visibility: String,
    pub doc: Option<String>,
}
