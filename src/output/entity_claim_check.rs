//! 实体声明防幻觉：生成期校验（第 5 类，与 dependency_check 并列）
//!
//! LLM 生成 Wiki 页的「## 核心实体」小节时可能声称不存在的实体名（编造），
//! 本模块在生成期（wiki.rs 重试循环）对正文声称对照允许集校验：
//! - 允许集 = chunk.entities 的真实实体名（归一后全量匹配，不截断）
//!   ∪ 模块引用名集（模块自身路径 + 依赖模块 + 导入外部 crate + std/core
//!   前缀）的精确命中与「模块名 + ::」前缀命中（跨模块引用/外部调用）
//!   ∪ 同前缀通配符系列名（`xxx_` 概括 `xxx_*`，与 lint 规则 4.5 同语义，
//!   防合法概括被误拦）。
//!
//! 提取复用 lint.rs 的提取器族（claimed_backtick_inner / is_non_entity_section
//! / has_code_extension / extract_entity_claims_with_lines）与 citation 的
//! fence_ranges，保证「什么算实体声称」的判定口径与磁盘级 lint 完全一致
//! （DRY：提取语义单一来源，见 lint.rs 内四处可见性放宽注释）。
//!
//! 纯函数形态：validate_entity_claims + entity_claim_retry_feedback，
//! 供 wiki.rs 重试循环消费（与 dependency_check 同构；本模块不接线磁盘级
//! lint——lint 域另管辖，YAGNI 不提前接线）。

use std::collections::HashSet;

use crate::generate::chunk::Chunk;
use crate::output::lint::{
    claimed_backtick_inner, entity_name_from_signature, has_code_extension, is_non_entity_section,
};

/// 实体声明校验重试上限（第 5 类校验；与 dependency_check 的声明并列，
/// 实际循环上限在 generate::wiki 侧合并取最大）
pub const ENTITY_CLAIM_RETRY_MAX: usize = 3;

/// 单条实体声明违反（编造实体）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityClaimViolation {
    /// LLM 声称的实体名原文（反引号内文）
    pub claimed: String,
    /// entity_name_from_signature 归一后的名字
    pub normalized: String,
}

/// 模块引用名集（规则 a/b 判定的前缀基准）：
/// `module_path.join("::")` ∪ chunk.dependencies ∪ 每条 import 的顶级
/// crate 名 ∪ {"std","core"}。
///
/// 语义：正文里命中这些名字（或其 `::` 子路径）的 backtick 内容是对真实
/// 模块/外部 crate 的引用，不是「核心实体」声称的编造实体，放行。
fn module_reference_names(chunk: &Chunk) -> HashSet<String> {
    let mut names = HashSet::new();
    // 模块自身全路径（如 src::config）——声称为自己模块名的行放行
    names.insert(chunk.module_path.join("::"));
    // 真实模块依赖（Imports ∪ Calls 推导出的跨模块引用）
    names.extend(chunk.dependencies.iter().cloned());
    // 每条导入的顶级 crate 名（如 serde::Serialize → serde）与 std/core
    for import in &chunk.imports {
        if let Some(top) = import.source.split("::").next() {
            names.insert(top.to_string());
        }
    }
    names.insert("std".to_string());
    names.insert("core".to_string());
    names
}

/// 声称原文是否精确命中模块引用名集（规则 a）
fn hit_module_name_exact(inner: &str, ref_names: &HashSet<String>) -> bool {
    ref_names.contains(inner)
}

/// 声称原文是否以「模块引用名 + ::」为前缀（规则 b）
///
/// 覆盖跨模块引用（`src::config::Config`，src::config 在引用集）与外部 crate
/// 子路径（import serde 时声称 `serde::Serialize`）。只前缀到「引用名 + ::」，
/// 更深的子路径自然落在同一前缀之下，不误拦。
fn hit_module_name_prefix(inner: &str, ref_names: &HashSet<String>) -> bool {
    ref_names
        .iter()
        .any(|name| inner.starts_with(&format!("{name}::")))
}

/// 归一后的实体名是否落在 chunk 真实实体名集合（规则 c，全量不截断）
fn hit_entity_name(normalized: &str, entity_names: &HashSet<String>) -> bool {
    entity_names.contains(normalized)
}

/// 通配符系列名（规则 d）：归一后以 `_` 结尾（如 `test_render_` 概括
/// `test_render_*`）且 chunk 存在同前缀实体名 → 放行，防合法概括被误拦。
fn hit_wildcard_series(normalized: &str, entity_names: &HashSet<String>) -> bool {
    if !normalized.ends_with('_') {
        return false;
    }
    let prefix = &normalized[..normalized.len() - 1];
    entity_names.iter().any(|n| n.starts_with(prefix))
}

/// 校验正文的「## 核心实体」实体声称（生成期入口，重试循环消费）
///
/// 遍历方式设计：对正文内容**独立遍历**，逐行用 lint 的 claimed_backtick_inner
/// 剥出反引号原文 inner、再用 entity_name_from_signature 归一，同时拿到
/// (inner, normalized) 对——这样规则 b 的「模块名 + ::」前缀命中（依赖 inner
/// 原文）与规则 c/d 的归一判定可在同一遍完成，避免依赖 lint 提取器
/// extract_entity_claims_with_lines 的剔除副作用（它已在内部剔除了模块名
/// 精确命中行，取不到 inner 原文做前缀判定）。fence 感知复用
/// citation::fence_ranges：围栏内是代码/示例，不参与声称提取（与
/// dependency_check / citation 同基准）。
pub fn validate_entity_claims(content: &str, chunk: &Chunk) -> Vec<EntityClaimViolation> {
    let ref_names = module_reference_names(chunk);
    let entity_names: HashSet<String> = chunk.entities.iter().map(|e| e.name.clone()).collect();
    let fences = crate::output::citation::fence_ranges(content);

    let mut violations = Vec::new();
    let mut offset = 0usize;
    let mut fence_idx = 0usize;
    let mut in_non_entity_section = false;
    for line in content.split('\n') {
        // fence 感知：围栏区间（含开/闭行）整体跳过
        while fence_idx < fences.len() && offset >= fences[fence_idx].1 {
            fence_idx += 1;
        }
        if fence_idx < fences.len() && offset >= fences[fence_idx].0 {
            offset += line.len() + 1;
            continue;
        }
        let t = line.trim();
        // 节标题切换：非实体节（依赖/使用方式）整节跳过——其下 `- \`...\``
        // 是模块引用与使用说明，不是实体声称（与 lint is_non_entity_section 同口径）
        if t.starts_with("## ") {
            in_non_entity_section = is_non_entity_section(t);
        } else if !in_non_entity_section {
            let Some(inner) = claimed_backtick_inner(line) else {
                offset += line.len() + 1;
                continue;
            };
            // 路径形态（src/main.rs 等）是文件引用，不是实体声称（lint 同口径）
            if inner.contains('/') || inner.contains('\\') || has_code_extension(inner) {
                offset += line.len() + 1;
                continue;
            }
            // 归一提前算一次：规则 c/d 与 violation 构造共用（避免重复解析）
            let normalized = entity_name_from_signature(inner);
            // 放行规则 a-d，任一命中即放行；全部不中 → 编造 violation
            if !(hit_module_name_exact(inner, &ref_names)
                || hit_module_name_prefix(inner, &ref_names)
                || normalized
                    .as_deref()
                    .is_some_and(|n| hit_entity_name(n, &entity_names))
                || normalized
                    .as_deref()
                    .is_some_and(|n| hit_wildcard_series(n, &entity_names)))
            {
                violations.push(EntityClaimViolation {
                    claimed: inner.to_string(),
                    normalized: normalized.unwrap_or_default(),
                });
            }
        }
        offset += line.len() + 1;
    }
    violations
}

/// 将违反列表格式化为重试反馈文本（注入 LLM 输入，仿 dependency_retry_feedback
/// 的格式与语气）
pub fn entity_claim_retry_feedback(violations: &[EntityClaimViolation]) -> String {
    let mut lines = String::from("上一版输出存在不存在的实体，请修正后重新输出完整文档：\n");
    for v in violations {
        lines.push_str(&format!(
            "- `{}` — 该实体不存在于本模块，疑似编造\n",
            v.claimed
        ));
    }
    lines.push_str(
        "要求：「核心实体」小节只列出输入「实体引用清单」节中给出的实体名\
         （名称与输入一致，不得添加输入中不存在的名字）；概括同名测试系列时\
         可写 `xxx_*` 形式；找不到时列表可以为空。",
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::{Entity, ImportStmt};
    use std::path::PathBuf;

    /// 仿 dependency_check tests 的 make_chunk，含 entities 构造（name/kind 必填，
    /// 其余默认）
    fn make_entity(name: &str, kind: &str) -> Entity {
        Entity {
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 10,
            doc_comment: None,
            signature: None,
            visibility: None,
        }
    }

    fn make_chunk(entities: &[&str], deps: &[&str], import_sources: &[&str]) -> Chunk {
        Chunk {
            module_path: vec!["src".into(), "a".into()],
            entities: entities.iter().map(|n| make_entity(n, "fn")).collect(),
            imports: import_sources
                .iter()
                .map(|s| ImportStmt {
                    source: s.to_string(),
                    alias: None,
                    line: 1,
                })
                .collect(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            file_paths: vec![PathBuf::from("src/a/mod.rs")],
            entity_sources: vec![],
        }
    }

    /// 本模块实体命中放行（规则 c）
    #[test]
    fn test_claim_hitting_entity_passes() {
        let chunk = make_chunk(&["render", "parse"], &[], &[]);
        let content = "## 核心实体\n- `render` — 渲染\n- `parse` — 解析\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "本模块真实实体不应判违反: {violations:?}"
        );
    }

    /// 函数带括号形态放行（规则 c 归一）：`fn_name()` 归一到 `fn_name`（真实
    /// LLM 输出核心实体节的标准形态，prompt.rs 模板 `- \`fn_name()\` — 描述`）
    #[test]
    fn test_function_with_parens_passes() {
        let chunk = make_chunk(&["render", "parse"], &[], &[]);
        let content = "## 核心实体\n- `render()` — 渲染函数\n- `parse()` — 解析函数\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "函数带括号形态归一到实体名应放行: {violations:?}"
        );
    }

    /// 关联方法形态放行（规则 c 归一）：`Foo::bar` 归一到 `bar`（impl 内
    /// method 实体为纯名，Rust parser function_item 统一记实体名）
    #[test]
    fn test_associated_method_form_passes() {
        let chunk = make_chunk(&["Foo", "bar"], &[], &[]);
        let content = "## 核心实体\n- `Foo::bar` — 关联方法\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "关联方法形态归一到 method 实体名应放行: {violations:?}"
        );
    }

    /// 编造名拦截：声称不存在的实体名 → 违反
    #[test]
    fn test_fabricated_claim_flagged() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 核心实体\n- `render` — 真实\n- `nonexistent_thing` — 编造\n";
        let violations = validate_entity_claims(content, &chunk);
        assert_eq!(violations.len(), 1, "应拦截编造实体: {violations:?}");
        assert_eq!(violations[0].claimed, "nonexistent_thing");
        assert_eq!(violations[0].normalized, "nonexistent_thing");
    }

    /// 通配符系列名放行（规则 d）：chunk 含 test_render_one，
    /// 声称 `test_render_*` 概括系列 → 放行
    #[test]
    fn test_wildcard_series_name_passes() {
        let chunk = make_chunk(&["test_render_one"], &[], &[]);
        let content = "## 核心实体\n- `test_render_*` — 渲染相关测试系列\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "同前缀通配符系列名不应判违反: {violations:?}"
        );
    }

    /// 通配符但无同前缀实体 → 拦截（非真实系列概括）
    #[test]
    fn test_wildcard_series_without_base_flagged() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 核心实体\n- `fabricated_foo_*` — 无本模块依据的系列\n";
        let violations = validate_entity_claims(content, &chunk);
        assert_eq!(
            violations.len(),
            1,
            "无同前缀基实的系列应拦截: {violations:?}"
        );
    }

    /// 跨模块前缀放行（规则 b）：chunk.dependencies 含 "src::config"，
    /// 声称 `src::config::Config` → 放行
    #[test]
    fn test_cross_module_prefix_passes() {
        let chunk = make_chunk(&["render"], &["src::config"], &[]);
        let content = "## 核心实体\n- `src::config::Config` — 引用依赖模块的实体\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "跨模块引用（模块名+::前缀）不应判违反: {violations:?}"
        );
    }

    /// imports 顶级 crate 放行：import serde 时声称 `serde`（规则 a 顶级名）
    /// 与 `serde::Serialize`（规则 b 前缀）→ 均放行
    #[test]
    fn test_import_top_level_crate_passes() {
        let chunk = make_chunk(&["render"], &[], &["serde::Serialize"]);
        let content = "## 核心实体\n- `serde` — 外部 crate\n- `serde::Serialize` — 其子路径\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "import 顶级 crate 及其子路径不应判违反: {violations:?}"
        );
    }

    /// 模块自身全路径精确命中放行（规则 a）：module_path src::a
    #[test]
    fn test_own_module_name_exact_passes() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 核心实体\n- `src::a` — 本模块名\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "本模块名精确命中不应判违反: {violations:?}"
        );
    }

    /// 非实体节不提取：`## 使用方式` 下的 `- \`fake\`` 不报
    #[test]
    fn test_non_entity_section_not_extracted() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 使用方式\n- `fake` — 使用说明\n\n## 核心实体\n- `render`\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "使用方式节内的 `fake` 是用法不是实体声称: {violations:?}"
        );
    }

    /// 依赖节内的声称不提取（同 is_non_entity_section 口径）
    #[test]
    fn test_dependency_section_not_extracted() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 依赖关系\n- `fake_crate` — 不是实体声称\n\n## 核心实体\n- `render`\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "依赖节内的 backtick 不是实体声称: {violations:?}"
        );
    }

    /// fence 内声称不提取：代码围栏里的 `- \`fake\`` 是示例代码
    #[test]
    fn test_fenced_claim_not_extracted() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content =
            "## 核心实体\n- `render`\n\n```rust\n// 示例\n- `fake`\n```\n- `parse` — 编造\n";
        let violations = validate_entity_claims(content, &chunk);
        assert_eq!(violations.len(), 1, "围栏内 fake 不应提取: {violations:?}");
        assert_eq!(violations[0].claimed, "parse");
    }

    /// 路径形态不提取：`src/main.rs` 是文件引用不是实体声称
    #[test]
    fn test_path_form_not_extracted() {
        let chunk = make_chunk(&["render"], &[], &[]);
        let content = "## 核心实体\n- `src/main.rs` — 文件引用\n";
        let violations = validate_entity_claims(content, &chunk);
        assert!(
            violations.is_empty(),
            "路径形态声称（文件引用）不应判违反: {violations:?}"
        );
    }

    /// feedback 格式断言
    #[test]
    fn test_entity_claim_retry_feedback_format() {
        let violations = vec![
            EntityClaimViolation {
                claimed: "nonexistent_thing".into(),
                normalized: "nonexistent_thing".into(),
            },
            EntityClaimViolation {
                claimed: "ghost_fn".into(),
                normalized: "ghost_fn".into(),
            },
        ];
        let feedback = entity_claim_retry_feedback(&violations);
        // 首行要求
        assert!(feedback.contains("上一版输出存在不存在的实体，请修正后重新输出完整文档："));
        // 每条「- `claimed` — 该实体不存在于本模块，疑似编造」
        assert!(feedback.contains("- `nonexistent_thing` — 该实体不存在于本模块，疑似编造"));
        assert!(feedback.contains("- `ghost_fn` — 该实体不存在于本模块，疑似编造"));
        // 结尾要求
        assert!(feedback.contains("「核心实体」小节只列出输入「实体引用清单」节中给出的实体名"));
        assert!(feedback.contains("可写 `xxx_*` 形式"));
        assert!(feedback.contains("找不到时列表可以为空"));
    }
}
