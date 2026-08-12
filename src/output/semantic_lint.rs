//! 语义一致性 lint（v14 D 组，t05 拍板：变更驱动 + 成本控制）
//!
//! 与 `output::lint` 的七类静态检查互补：静态检查零 LLM（孤儿/断链/
//! 过时/引用/实体覆盖/Mermaid/符号漂移），本模块用 LLM 检测**跨页矛盾**
//! 与**过期声明**——页面声称的语义与仓库现状冲突（如"模块已废弃"而
//! api.md 仍列为核心、两页对同一实体给出矛盾描述）。
//!
//! 成本控制（t05 拍板）：
//! - 变更驱动：输入 = 本次生成的受影响页（update 尾部接线），不扫全量
//! - 单次调用合并多页：所有受影响页 + api.md 实体清单拼入一次 prompt，
//!   每页设置截断上限，token 成本≈单次生成调用
//! - LLM 失败降级为跳过并告警（"失败只告警"，不改变 update 退出码）
//!
//! 结果形态与 `LintIssue` 一致（kind="semantic-conflict"），调用方
//! （main.rs update 尾部校验）与既有 lint 输出合并展示。

use anyhow::Result;


use crate::config::schema::WikiConfig;
use crate::generate::llm::{LlmProvider, Message};
use crate::model::WikiDocument;
use crate::output::lint::LintIssue;

/// 单页内容并入证据的字符上限（页面过长时截断，控制单次调用 token）
const PAGE_EVIDENCE_LIMIT: usize = 4_000;
/// 证据总量上限（极端大变更集下防止单次调用超长）
const TOTAL_EVIDENCE_LIMIT: usize = 40_000;
/// api.md 实体清单并入证据的字符上限
const API_EVIDENCE_LIMIT: usize = 6_000;

/// 对本次生成的文档做语义一致性检查，返回矛盾清单（LintIssue 形态）
///
/// `docs` = 本次生成/更新的文档（受影响页，变更驱动）；api.md 与产物
/// 实体清单作为权威证据与页面声明交叉比对。LLM 不可用（无 key/
/// provider 构建失败）时返回 Err——由调用方按"失败只告警"处理。
/// 返回空 Vec = 无跨页矛盾。
pub fn check_semantic_consistency(
    config: &WikiConfig,
    docs: &[WikiDocument],
) -> Result<Vec<LintIssue>> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    // 1. 裁判 LLM（config.llm 决定模型；无 key 时 create_provider 报错 → Err）
    let provider = crate::generate::create_provider(config)?;

    // 2. 证据组装：受影响页内容（截断）+ api.md 实体清单（权威基准）
    let mut evidence = String::new();
    for doc in docs {
        if evidence.len() >= TOTAL_EVIDENCE_LIMIT {
            break;
        }
        evidence.push_str(&format!(
            "### 页面 {}\n{}\n",
            doc.title,
            doc.content.chars().take(PAGE_EVIDENCE_LIMIT).collect::<String>()
        ));
    }
    let api_path = crate::output::api_doc_path(config.output_dir(), &config.wiki.language);
    if let Ok(api_content) = std::fs::read_to_string(&api_path) {
        evidence.push_str(&format!(
            "### api.md 权威清单\n{}\n",
            api_content.chars().take(API_EVIDENCE_LIMIT).collect::<String>()
        ));
    }
    let evidence = evidence.chars().take(TOTAL_EVIDENCE_LIMIT).collect::<String>();

    // 3. 单次 LLM 调用（合并多页，成本≈一次生成调用）
    let messages = semantic_conflict_prompt(&config.wiki.language, &evidence);
    let content = crate::get_global_runtime().block_on(provider.complete(&messages))?;

    // 4. 容错解析：剥离围栏 → {"conflicts": [...]} → LintIssue
    Ok(parse_conflicts(&content))
}

/// 语义矛盾检查 prompt：受影响页 + api 权威清单 → 矛盾 JSON 清单
fn semantic_conflict_prompt(lang: &str, evidence: &str) -> Vec<Message> {
    let system = format!(
        r#"你是代码仓库 Wiki 文档一致性裁判。检查下面文档证据中是否存在**跨页矛盾**或**与权威清单冲突**的声明：
1. 页面之间的矛盾（同一实体/概念两页描述不一致）
2. 页面声明与 api.md 权威清单冲突（如声称已废弃/不存在，但 api.md 仍列为核心实体）
3. 页面内部的过期语义（声称的功能与实体清单矛盾）

只报告明确矛盾，不做猜测。输出 JSON（语言：{lang}）：
{{"conflicts": [{{"page": "页面标题", "claim": "矛盾声明原文", "conflict": "与什么矛盾"}}]}}
没有矛盾时输出 {{"conflicts": []}}。只输出 JSON。

示例：
{{"conflicts": [{{"page": "src::net", "claim": "模块已废弃", "conflict": "api.md 仍列为核心模块"}}]}}"#
    );
    vec![
        Message::system(system),
        Message::user(evidence.to_string()),
    ]
}

/// 解析矛盾清单（围栏剥离 + conflicts 键；非 JSON/缺键 → 空清单，
/// 语义检查是增强项，解析失败不阻断主流程）
fn parse_conflicts(content: &str) -> Vec<LintIssue> {
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stripped) else {
        tracing::warn!("语义一致性检查输出非 JSON，跳过（增强项不阻断）");
        return Vec::new();
    };
    let Some(list) = value.get("conflicts").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let page = item.get("page")?.as_str()?.to_string();
            let claim = item.get("claim")?.as_str()?.to_string();
            let conflict = item.get("conflict").and_then(|c| c.as_str()).unwrap_or("").to_string();
            Some(LintIssue {
                kind: "semantic-conflict",
                path: format!("wiki/{}", page),
                message: format!("语义矛盾: 声明「{claim}」与{conflict}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 容错解析：围栏剥离 + conflicts 键 + 无矛盾空清单 + 非 JSON 空清单
    #[test]
    fn test_parse_conflicts_forms() {
        let with_fence = r#"```json
{"conflicts": [{"page": "src::net", "claim": "模块已废弃", "conflict": "api.md 仍列为核心模块"}]}
```"#;
        let issues = parse_conflicts(with_fence);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "semantic-conflict");
        assert!(issues[0].path.contains("src::net"));
        assert!(issues[0].message.contains("模块已废弃"));

        assert!(parse_conflicts(r#"{"conflicts": []}"#).is_empty(), "无矛盾应返回空");
        assert!(parse_conflicts("not json").is_empty(), "非 JSON 应降级为空");
        assert!(parse_conflicts(r#"{"other": 1}"#).is_empty(), "缺 conflicts 键应返回空");
    }

    /// 空输入短路：无受影响页时零 LLM 调用
    #[test]
    fn test_check_semantic_empty_docs_returns_empty() {
        let config = WikiConfig::default();
        let issues = check_semantic_consistency(&config, &[]).unwrap();
        assert!(issues.is_empty());
    }

    /// C-008（Phase 16.4）：semantic_conflict_prompt few-shot——含「示例：」
    /// 与可解析的 conflicts 单条示例（带具体内容）
    #[test]
    fn test_semantic_conflict_prompt_has_fewshot_example() {
        let messages = semantic_conflict_prompt("zh", "证据内容");
        let sys = messages[0].content.clone();
        assert!(sys.contains("示例："), "应含示例标记: {sys}");
        assert!(sys.contains("只输出 JSON"), "应含 JSON 输出约束: {sys}");
        // 提取 {…} 平衡片段解析 JSON，断言存在含具体 claim 的 conflicts 示例
        let chars: Vec<char> = sys.chars().collect();
        let mut examples = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                let mut depth = 0usize;
                let mut j = i;
                while j < chars.len() {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j < chars.len() {
                    let candidate: String = chars[i..=j].iter().collect();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&candidate) {
                        examples.push(v);
                    }
                    i = j + 1;
                } else {
                    break;
                }
            } else {
                i += 1;
            }
        }
        assert!(sys.contains("模块已废弃"), "示例应带具体内容: {sys}");
        assert!(
            examples.iter().any(|v| v
                .get("conflicts")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|item| item.get("claim"))
                .and_then(|claim| claim.as_str())
                .is_some()),
            "应含可解析的 conflicts 单条示例: {sys}"
        );
    }
}
