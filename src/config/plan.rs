//! wiki_plan.yaml 前置干预配置系统
//!
//! 用户通过 wiki_plan.yaml 文件控制 LLM 生成的内容方向，
//! 包括全局 notes、Knowledge Card 专用 notes、模块粒度的 template 选择、
//! 文档白名单和扫描范围覆盖。
//!
//! 该文件仅在 config.plan.enabled=true 时生效，路径相对于项目根
//! （当前工作目录）解析，默认 "wiki_plan.yaml"。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use std::path::Path;

use crate::project::ProjectRoot;

/// wiki_plan.yaml 根结构（对齐 Qoder 官方语义，平铺）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiPlan {
    /// 格式版本（当前仅支持 1，缺省视为 1）
    #[serde(default)]
    pub version: Option<u32>,
    /// 全局 notes：追加到所有 system prompt 末尾
    pub notes: Option<String>,
    /// Knowledge Card 专用 notes：追加到卡片 prompt 末尾
    pub knowledgecard: Option<PlanKnowledgeCard>,
    /// 扫描范围覆盖（优先于 config.toml scope）
    pub scope: Option<crate::config::schema::ScopeSection>,
    /// 按模块模式的细致规划（项目扩展）
    #[serde(default)]
    pub sections: Vec<PlanSection>,
    /// 文档白名单（提供时严格只输出列出的页面）
    #[serde(default)]
    pub documents: Vec<PlanDocument>,
}

/// knowledgecard 节
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanKnowledgeCard {
    pub notes: Option<String>,
}

/// 模块级别的规划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSection {
    /// 匹配的模块路径（支持 glob: "src/config/**"）
    pub module_pattern: String,
    /// 该模块使用的模板类型
    pub template_type: PlanTemplateType,
    /// 对该模块 LLM 的额外指导（叠加在全局 notes 之上）
    pub notes: Option<String>,
}

/// 文档模板类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanTemplateType {
    #[serde(rename = "architecture")]
    Architecture,
    #[serde(rename = "prd")]
    Prd,
    #[serde(rename = "api-ref")]
    ApiRef,
}

/// 文档白名单条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDocument {
    /// 文档标题
    pub title: String,
    /// 生成目标描述
    pub goal: String,
    /// 父文档标题（用于层级结构）
    pub parent: Option<String>,
    /// 包含的文件模式（glob）
    #[serde(default)]
    pub include_patterns: Vec<String>,
    /// 排除的文件模式（glob）
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// 针对该文档的额外生成提示（注入该文档的页面 prompt）
    #[serde(default)]
    pub hints: Option<String>,
}


/// 在指定项目根下加载 wiki_plan.yaml
///
/// 计划文件定位基准显式注入：不依赖进程 cwd，watch 常驻进程的
/// cwd 漂移不再改变计划文件解析目标。
pub fn load_plan_at(root: &ProjectRoot, path: &str) -> Result<Option<WikiPlan>> {
    let plan_path = root.join(Path::new(path));
    if !plan_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("读取 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    let plan: WikiPlan = serde_yaml::from_str(&content)
        .with_context(|| format!("解析 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    // 版本校验：缺省视为 1，其余版本一律拒绝，防止未来不兼容格式静默误读
    match plan.version {
        Some(1) | None => {}
        Some(v) => bail!("wiki_plan.yaml 版本 {} 不受支持（当前支持: 1）", v),
    }
    Ok(Some(plan))
}

/// 解析后的生效计划（生成流水线唯一消费视图）
///
/// 生成层不直接接触 wiki_plan.yaml 文件，只依赖本结构，
/// 后续切换配置来源（如远端 plan）时无需改动生成代码。
#[derive(Debug, Clone, Default)]
pub struct ResolvedPlan {
    /// 全局 notes：追加到所有 system prompt 末尾
    pub notes: Option<String>,
    /// Knowledge Card 专用 notes：追加到卡片 prompt 末尾
    pub card_notes: Option<String>,
    /// 文档白名单（Some 时严格只输出列出的页面）
    pub whitelist: Option<Vec<PlanDocument>>,
    /// 按模块模式的细致规划
    pub sections: Vec<PlanSection>,
    /// 扫描范围覆盖（优先于 config.toml scope）
    pub scope_override: Option<crate::config::schema::ScopeSection>,
}


/// 在指定项目根下解析生效计划
///
/// root 注入链路：resolve_plan_at → load_plan_at，计划文件的
/// 解析基准全程显式传递，与进程 cwd 解耦。
pub fn resolve_plan_at(
    root: &ProjectRoot,
    config: &crate::config::schema::WikiConfig,
) -> Result<Option<ResolvedPlan>> {
    if !config.plan.enabled {
        return Ok(None);
    }
    let Some(plan) = load_plan_at(root, &config.plan.path)? else {
        return Ok(None);
    };
    Ok(Some(ResolvedPlan {
        notes: plan.notes,
        card_notes: plan.knowledgecard.and_then(|kc| kc.notes),
        // 空白名单等价于"全部生成"，统一折叠为 None 便于生成层判断
        whitelist: if plan.documents.is_empty() {
            None
        } else {
            Some(plan.documents)
        },
        sections: plan.sections,
        scope_override: plan.scope,
    }))
}
