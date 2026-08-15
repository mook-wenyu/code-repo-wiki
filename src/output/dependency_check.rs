//! 依赖关系防幻觉双闸之一：生成期校验
//!
//! LLM 生成 Wiki 页的「## 依赖关系」小节时可能声称不存在的模块（虚构依赖）。
//! 本模块在生成期（wiki.rs 重试循环）解析正文声称并对照允许集校验：
//! - 允许集 = chunk.dependencies（真实模块依赖，Imports ∪ Calls 推导）
//!   ∪ 每条 ImportStmt.source 的顶级 crate 名（实际导入的外部包）
//!   ∪ {"std"/"core"} 前缀（Rust 标准库）
//!   ∪ 诚实标记行「（信息不足）」（显式标注未知，不构成声称）。
//!
//! 另一个用途：纯函数形态（`extract_dependency_claims` + `validate_claims`）
//! 保持 lint 可复用——磁盘级 lint 当前未接线（等 lint 域稳定，YAGNI 不提前
//! 接线）；如需 lint 复用，以自身的模块清单（图/快照）构建允许集，对产物
//! 页面做同样的防幻觉检查。

use std::collections::BTreeSet;

use crate::generate::chunk::Chunk;

/// 单条依赖声称违反
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyViolation {
    /// LLM 声称的模块名
    pub claimed: String,
    pub reason: DependencyViolationReason,
}

/// 违反原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyViolationReason {
    /// 模块真实存在（代码库内）但不在本模块依赖列表中
    NotADependency,
    /// 模块既不在项目内、也未被导入，疑似编造
    UnknownExternal,
}

/// 从正文提取「## 依赖关系」节的模块声称
///
/// 节范围 = `## 依赖关系`（及变体）标题到下一个 `## ` 标题之间；
/// 只取 `- 模块名` 列表行，剥掉 backtick 与「—」后的说明。
/// 诚实标记行（含「信息不足」）跳过——显式标注未知不算声称。
/// fence 感知：代码围栏区间内的 `- xxx` 是示例代码而非声称（示例常写
/// 假模块名），与 citation::extract_citations 同基准跳过围栏。
pub fn extract_dependency_claims(content: &str) -> Vec<String> {
    let fences = crate::output::citation::fence_ranges(content);
    let mut claims = Vec::new();
    let mut in_section = false;
    let mut offset = 0usize;
    let mut fence_idx = 0usize;
    for line in content.split('\n') {
        while fence_idx < fences.len() && offset >= fences[fence_idx].1 {
            fence_idx += 1;
        }
        // 围栏区间（含开/闭行）整体跳过：围栏内是代码/示例，不参与
        // 节标题判定也不提取声称
        if fence_idx < fences.len() && offset >= fences[fence_idx].0 {
            offset += line.len() + 1;
            continue;
        }
        let t = line.trim();
        if t.starts_with("## ") {
            in_section = is_dependency_section(t);
        } else if in_section && let Some(item) = t.strip_prefix("- ") {
            // 诚实标记行（含「信息不足」）跳过——显式标注未知不算声称
            if !item.contains("信息不足") {
                let name = claim_name_from_line(item);
                if !name.is_empty() {
                    claims.push(name);
                }
            }
        }
        offset += line.len() + 1;
    }
    claims
}

/// 依赖小节标题判定（精确档：整标题匹配，不再 contains）
///
/// 旧版 `contains("依赖")` 会把「## 依赖注入实现」这类 DI 主题节误判为依赖节，
/// 其下所有 `- ` 行（Spring/Dagger 等）变依赖声称 → UnknownExternal 误报，
/// 白费一次 LLM 重试。只认依赖节的确切标题变体。
fn is_dependency_section(heading: &str) -> bool {
    let title = heading.trim_start_matches('#').trim().to_lowercase();
    matches!(
        title.as_str(),
        "依赖" | "依赖关系" | "dependencies" | "dependency"
    )
}

/// 从单行声称提取模块名（`- 名称` 或 `- 名称 — 说明` / `- \`名称\` — 说明`）
fn claim_name_from_line(item: &str) -> String {
    let name = item
        .trim()
        .split("—")
        .next()
        .unwrap_or("")
        .split(" - ")
        .next()
        .unwrap_or("")
        .trim();
    name.trim_matches('`').trim().to_string()
}

/// 构建允许集：chunk.dependencies ∪ 每条导入的顶级 crate 名
/// ∪ {"std"/"core"}（Rust 标准库前缀，与 is_allowed 的硬编码兜底一致——
/// 加入集合使重试反馈的允许集清单也能列出标准库，避免反馈与判定脱节）
fn build_allowed_set(chunk: &Chunk) -> BTreeSet<String> {
    let mut allowed: BTreeSet<String> = chunk.dependencies.iter().cloned().collect();
    for import in &chunk.imports {
        // 导入形如 "serde::Serialize" / "std::collections::HashMap"：
        // 顶级段（serde/std）是真实外部依赖名，声明其任意子路径都合法
        if let Some(top) = import.source.split("::").next() {
            allowed.insert(top.to_string());
        }
    }
    allowed.insert("std".to_string());
    allowed.insert("core".to_string());
    allowed
}

/// 声称是否落在允许集内
fn is_allowed(claim: &str, allowed: &BTreeSet<String>) -> bool {
    if allowed.contains(claim) {
        return true;
    }
    // 顶级 crate 前缀命中：导入 serde 时声称 serde::Serialize 合法
    if let Some(top) = claim.split("::").next()
        && allowed.contains(top)
    {
        return true;
    }
    // std/core 前缀（Rust 标准库）
    claim == "std" || claim == "core" || claim.starts_with("std::") || claim.starts_with("core::")
}

/// 内部模块形态判定：顶层为 crate/src/self/super → 代码库内部引用
/// （不是真实依赖时判 NotADependency；外部编造名判 UnknownExternal）
fn looks_like_internal_module(claim: &str) -> bool {
    let top = claim.split("::").next().unwrap_or(claim);
    matches!(top, "crate" | "src" | "self" | "super")
}

/// 纯函数校验（生成期校验的判定核心；磁盘级 lint 当前未接线，如需 lint
/// 复用可自行构建允许集调用）：claims 对照 allowed 集合出违反
pub fn validate_claims<'a>(
    claims: impl IntoIterator<Item = &'a str>,
    allowed: &BTreeSet<String>,
) -> Vec<DependencyViolation> {
    claims
        .into_iter()
        .filter(|c| !c.is_empty())
        .filter(|c| !is_allowed(c, allowed))
        .map(|c| DependencyViolation {
            claimed: c.to_string(),
            reason: if looks_like_internal_module(c) {
                DependencyViolationReason::NotADependency
            } else {
                DependencyViolationReason::UnknownExternal
            },
        })
        .collect()
}

/// 校验正文的「## 依赖关系」声称（生成期入口，重试循环消费）
pub fn validate_dependencies(content: &str, chunk: &Chunk) -> Vec<DependencyViolation> {
    let allowed = build_allowed_set(chunk);
    validate_claims(
        extract_dependency_claims(content)
            .iter()
            .map(|s| s.as_str()),
        &allowed,
    )
}

/// 将违反列表格式化为重试反馈文本（注入 LLM 输入）
///
/// 反馈附「允许集清单」：由 build_allowed_set(chunk) 构造（BTreeSet，
/// 已排序、确定性输出），告诉 LLM 只能列哪些模块，从源头杜绝编造。
pub fn dependency_retry_feedback(violations: &[DependencyViolation], chunk: &Chunk) -> String {
    let mut lines =
        String::from("上一版输出存在虚构或不存在的依赖关系，请修正后重新输出完整文档：\n");
    for v in violations {
        let reason = match v.reason {
            DependencyViolationReason::NotADependency => {
                "该模块存在但不在本模块的依赖列表中".to_string()
            }
            DependencyViolationReason::UnknownExternal => {
                "该模块既不在项目模块中、也未被本模块导入，疑似编造".to_string()
            }
        };
        lines.push_str(&format!("- `{}` — {}\n", v.claimed, reason));
    }
    // 允许集清单（BTreeSet 已排序，确定性、去重输出；真实可以引用的上限）
    lines.push_str("\n本模块的真实依赖/导入清单（只能列出这些，不得添加其他模块）：\n");
    for allowed in build_allowed_set(chunk) {
        lines.push_str(&format!("- {allowed}\n"));
    }
    lines.push_str(
        "要求：「依赖关系」小节只列出输入依赖模块节中给出的模块名以及实际导入的\
         外部 crate（如 tokio、serde），不得添加不存在的模块。",
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::ImportStmt;

    fn make_chunk(deps: &[&str], import_sources: &[&str]) -> Chunk {
        Chunk {
            module_path: vec!["src".into(), "a".into()],
            entities: vec![],
            imports: import_sources
                .iter()
                .map(|s| ImportStmt {
                    source: s.to_string(),
                    alias: None,
                    line: 1,
                })
                .collect(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            file_paths: vec![],
            entity_sources: vec![],
        }
    }

    #[test]
    fn test_extract_dependency_claims() {
        let content = "## 概述\nxxx\n\n## 依赖关系\n- `src::db` — 持久层\n- tokio — 异步运行时\n\n## 使用方式\n用法";
        let claims = extract_dependency_claims(content);
        assert_eq!(claims, vec!["src::db".to_string(), "tokio".to_string()]);
    }

    #[test]
    fn test_extract_skips_honest_marker() {
        let content = "## 依赖关系\n- src::db — 持久层\n- 某外部服务 — （信息不足）\n";
        let claims = extract_dependency_claims(content);
        assert_eq!(claims, vec!["src::db".to_string()]);
    }

    #[test]
    fn test_extract_english_heading() {
        let content = "## Dependencies\n- tokio\n";
        assert_eq!(
            extract_dependency_claims(content),
            vec!["tokio".to_string()]
        );
    }

    /// fence 感知：围栏内的 `- xxx` 是示例代码不是依赖声称（示例常写假模块名，
    /// 旧实现会误提取 → UnknownExternal 误报白费一次 LLM 重试）
    #[test]
    fn test_extract_skips_fenced_code() {
        let content =
            "## 依赖关系\n- src::db — 持久层\n\n```rust\nlet x = 1;\n- fake_crate\n```\n- tokio\n";
        let claims = extract_dependency_claims(content);
        assert_eq!(
            claims,
            vec!["src::db".to_string(), "tokio".to_string()],
            "围栏内声称不应被提取: {claims:?}"
        );
    }

    /// 节标题精确档：「依赖注入实现」这类 DI 主题节不是依赖声称节——
    /// 旧版 contains("依赖") 误判后其下所有 `- ` 行（Spring/Dagger）变声称
    #[test]
    fn test_dependency_injection_section_not_treated_as_dependency() {
        let content = "## 依赖注入实现\n- Spring\n- Dagger\n\n## 依赖关系\n- src::db\n";
        let claims = extract_dependency_claims(content);
        assert_eq!(
            claims,
            vec!["src::db".to_string()],
            "DI 主题节不应被解析为依赖节: {claims:?}"
        );
    }

    #[test]
    fn test_validate_dependencies_allows_known_deps_and_imports() {
        let chunk = make_chunk(
            &["src::db"],
            &["serde::Serialize", "std::collections::HashMap"],
        );
        // 依赖模块、导入 crate（及其子路径）、std 前缀 → 全部通过
        let content = "## 依赖关系\n- src::db — 持久层\n- serde — 序列化\n- serde::Deserialize\n- std::io\n- core::fmt\n";
        let violations = validate_dependencies(content, &chunk);
        assert!(
            violations.is_empty(),
            "已知依赖与导入 crate 不应判违反: {violations:?}"
        );
    }

    #[test]
    fn test_validate_dependencies_catches_fabricated() {
        let chunk = make_chunk(&["src::db"], &["serde::Serialize"]);
        let content = "## 依赖关系\n- src::db — 持久层\n- src::nonexistent — 不存在\n- totally_made_up_crate — 编造\n";
        let violations = validate_dependencies(content, &chunk);
        assert_eq!(violations.len(), 2, "应捕获 2 条违反: {violations:?}");
        // 内部形态 → NotADependency；外部编造 → UnknownExternal
        assert_eq!(
            violations[0].reason,
            DependencyViolationReason::NotADependency
        );
        assert_eq!(violations[0].claimed, "src::nonexistent");
        assert_eq!(
            violations[1].reason,
            DependencyViolationReason::UnknownExternal
        );
        assert_eq!(violations[1].claimed, "totally_made_up_crate");
    }

    #[test]
    fn test_validate_dependencies_ignores_other_sections() {
        let chunk = make_chunk(&[], &[]);
        let content = "## 概述\nsrc::db 不在此节\n## 核心实体\n- tokio — 不是依赖节\n";
        let violations = validate_dependencies(content, &chunk);
        assert!(
            violations.is_empty(),
            "非依赖节的内容不参与校验: {violations:?}"
        );
    }

    #[test]
    fn test_dependency_retry_feedback_lists_reasons() {
        let chunk = make_chunk(&["src::db"], &["serde::Serialize"]);
        let violations = vec![
            DependencyViolation {
                claimed: "src::nonexistent".into(),
                reason: DependencyViolationReason::NotADependency,
            },
            DependencyViolation {
                claimed: "fake_crate".into(),
                reason: DependencyViolationReason::UnknownExternal,
            },
        ];
        let feedback = dependency_retry_feedback(&violations, &chunk);
        assert!(feedback.contains("src::nonexistent"));
        assert!(feedback.contains("不在本模块的依赖列表中"));
        assert!(feedback.contains("fake_crate"));
        assert!(feedback.contains("疑似编造"));
        assert!(feedback.contains("重新输出完整文档"));
        assert!(
            feedback.contains("本模块的真实依赖/导入清单"),
            "反馈应附允许集清单: {feedback}"
        );
    }

    /// 允许集清单：反馈按 chunk 的真实依赖/导入构建允许集并逐项列出
    /// （deps={"src::db"} + imports={"serde::Serialize" }→ 允许集含
    /// src::db、serde，并含 std/core 前缀兜底），LLM 只能在这些中选
    #[test]
    fn test_dependency_retry_feedback_lists_allowed_set() {
        let chunk = make_chunk(&["src::db"], &["serde::Serialize"]);
        let feedback = dependency_retry_feedback(&[], &chunk);
        assert!(feedback.contains("src::db"), "应列出真实依赖");
        assert!(feedback.contains("serde"), "应列出导入 crate 顶级名");
        assert!(feedback.contains("std"), "应含 std 前缀兜底条目");
        assert!(feedback.contains("core"), "应含 core 前缀兜底条目");
        assert!(
            feedback.contains("本模块的真实依赖/导入清单"),
            "应含允许集清单标题: {feedback}"
        );
    }
}
