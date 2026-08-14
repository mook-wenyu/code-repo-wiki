//! wiki_plan.yaml 前置干预配置（v0.9 W1 重构：对齐 Qoder 语义，替代旧 [wiki.guide]）
//!
//! 用户通过项目根 `wiki_plan.yaml` 控制 LLM 生成方向：
//! - `repowiki.notes`：Wiki 页生成引导（替代旧 [wiki.guide].notes，新增 author 字段）
//! - `repowiki.template`：模板选择（"" 默认 / "architecture" 架构概览模板）
//! - `repowiki.documents`：自定义文档页面（LLM 按 goal/hints 生成 title 页，
//!   parent 决定 _toc 挂载），与自动模块页并存
//! - `knowledgecard.notes`：卡片生成引导（新增能力）
//! - `knowledgecard.scope`（顶层 scope 为显式别名）：文件扫描范围（.gitignore 语法）
//!
//! 加载契约：文件不存在 = 空 plan（保持默认行为）；文件存在但解析/校验失败 =
//! 显式报错终止，不静默忽略、不兜底。
//!
//! 与旧实现（ed2c5be 前）的差异：旧实现由 config.plan.enabled 开关控制且字段
//! 全 Option 兜底；本次重构删除开关与兜底——文件存在即生效，坏文件即报错。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::project::ProjectRoot;

/// wiki_plan.yaml 文件名（相对项目根解析）
pub const PLAN_PATH: &str = "wiki_plan.yaml";

/// 当前支持的 plan 格式版本（仅支持 1）
pub const PLAN_VERSION: u32 = 1;

/// wiki_plan.yaml 根结构（对齐 Qoder 官方语义，严格按 W1 schema）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiPlan {
    /// 格式版本（仅支持 1；缺省视为 1，显式其它版本在校验时报错）
    #[serde(default = "default_version")]
    pub version: u32,
    /// repowiki 节：Wiki 页生成引导 + 模板 + 自定义文档页面
    #[serde(default)]
    pub repowiki: RepoWikiSection,
    /// knowledgecard 节：卡片生成引导 + 文件范围（官方位置）
    #[serde(default)]
    pub knowledgecard: KnowledgeCardSection,
    /// 顶层 scope 别名：兼容历史 wiki_plan.yaml 的顶层 scope 形态。
    /// 显式 alias 而非静默兜底——两层都提供时以 knowledgecard.scope 为准。
    #[serde(default)]
    pub scope: Option<PlanScope>,
}

/// repowiki 节
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoWikiSection {
    /// 模板：""（默认）| "architecture"；其他值在 serde 解析时报错
    #[serde(default)]
    pub template: PlanTemplate,
    /// 全局生成引导（替代旧 [wiki.guide].notes；每条含 text + author）
    #[serde(default)]
    pub notes: Vec<PlanNote>,
    /// 自定义文档页面（与自动模块页并存；title 不覆盖模块页标题）
    #[serde(default)]
    pub documents: Vec<PlanDocument>,
}

/// knowledgecard 节
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KnowledgeCardSection {
    /// 卡片生成引导（新增能力）
    #[serde(default)]
    pub notes: Vec<PlanNote>,
    /// 文件范围（官方位置）；Option 区分「未提供」与「提供空列表」——
    /// 未提供时回落顶层 scope 别名，两者都未提供 = 无范围覆盖。
    #[serde(default)]
    pub scope: Option<PlanScope>,
}

/// 生成引导注记（v0.9 新增 author 字段，缺省空串兼容旧格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNote {
    /// 引导文本
    pub text: String,
    /// 作者（可空）
    #[serde(default)]
    pub author: String,
}

/// 自定义文档页面条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDocument {
    /// 页面标题（LLM 生成 title 页）
    pub title: String,
    /// 生成目标描述（LLM 撰写页面的依据）
    pub goal: String,
    /// 父页面标题（可选，决定 _toc 挂载层级；空 = 顶层全局文档）
    #[serde(default)]
    pub parent: String,
    /// 额外生成提示（可选）
    #[serde(default)]
    pub hints: String,
}

/// 文件扫描范围（.gitignore 语法，含 glob 合法性校验）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanScope {
    /// 包含模式白名单（空 = 不限制，全部纳入）
    #[serde(default)]
    pub include: Vec<String>,
    /// 排除模式黑名单
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// repowiki.template 模板选择：支持 ""（默认）、"architecture" 与
/// "product_requirement"；其他值在 serde 解析时报错（unknown variant），
/// 不静默兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlanTemplate {
    /// 默认模板：模块页用标准 wiki_page_prompt
    #[default]
    #[serde(rename = "")]
    Default,
    /// 架构概览模板：复用 architecture_overview_prompt
    #[serde(rename = "architecture")]
    Architecture,
    /// 产品需求模板：模块页按产品需求格式输出
    #[serde(rename = "product_requirement")]
    ProductRequirement,
}

/// 在指定项目根下加载并校验 wiki_plan.yaml
///
/// 计划文件定位基准显式注入：不依赖进程 cwd（watch 常驻进程 cwd 漂移
/// 不再改变计划文件解析目标）。
/// - 文件不存在 → Ok(None)（空 plan，保持默认行为）；
/// - 文件存在但解析/校验失败 → Err（显式报错终止，不静默忽略、不兜底）。
pub fn load_plan_at(root: &ProjectRoot, path: &str) -> Result<Option<WikiPlan>> {
    let plan_path = root.join(Path::new(path));
    if !plan_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("读取 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    let plan: WikiPlan = serde_yaml_ng::from_str(&content)
        .with_context(|| format!("解析 wiki_plan.yaml 失败: {}", plan_path.display()))?;
    validate_plan(&plan)?;
    Ok(Some(plan))
}

/// 校验 wiki_plan.yaml 语义合法性（解析层已拦字段类型与 template 枚举；
/// 此处补 version 与 scope 模式语法两项跨字段校验）
fn validate_plan(plan: &WikiPlan) -> Result<()> {
    // 版本校验：仅支持 1（缺省视为 1）——其余版本一律拒绝，
    // 防止未来不兼容格式被静默误读。
    if plan.version != PLAN_VERSION {
        bail!(
            "wiki_plan.yaml 版本 {} 不受支持（当前支持: {}）",
            plan.version,
            PLAN_VERSION
        );
    }
    // scope 模式语法校验（.gitignore 语法，含 glob 合法性）：
    // 语法错误在解析期拦截，避免运行期扫描静默错漏。
    let scope = plan.knowledgecard.scope.as_ref().or(plan.scope.as_ref());
    if let Some(scope) = scope {
        validate_scope_patterns(scope)?;
    }
    Ok(())
}

/// scope include/exclude 模式语法校验：用 ignore::gitignore::GitignoreBuilder
/// 逐条加载——非法 glob（未闭合 alternate 组等）在 add_line 时报错，一并拦截。
/// 校验基准与运行期扫描的匹配语义同源（ignore 底层 globset），保证
/// 「解析期认为合法」=「运行期可匹配」，不出现解析通过但静默不匹配的错漏。
/// 注：gitignore 对未闭合 `[` 本身是宽容的（视为字面量），此处不额外收紧。
fn validate_scope_patterns(scope: &PlanScope) -> Result<()> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(".");
    for pat in scope.include.iter().chain(scope.exclude.iter()) {
        builder
            .add_line(None, pat)
            .with_context(|| format!("wiki_plan.yaml scope 模式语法错误: {pat:?}"))?;
    }
    Ok(())
}

/// 解析后的生效计划（生成流水线唯一消费视图）
///
/// 生成层不直接接触 wiki_plan.yaml 文件，只依赖本结构——后续切换配置
/// 来源（如远端 plan）时无需改动生成代码。
#[derive(Debug, Clone, Default)]
pub struct ResolvedPlan {
    /// repowiki 全局生成引导（替代旧 guide.notes；含 author）
    pub notes: Vec<PlanNote>,
    /// knowledgecard 卡片生成引导（新增能力）
    pub card_notes: Vec<PlanNote>,
    /// 模板选择（""=默认 | "architecture"=架构概览模板）
    pub template: PlanTemplate,
    /// 自定义文档页面（与自动模块页并存）
    pub documents: Vec<PlanDocument>,
    /// 文件范围覆盖（plan 提供时生效；knowledgecard.scope 优先于顶层 scope 别名）
    pub scope_override: Option<PlanScope>,
}

/// 在指定项目根下解析生效计划（root 注入链路：resolve_plan_at → load_plan_at，
/// 计划文件解析基准全程显式传递，与进程 cwd 解耦）。
///
/// 文件不存在 → None（空 plan，保持默认行为）；解析/校验失败 → Err。
pub fn resolve_plan_at(root: &ProjectRoot) -> Result<Option<ResolvedPlan>> {
    let Some(plan) = load_plan_at(root, PLAN_PATH)? else {
        return Ok(None);
    };
    // 顶层 scope 为显式别名：官方位置 knowledgecard.scope 优先，
    // 未提供时回落到顶层 scope（两层都提供时以官方位置为准）。
    let scope_override = plan.knowledgecard.scope.or(plan.scope);
    Ok(Some(ResolvedPlan {
        notes: plan.repowiki.notes,
        card_notes: plan.knowledgecard.notes,
        template: plan.repowiki.template,
        documents: plan.repowiki.documents,
        scope_override,
    }))
}

fn default_version() -> u32 {
    PLAN_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ProductRequirement 变体 serde 解析：YAML/JSON 的字面量
    /// "product_requirement" ⇔ 变体可双向转换；未知值仍报错（不静默兜底）
    #[test]
    fn test_product_requirement_variant_serde() {
        // 反序列化（YAML 侧：字面量 → 变体）
        let tpl: PlanTemplate = serde_yaml_ng::from_str("product_requirement").unwrap();
        assert_eq!(tpl, PlanTemplate::ProductRequirement);
        // 序列化（serde_json 值 → 字面量）
        assert_eq!(
            serde_json::to_value(PlanTemplate::ProductRequirement).unwrap(),
            json!("product_requirement")
        );
        // 默认与既有变体不受影响
        assert_eq!(PlanTemplate::default(), PlanTemplate::Default);
        assert_eq!(
            serde_json::to_value(PlanTemplate::Architecture).unwrap(),
            json!("architecture")
        );
        // 未知值仍报错（unknown variant），不静默兜底
        assert!(
            serde_yaml_ng::from_str::<PlanTemplate>("product_software").is_err(),
            "未知 template 值应报错"
        );
    }

    /// 整块 wiki_plan.yaml 含 repowiki.template: "product_requirement" 时
    /// 能完整解析出 PlanTemplate（接入字段级消费的证据）
    #[test]
    fn test_template_field_parses_product_requirement() {
        let yaml = "repowiki:\n  template: product_requirement\n";
        let plan: WikiPlan = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(plan.repowiki.template, PlanTemplate::ProductRequirement);
    }
}
