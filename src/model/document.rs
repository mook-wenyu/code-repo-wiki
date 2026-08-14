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
    /// 父页面标题（v0.9 W1：repowiki.documents 自定义页的 parent——决定
    /// _toc 挂载层级；空 = 顶层全局文档）。serde default 兼容旧快照。
    #[serde(default)]
    pub parent: String,
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
    /// 模块名（模块卡）；项目卡背负固定卡片名——Spec 卡=`project::spec`、
    /// TechStack 卡=`project::tech-stack`（见 CardKind 注释）
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
    /// 设计意图与关键权衡（LLM 生成的描述性字段，A8 幻觉缓解契约）
    ///
    /// 生成域（card.rs/prompt.rs）负责写入；渲染端（render_knowledge_card）
    /// 非空时输出「设计意图」节；语义 lint 提取该节做调用/依赖声称的
    /// 跨页交叉校验。serde default 兼容旧卡与旧快照（旧产物无此字段）。
    #[serde(default)]
    pub design_rationale: Option<String>,
    /// 人工修改反向同步记录：被人工编辑的文档路径 + 内容摘要，
    /// 下次生成时作为 LLM 输入提示（"有人工修改待同步"）
    #[serde(default)]
    pub pending_manual_edits: Vec<String>,
    /// 本模块涉及的实体级特征名（演进计划 T3.3：特征追溯，
    /// 由生成管道从 graph.features 与模块实体的交集回填，不经过 LLM）
    #[serde(default)]
    pub features: Vec<String>,
    /// 卡片类型（serde default 兼容旧卡旧快照：缺省=模块卡）
    #[serde(default)]
    pub card_kind: CardKind,
    /// Spec 卡的结构化规约分类（仅 card_kind==Spec 时非空；模块卡恒空）
    #[serde(default)]
    pub spec_categories: Vec<SpecCategory>,
}

/// 知识卡片类型（对齐 Qoder 三类：架构文档 / 代码规约 Spec / 技术栈）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CardKind {
    /// 模块架构卡（既有模块卡，= Qoder 架构文档类）
    #[default]
    #[serde(rename = "module")]
    Module,
    /// 项目级代码规约卡（全项目一张）
    #[serde(rename = "spec")]
    Spec,
    /// 项目级技术栈卡（全项目一张）
    #[serde(rename = "tech-stack")]
    TechStack,
}

/// 规约分类（Spec 卡专用：分类名 + 规约条目列表）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpecCategory {
    pub name: String,
    pub items: Vec<SpecItem>,
}

/// 规约条目：rule=规约内容，source=来源文件路径（防幻觉锚定）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecItem {
    pub rule: String,
    #[serde(default)]
    pub source: String,
}

/// 项目卡输入文件名集合（增量检测用，basename 匹配）：
/// 依赖清单（techstack.rs 解析）∪ 规约文件（generate::project_card::SPEC_FILES）。
///
/// 放在 model 层的理由（依赖方向契约）：模块依赖方向 generate→incremental
/// 已存在（run_generation_filtered 消费 IncrementalResult），incremental 不得
/// 反向引用 generate（会成环）；本常量是 incremental（指纹检测）与 generate
/// （重生成判定）的共同输入，置于两者共同依赖的底层 model 最合适。
/// 与 techstack::MANIFEST_FILES / project_card::SPEC_FILES 的一致性由
/// tests/test_project_cards.rs 的断言守护。
pub const PROJECT_CARD_INPUT_FILES: &[&str] = &[
    // 依赖清单（techstack.rs MANIFEST_FILES）
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    // 规约文件（project_card.rs SPEC_FILES；docs/glossary.md 的 basename 是
    // glossary.md——SPEC_FILES 里是 "docs/glossary.md"，basename 匹配用
    // glossary.md 覆盖，两处都列出）
    "AGENTS.md",
    ".editorconfig",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    "CONTRIBUTING.md",
    "glossary.md",
];

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧卡/旧快照（无 card_kind / spec_categories 字段）反序列化默认
    /// 为模块卡 + 空分类，保证旧产物仍可读（serde default 契约）
    #[test]
    fn test_old_card_json_defaults_to_module_kind() {
        let old = r#"{
            "module_name": "src::config",
            "module_type": "module",
            "summary": "配置模块",
            "key_entities": [],
            "dependencies": [],
            "dependents": [],
            "design_patterns": [],
            "todo_notes": [],
            "related_files": [],
            "coding_spec": null,
            "tech_stack": [],
            "architecture": null,
            "design_rationale": null,
            "pending_manual_edits": [],
            "features": []
        }"#;
        let card: KnowledgeCard = serde_json::from_str(old).unwrap();
        assert_eq!(card.card_kind, CardKind::Module, "缺省应为模块卡");
        assert!(card.spec_categories.is_empty(), "缺省应为空规约分类");
    }

    /// CardKind 的 serde 序列化形态为 kebab-case 字符串
    ///（"module"/"spec"/"tech-stack"），供 _index.json 的 kind 字段消费
    #[test]
    fn test_card_kind_serde_names() {
        assert_eq!(serde_json::to_value(CardKind::Module).unwrap(), "module");
        assert_eq!(serde_json::to_value(CardKind::Spec).unwrap(), "spec");
        assert_eq!(
            serde_json::to_value(CardKind::TechStack).unwrap(),
            "tech-stack"
        );
    }

    /// Spec 卡 JSON 能反序列化出结构化规约分类（分类名 + 条目 + 来源）
    #[test]
    fn test_spec_card_deserializes_spec_categories() {
        let spec = r#"{
            "module_name": "project::spec",
            "module_type": "project",
            "summary": "仓库代码规约",
            "key_entities": [],
            "dependencies": [],
            "dependents": [],
            "design_patterns": [],
            "todo_notes": [],
            "related_files": ["AGENTS.md"],
            "coding_spec": null,
            "tech_stack": [],
            "architecture": null,
            "design_rationale": null,
            "pending_manual_edits": [],
            "features": [],
            "card_kind": "spec",
            "spec_categories": [
                {
                    "name": "提交纪律",
                    "items": [
                        { "rule": "一个逻辑变更一个提交", "source": "AGENTS.md" },
                        { "rule": "中文 commit message" }
                    ]
                }
            ]
        }"#;
        let card: KnowledgeCard = serde_json::from_str(spec).unwrap();
        assert_eq!(card.card_kind, CardKind::Spec);
        assert_eq!(card.spec_categories.len(), 1);
        assert_eq!(card.spec_categories[0].name, "提交纪律");
        assert_eq!(
            card.spec_categories[0].items[0].rule,
            "一个逻辑变更一个提交"
        );
        assert_eq!(card.spec_categories[0].items[0].source, "AGENTS.md");
        // 缺省 source 归空串
        assert_eq!(card.spec_categories[0].items[1].source, "");
    }
}
