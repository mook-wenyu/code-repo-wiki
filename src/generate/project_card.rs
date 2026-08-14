//! 项目级知识卡片生成（Spec 代码规约卡 + TechStack 技术栈卡）
//!
//! 与模块卡（CardKind::Module）的区别：项目卡全项目各一张，回答
//! 「项目被什么约束（规约）/用什么技术（清单）」；模块卡回答
//! 「单个模块内部设计」。项目卡不依赖 chunk/模块划分。
//!
//! - Spec 卡（CardKind::Spec）：LLM 从规约文件材料（AGENTS.md 等）提炼
//!   结构化规约 JSON，输入驱动——无规约材料与 notes 时不生成（防幻觉）。
//! - TechStack 卡（CardKind::TechStack）：确定性解析依赖清单（零 LLM），
//!   来自 [`crate::analysis::techstack::parse_tech_stack`]。

use std::path::Path;

use anyhow::Result;

use crate::config::plan::PlanNote;
use crate::config::schema::WikiConfig;
use crate::generate::llm::{LlmProvider, Message};
use crate::generate::prompt;
use crate::model::{CardKind, KnowledgeCard, SpecCategory, SpecItem};

/// 规约文件清单（相对项目根；存在才纳入，.rustfmt.toml 与 rustfmt.toml 二选一）
pub const SPEC_FILES: &[&str] = &[
    "AGENTS.md",
    ".editorconfig",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    "CONTRIBUTING.md",
    "docs/glossary.md",
];

/// 规约文件内容截断上限（32KB，防长文件撑爆 prompt）
const SPEC_FILE_MAX_BYTES: u64 = 32 * 1024;

/// Spec 卡输出侧 token 预算（与 card.rs CARD_MAX_OUTPUT_TOKENS 同量级，
/// 防推理型模型 reasoning 吞预算导致 JSON 截断）
const SPEC_CARD_MAX_OUTPUT_TOKENS: u32 = 8192;

/// 项目卡生成统一入口：Spec 卡（LLM 提炼）+ TechStack 卡（确定性解析）。
/// 返回 0~2 张卡（无输入/无清单时对应卡不生成）。Spec 卡生成失败返回 Err
/// （由流水线按失败隔离语义处理——显式告警，不静默）。
pub async fn generate_project_cards<P: LlmProvider>(
    provider: &P,
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    card_notes: &[PlanNote],
) -> Result<Vec<KnowledgeCard>> {
    let mut cards = Vec::new();
    if let Some(spec) = generate_project_spec_card(provider, config, root, card_notes).await? {
        cards.push(spec);
    }
    if let Some(stack) = generate_project_tech_stack_card(root)? {
        cards.push(stack);
    }
    Ok(cards)
}

/// Spec 卡：输入驱动，无输入不生成（防幻觉）——规约文件与 notes 皆无时
/// 返回 Ok(None)。LLM 输出 JSON 解析失败重试一次（复用 extract_json 模式）。
pub async fn generate_project_spec_card<P: LlmProvider>(
    provider: &P,
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    card_notes: &[PlanNote],
) -> Result<Option<KnowledgeCard>> {
    // 输入驱动防幻觉：规约文件与人工引导注记皆无时，LLM 无材料可提炼，
    // 直接不生成（返回 Ok(None)）——不向空材料请求 LLM，杜绝编造。
    let spec_files = collect_spec_files(root);
    if spec_files.is_empty() && card_notes.is_empty() {
        return Ok(None);
    }

    let mut messages = vec![
        Message::system(prompt::spec_card_system_prompt(&config.wiki.language)),
        Message::user(prompt::spec_card_user_prompt(&spec_files, card_notes)),
    ];
    let mut response = provider
        .complete_with_budget(&messages, Some(SPEC_CARD_MAX_OUTPUT_TOKENS))
        .await?;
    // LLM 输出可能带 Markdown 围栏或尾随散文：解析失败时追加约束消息重试一次
    let parsed = match parse_spec_json(&response) {
        Ok(v) => v,
        Err(first_err) => {
            messages.push(Message::user(
                "你上一次的输出无法解析为合法 JSON。请只输出 JSON 对象本体：\
                 不要任何 ``` 围栏、不要解释、不要尾随内容。"
                    .to_string(),
            ));
            response = provider
                .complete_with_budget(&messages, Some(SPEC_CARD_MAX_OUTPUT_TOKENS))
                .await?;
            parse_spec_json(&response).map_err(|second_err| {
                anyhow::anyhow!(
                    "Spec 卡 JSON 重试仍解析失败（首次: {}；重试: {}）",
                    first_err,
                    second_err
                )
            })?
        }
    };

    // 字段映射：category/rule 缺省空串（source 缺省空串）；related_files
    // 确定性回填实际纳入的规约文件（不经过 LLM，防幻觉）
    let summary = parsed["summary"].as_str().unwrap_or("").to_string();
    let spec_categories: Vec<SpecCategory> = parsed["categories"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|v| SpecCategory {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    items: v["items"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|it| SpecItem {
                                    rule: it["rule"].as_str().unwrap_or("").to_string(),
                                    source: it["source"].as_str().unwrap_or("").to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let related_files: Vec<String> = spec_files.iter().map(|(p, _)| p.clone()).collect();

    Ok(Some(KnowledgeCard {
        module_name: "project::spec".to_string(),
        module_type: "project".to_string(),
        summary,
        // Spec 卡不承载模块级维度：结构化规约分类才是内容主体，其余留空
        key_entities: vec![],
        dependencies: vec![],
        dependents: vec![],
        design_patterns: vec![],
        todo_notes: vec![],
        related_files,
        coding_spec: None,
        tech_stack: vec![],
        architecture: None,
        design_rationale: None,
        pending_manual_edits: vec![],
        features: vec![],
        card_kind: CardKind::Spec,
        spec_categories,
    }))
}

/// TechStack 卡：确定性解析依赖清单（零 LLM，防幻觉）；无清单时 Ok(None)。
pub fn generate_project_tech_stack_card(
    root: &crate::project::ProjectRoot,
) -> Result<Option<KnowledgeCard>> {
    let entries = crate::analysis::techstack::parse_tech_stack(root.path());
    if entries.is_empty() {
        return Ok(None);
    }
    // 清单文件数 = 命中去重后的 manifest 名数量（用于 summary「N 项 / M 类」）
    let mut manifests: Vec<String> = entries.iter().map(|e| e.manifest.clone()).collect();
    manifests.sort();
    manifests.dedup();
    // 确定性渲染行：version 空串时省略 @version（清单无版本字段）
    let tech_stack: Vec<String> = entries
        .iter()
        .map(|e| {
            if e.version.is_empty() {
                format!("{}（{}，{}）", e.name, e.category, e.manifest)
            } else {
                format!("{}@{}（{}，{}）", e.name, e.version, e.category, e.manifest)
            }
        })
        .collect();

    Ok(Some(KnowledgeCard {
        module_name: "project::tech-stack".to_string(),
        module_type: "project".to_string(),
        summary: format!(
            "共 {} 项依赖，分布于 {} 类清单",
            entries.len(),
            manifests.len()
        ),
        key_entities: vec![],
        dependencies: vec![],
        dependents: vec![],
        design_patterns: vec![],
        todo_notes: vec![],
        // 关联文件 = 实际命中的清单文件名（去重排序，确定性）
        related_files: manifests,
        coding_spec: None,
        tech_stack,
        architecture: None,
        design_rationale: None,
        pending_manual_edits: vec![],
        features: vec![],
        card_kind: CardKind::TechStack,
        spec_categories: vec![],
    }))
}

/// 扫描规约文件：root 相对路径逐个尝试读取；不存在跳过、读取失败显式告警
/// （不中断）。内容超 SPEC_FILE_MAX_BYTES 截断（截断处追加截断标记）。
/// rustfmt.toml 与 .rustfmt.toml 视为同一「rustfmt」槽位：SPEC_FILES 顺序
/// 扫描时后命中者（.rustfmt.toml）覆盖前驻（rustfmt.toml），惯例优先，
/// 二者不会同时出现（去重）。
fn collect_spec_files(root: &crate::project::ProjectRoot) -> Vec<(String, String)> {
    // 槽位键：非 rustfmt 文件 = 自身路径；两个 rustfmt 形变共享 "rustfmt"
    const RUSTFMT_SLOT: &str = "rustfmt";
    let slot_of = |path: &str| -> String {
        if path == "rustfmt.toml" || path == ".rustfmt.toml" {
            RUSTFMT_SLOT.to_string()
        } else {
            path.to_string()
        }
    };
    let mut slots: Vec<(String, String, String)> = Vec::new();
    for &candidate in SPEC_FILES {
        let joined = root.join(Path::new(candidate));
        // 文件不存在（含目录）→ 跳过该候选
        if !joined.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&joined) {
            Ok(c) => c,
            Err(e) => {
                // 读取失败：显式告警并跳过（不中断其余文件）
                tracing::warn!("读取规约文件失败，跳过 {}: {}", candidate, e);
                continue;
            }
        };
        // 空内容文件不纳入（该文件不出现）
        let content = if content.is_empty() {
            continue;
        } else if content.len() as u64 > SPEC_FILE_MAX_BYTES {
            truncate_bytes_safe(&content, SPEC_FILE_MAX_BYTES)
        } else {
            content
        };
        // 后命中者覆盖同槽位（去重：同一逻辑文件不重复出现）
        let key = slot_of(candidate);
        match slots.iter().position(|(k, _, _)| *k == key) {
            Some(i) => slots[i] = (key, candidate.to_string(), content),
            None => slots.push((key, candidate.to_string(), content)),
        }
    }
    slots
        .into_iter()
        .map(|(_, path, content)| (path, content))
        .collect()
}

/// 按字节上限做 UTF-8 安全截断（不劈开多字节字符），追加截断标记。
/// 返回字符串可能略超字节上限（追加的标记不计入源内容上限）。
fn truncate_bytes_safe(s: &str, max_bytes: u64) -> String {
    // 不变量：s.len() > max_bytes（调用方已判超限），char_indices 首个
    // 超限位置必存在——找不到即 s 未超限，但按调用契约不会走到（结构安全）。
    let mut byte_end = 0usize;
    for (idx, ch) in s.char_indices() {
        let next_end = idx + ch.len_utf8();
        if next_end as u64 > max_bytes {
            break;
        }
        byte_end = next_end;
    }
    let mut out = s[..byte_end].to_string();
    out.push_str("\n…（内容超上限已截断）");
    out
}

/// 从 LLM 响应提取 JSON 并解析（复用 card.rs extract_json：取首{尾}切片）
fn parse_spec_json(response: &str) -> Result<serde_json::Value> {
    let json_str = crate::generate::card::extract_json(response);
    serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("解析 Spec 卡 JSON 失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 建唯一临时目录（项目根 fixture），返回路径（约定见仓库现有测试）
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_project_card_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写文件并自动创建父目录（docs/glossary.md 等嵌套路径）
    fn write_file(dir: &std::path::Path, name: &str, content: &str) {
        if let Some(parent) = std::path::Path::new(name).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir.join(parent)).unwrap();
        }
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// 返回始终合法 Spec 卡 JSON 的 provider（mock 集成用；自定义 provider
    /// ——MockProvider 不认 Spec 卡的「只输出 JSON 本体」措辞，会回 Markdown）
    struct SpecJsonProvider;
    impl crate::generate::llm::LlmProvider for SpecJsonProvider {
        async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
            Ok(
                r#"{"summary":"仓库代码规约","categories":[{"name":"提交纪律","items":[{"rule":"一个逻辑变更一个提交","source":"AGENTS.md"},{"rule":"中文提交"}]}]}"#
                    .to_string(),
            )
        }
        fn call_count(&self) -> usize {
            0
        }
    }

    /// 首次坏 JSON（围栏+尾随），重试合法——验证重试一次成功且调用 2 次
    struct FlakySpecProvider {
        calls: AtomicUsize,
    }
    impl crate::generate::llm::LlmProvider for FlakySpecProvider {
        async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                Ok("```json\n{\"summary\":\"首次\"}\n```\n尾随 { \"extra\": 1 }".to_string())
            } else {
                Ok(r#"{"summary":"重试成功","categories":[]}"#.to_string())
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn test_generate_spec_card_no_input_yields_none() {
        let dir = temp_dir("spec_none");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let provider = SpecJsonProvider;
        let config = WikiConfig::default();
        let card = generate_project_spec_card(&provider, &config, &root, &[])
            .await
            .unwrap();
        assert!(
            card.is_none(),
            "无规约文件且无 notes 时不应生成 Spec 卡（防幻觉）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_generate_spec_card_with_agents_md() {
        let dir = temp_dir("spec_agents");
        write_file(&dir, "AGENTS.md", "禁止 git add -A\n一个逻辑变更一个提交\n");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let provider = SpecJsonProvider;
        let config = WikiConfig::default();
        let card = generate_project_spec_card(&provider, &config, &root, &[])
            .await
            .unwrap()
            .expect("有 AGENTS.md 应生成 Spec 卡");
        assert_eq!(card.module_name, "project::spec");
        assert_eq!(card.module_type, "project");
        assert_eq!(card.card_kind, CardKind::Spec);
        assert_eq!(card.summary, "仓库代码规约");
        // related_files 确定性回填实际纳入的规约文件（含 AGENTS.md，不经过 LLM）
        assert!(card.related_files.contains(&"AGENTS.md".to_string()));
        // JSON categories 结构化映射完整（source 缺省空串）
        assert_eq!(card.spec_categories.len(), 1);
        assert_eq!(card.spec_categories[0].name, "提交纪律");
        assert_eq!(
            card.spec_categories[0].items[0].rule,
            "一个逻辑变更一个提交"
        );
        assert_eq!(card.spec_categories[0].items[0].source, "AGENTS.md");
        assert_eq!(
            card.spec_categories[0].items[1].source, "",
            "source 缺省空串"
        );
        // 模块级维度全空
        assert!(card.key_entities.is_empty());
        assert!(card.tech_stack.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_generate_spec_card_retries_on_bad_json() {
        let dir = temp_dir("spec_retry");
        write_file(&dir, "AGENTS.md", "规约内容\n");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let provider = FlakySpecProvider {
            calls: AtomicUsize::new(0),
        };
        let config = WikiConfig::default();
        let card = generate_project_spec_card(&provider, &config, &root, &[])
            .await
            .unwrap()
            .expect("重试后应生成 Spec 卡");
        assert_eq!(card.summary, "重试成功");
        assert_eq!(provider.call_count(), 2, "首次坏 JSON + 重试 = 2 次调用");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TechStack：确定性（零 LLM）解析 Cargo.toml + package.json
    #[test]
    fn test_generate_tech_stack_card_parses_manifests() {
        let dir = temp_dir("stack_both");
        write_file(
            &dir,
            "Cargo.toml",
            "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
        );
        write_file(
            &dir,
            "package.json",
            "{ \"dependencies\": { \"react\": \"^18.0.0\" } }",
        );
        let root = crate::project::ProjectRoot::new(dir.clone());
        let card = generate_project_tech_stack_card(&root)
            .unwrap()
            .expect("有清单应生成 TechStack 卡");
        assert_eq!(card.module_name, "project::tech-stack");
        assert_eq!(card.card_kind, CardKind::TechStack);
        // summary 含计数「N 项 / M 类」
        assert!(
            card.summary.starts_with("共 "),
            "summary 计数: {}",
            card.summary
        );
        assert!(card.summary.contains("项依赖"));
        assert!(card.summary.contains("类清单"));
        // related_files 含两个清单名（去重排序）
        assert!(card.related_files.contains(&"Cargo.toml".to_string()));
        assert!(card.related_files.contains(&"package.json".to_string()));
        // tech_stack 渲染行（有版本 @version + 全角括号；category/manifest 溯源）
        assert!(
            card.tech_stack
                .iter()
                .any(|l| l.contains("serde@1.0") && l.contains("rust/cargo")),
            "serde 行: {:?}",
            card.tech_stack
        );
        assert!(
            card.tech_stack
                .iter()
                .any(|l| l.contains("react@^18.0.0") && l.contains("javascript/npm")),
            "react 行: {:?}",
            card.tech_stack
        );
        assert!(
            card.spec_categories.is_empty(),
            "TechStack 卡 spec_categories 应为空"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无清单 → TechStack 卡 Ok(None)（确定性空输入）
    #[test]
    fn test_generate_tech_stack_card_no_manifests_yields_none() {
        let dir = temp_dir("stack_none");
        let root = crate::project::ProjectRoot::new(dir.clone());
        let card = generate_project_tech_stack_card(&root).unwrap();
        assert!(card.is_none(), "无清单不应生成 TechStack 卡");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 统一入口：Spec 与 TechStack 都有 → 2 张卡
    #[tokio::test]
    async fn test_generate_project_cards_both() {
        let dir = temp_dir("entry_both");
        write_file(&dir, "AGENTS.md", "规约\n");
        write_file(
            &dir,
            "Cargo.toml",
            "[package]\nname=\"x\"\n\n[dependencies]\nserde=\"1\"\n",
        );
        let root = crate::project::ProjectRoot::new(dir.clone());
        let provider = SpecJsonProvider;
        let config = WikiConfig::default();
        let cards = generate_project_cards(&provider, &config, &root, &[])
            .await
            .unwrap();
        assert_eq!(cards.len(), 2, "应有 Spec + TechStack 两张卡");
        assert!(cards.iter().any(|c| c.card_kind == CardKind::Spec));
        assert!(cards.iter().any(|c| c.card_kind == CardKind::TechStack));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 统一入口：仅 TechStack（无规约文件/notes）→ 1 张卡（Spec 不生成）
    #[tokio::test]
    async fn test_generate_project_cards_only_techstack() {
        let dir = temp_dir("entry_stack");
        write_file(
            &dir,
            "package.json",
            "{ \"dependencies\": { \"lodash\": \"^4.0\" } }",
        );
        let root = crate::project::ProjectRoot::new(dir.clone());
        let provider = SpecJsonProvider;
        let config = WikiConfig::default();
        let cards = generate_project_cards(&provider, &config, &root, &[])
            .await
            .unwrap();
        assert_eq!(cards.len(), 1, "仅 TechStack 一张卡（Spec 无输入不生成）");
        assert_eq!(cards[0].card_kind, CardKind::TechStack);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 截断边界：内容超 SPEC_FILE_MAX_BYTES 时截断并追加标记；默认截断
    /// 在 UTF-8 字符边界（不劈开多字节字符）。
    #[test]
    fn test_truncate_bytes_safe_keeps_utf8_boundary() {
        let big = "a".repeat((SPEC_FILE_MAX_BYTES as usize) + 100);
        let out = truncate_bytes_safe(&big, SPEC_FILE_MAX_BYTES);
        assert!(out.ends_with("…（内容超上限已截断）"));
        assert!(out.starts_with(&"a".repeat(SPEC_FILE_MAX_BYTES as usize)));
        // 多字节内容：ASCII 堆满 cap 前预留一个 3 字节「中」，截断不得劈开它
        let mixed = format!(
            "{}{}",
            "a".repeat((SPEC_FILE_MAX_BYTES as usize) - 3),
            "中".repeat(10)
        );
        const MARKER: &str = "\n…（内容超上限已截断）";
        let out = truncate_bytes_safe(&mixed, SPEC_FILE_MAX_BYTES);
        let body = out.strip_suffix(MARKER).expect("应含截断标记");
        assert!(
            body.is_char_boundary(body.len()) && body.ends_with('中'),
            "截断不得劈开多字节字符: {body}"
        );
    }
}
