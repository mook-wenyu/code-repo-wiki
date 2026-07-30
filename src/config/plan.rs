//! wiki_plan.yaml 前置干预配置系统
//!
//! 用户通过 wiki_plan.yaml 文件控制 LLM 生成的内容方向，
//! 包括全局 notes、模块粒度的 template 选择和文档白名单。
//!
//! 该文件仅在 config.plan.enabled=true 时生效。

use std::path::Path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// wiki_plan.yaml 的根结构
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiPlan {
    /// 全局 notes：追加到所有 system prompt 末尾
    pub notes: Option<String>,
    /// 按模块模式的细致规划
    #[serde(default)]
    pub sections: Vec<PlanSection>,
    /// 要生成的文档白名单（空 = 全部生成）
    #[serde(default)]
    pub documents: Vec<PlanDocument>,
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
}

/// 从输出目录加载 wiki_plan.yaml
pub fn load_plan(output_dir: &Path) -> Result<Option<WikiPlan>> {
    let plan_path = output_dir.join("wiki_plan.yaml");
    if !plan_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("读取 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    let plan: WikiPlan = serde_yaml::from_str(&content)
        .with_context(|| format!("解析 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    Ok(Some(plan))
}
