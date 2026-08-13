//! 依赖关系防幻觉双闸之一：生成期校验
//!
//! LLM 生成 Wiki 页的「## 依赖关系」小节时可能声称不存在的模块（虚构依赖）。
//! 本模块在生成期（wiki.rs 重试循环）解析正文声称并对照允许集校验：
//! - 允许集 = chunk.dependencies（真实模块依赖，Imports ∪ Calls 推导）
//!   ∪ 每条 ImportStmt.source 的顶级 crate 名（实际导入的外部包）
//!   ∪ {"std"/"core"} 前缀（Rust 标准库）
//!   ∪ 诚实标记行「（信息不足）」（显式标注未知，不构成声称）。
//!
//! 另一个用途：lint.rs（磁盘级 lint，S3 接线）可调用纯函数
//! `extract_dependency_claims` + `validate_claims`，以自身的模块清单
//! （图/快照）构建允许集，对产物页面做同样的防幻觉检查。

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
pub fn extract_dependency_claims(content: &str) -> Vec<String> {
    let mut claims = Vec::new();
    let mut in_section = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            in_section = is_dependency_section(t);
            continue;
        }
        if in_section && let Some(item) = t.strip_prefix("- ") {
            if item.contains("信息不足") {
                continue;
            }
            let name = claim_name_from_line(item);
            if !name.is_empty() {
                claims.push(name);
            }
        }
    }
    claims
}

/// 依赖小节标题判定（容忍 LLM 措辞变体：## 依赖关系 / ## 依赖 / ## Dependencies）
fn is_dependency_section(heading: &str) -> bool {
    let lower = heading.trim_start_matches('#').trim().to_lowercase();
    lower.contains("依赖") || lower.starts_with("dependenc")
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
fn build_allowed_set(chunk: &Chunk) -> BTreeSet<String> {
    let mut allowed: BTreeSet<String> = chunk.dependencies.iter().cloned().collect();
    for import in &chunk.imports {
        // 导入形如 "serde::Serialize" / "std::collections::HashMap"：
        // 顶级段（serde/std）是真实外部依赖名，声明其任意子路径都合法
        if let Some(top) = import.source.split("::").next() {
            allowed.insert(top.to_string());
        }
    }
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

/// 纯函数校验（供 lint.rs S3 接线复用）：claims 对照 allowed 集合出违反
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
pub fn dependency_retry_feedback(violations: &[DependencyViolation]) -> String {
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
            caller_modules: vec![],
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
        let feedback = dependency_retry_feedback(&violations);
        assert!(feedback.contains("src::nonexistent"));
        assert!(feedback.contains("不在本模块的依赖列表中"));
        assert!(feedback.contains("fake_crate"));
        assert!(feedback.contains("疑似编造"));
        assert!(feedback.contains("重新输出完整文档"));
    }
}
