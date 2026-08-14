//! llms.txt / llms-full.txt 导出（v14 E 组 t07 + v19 t05）
//!
//! 产物根 `output.dir/llms.txt`：面向 LLM 代理的站点地图（llmstxt.org
//! 社区规范），列出全部 wiki 页面（模块页 + 全局文档）与卡片路径，
//! 供 Agent 在生成上下文/搜索前快速发现文档位置。
//!
//! `output.dir/llms-full.txt`（v19 t05）：Agent 实际检索时 llms.txt 的
//! 链接需要二次打开页面，llms-full.txt 把模块职责 + 实体清单直接内联，
//! 单次读取即获得完整骨架（Stripe/Vercel 出货形态；llmstxt-gen 的
//! 8K/32K 预算模式）。非官方规范（社区惯例），格式自定并在此注释声明。
//!
//! 确定性契约：页面按 title 排序、语言目录按名称排序——同输入两次
//! 渲染字节一致（与 _toc.md/index.md 同一确定性要求；test_determinism
//! 锁死全链路哈希）。**内容禁止注入时间戳/基线等易变信号**——曾尝试
//! 在头部注入生成时间戳（v28 t07 初版），破坏了确定性契约，已回退为
//! 文件系统 mtime 判定（见 warn_if_stale）。
//! 不参与人工修改保护（机器消费索引，确定性重生成覆盖）。
//!
//! 新鲜度信号（v28 t07 修正版）：写盘前对比既有产物文件 mtime——
//! 距今 >7 天时 warn 过期（阈值对齐 Synscribe 审计线；t12 核证：过期
//! llms.txt 主动伤害检索，ρ=−0.54）。mtime 是唯一信号（不读内容，
//! 保证内容确定性）；源码基线漂移由流水线既有机制覆盖（增量
//! should_skip_noop 的 HEAD 比较；全量 generate 产物天然新鲜）。

use crate::config::schema::WikiConfig;
use crate::model::{EntitySummary, KnowledgeCard, WikiDocument};

/// llms-full.txt token 预算（llmstxt-gen 的 32K 档；token 估算 = 字符数/4，
/// 避免引入 tiktoken 依赖——估算偏差只影响裁剪时机，不影响正确性）
pub const LLMS_FULL_TOKEN_BUDGET: usize = 32_000;

/// 过期阈值：7 天（对齐 Synscribe agent-readiness 审计线 fresh ≤7 天；
/// t12 核证，生态缓存 TTL 参考 24h~7d，取审计线上限）
const LLMS_STALE_DAYS: i64 = 7;

/// 产物是否过期（纯函数，可测）：mtime 距今 >7 天视为过期
///
/// 未来时间戳（时钟回拨/时区错乱）差值为负，天然不算过期——不误报。
fn stale_by_age(
    modified: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    if now.signed_duration_since(modified) > chrono::TimeDelta::days(LLMS_STALE_DAYS) {
        Some(format!(
            "生成于 {}，距今超过 {LLMS_STALE_DAYS} 天",
            modified.to_rfc3339()
        ))
    } else {
        None
    }
}

/// 检查既有产物是否过期并输出 warn（v28 t07；llms.txt 与 llms-full.txt 共用）
///
/// 过期提示依据（t12 核证，Synscribe agent-readiness 审计实测）：新鲜度
/// 是检索质量最强相关因子（Spearman ρ=−0.54，n=17；最鲜 5 条均分 83.6
/// vs 最旧 5 条 72.3，30 天可丢 50% 检索份额）——过期 llms.txt 主动
/// 伤害检索而非中性，必须显式提示而非静默重写。
///
/// 设计约束：**不读取产物内容**（内容属于确定性契约范围）；mtime 读取
/// 失败（文件被外部删除/权限）静默返回——旧产物即将被覆盖，无 mtime
/// 可判定时不制造噪音。
fn warn_if_stale(path: &std::path::Path, label: &str) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let Ok(modified) = meta.modified() else {
        return;
    };
    let modified = chrono::DateTime::<chrono::Utc>::from(modified);
    if let Some(reason) = stale_by_age(modified, chrono::Utc::now()) {
        tracing::warn!(
            "{} 已过期（{}）；过期产物会降低 Agent 检索质量（Synscribe 实测 ρ=−0.54），请重新运行 generate",
            label,
            reason
        );
    }
}

/// 渲染 llms.txt 内容（确定性：页面按语言目录 + title 排序）
pub fn render_llms_txt(
    repo_name: &str,
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    languages: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {repo_name} Wiki\n\n"));
    out.push_str("> code-repo-wiki 生成的代码仓库 Wiki 文档索引。\n");
    // v19 t01：版本自检载体——Agent 读取到该行可判断产物由哪个工具版本
    // 生成（与 doctor 版本检查同源 env!("CARGO_PKG_VERSION")）；产物与
    // 工具版本相关时提示重跑 generate
    out.push_str(&format!(
        "> 由 code-repo-wiki v{} 生成；发现索引与工具版本不匹配时，请重新运行 generate。\n\n",
        env!("CARGO_PKG_VERSION")
    ));

    // 模块页：按 (语言目录, title) 排序（确定性；wiki/{lang}/{title}.md 与
    // write_document 落盘路径一致）
    let mut pages: Vec<(&str, &str)> = Vec::new();
    for lang in languages {
        for doc in documents {
            if doc.kind != crate::model::DocumentKind::WikiPage {
                continue;
            }
            pages.push((lang, doc.title.as_str()));
        }
    }
    pages.sort_unstable();
    if !pages.is_empty() {
        out.push_str("## Modules\n\n");
        for (lang, title) in &pages {
            out.push_str(&format!(
                "- [{title}](wiki/{lang}/{}.md)\n",
                title.replace("::", "_")
            ));
        }
        out.push('\n');
    }

    // 全局文档（api/overview/architecture/index；_toc 在产物根）
    let mut globals: Vec<String> = Vec::new();
    for lang in languages {
        for name in ["api.md", "overview.md", "architecture.md", "index.md"] {
            globals.push(format!("wiki/{lang}/{name}"));
        }
    }
    globals.sort();
    out.push_str("## Global\n\n");
    for path in &globals {
        let label = path
            .trim_end_matches(".md")
            .rsplit('/')
            .next()
            .unwrap_or(path);
        out.push_str(&format!("- [{label}]({path})\n"));
    }
    out.push_str("- [目录](_toc.md)\n\n");

    // 卡片（Agent 结构化知识，主语言目录）
    if !cards.is_empty()
        && let Some(primary) = languages.first()
    {
        out.push_str("## Cards\n\n");
        // 卡片条目路径按 card_kind 分派：模块卡 cards/{primary}/{stem}.md，
        // 项目卡（Spec/TechStack）cards/{primary}/project/{spec|tech-stack}.md。
        // 相对路径与 output::card_write_path 布局一致（此函数无 output_dir，
        // 直接构造相对路径；project 叶子名与 project_card_page_path 的
        // CardKind::Spec→"spec"/CardKind::TechStack→"tech-stack" 对齐）。
        let mut card_rows: Vec<(String, String)> = cards
            .iter()
            .map(|c| {
                let rel = match c.card_kind {
                    crate::model::CardKind::Module => {
                        format!(
                            "cards/{primary}/{}.md",
                            crate::output::card_file_stem(&c.module_name)
                        )
                    }
                    crate::model::CardKind::Spec => format!("cards/{primary}/project/spec.md"),
                    crate::model::CardKind::TechStack => {
                        format!("cards/{primary}/project/tech-stack.md")
                    }
                };
                (c.module_name.clone(), rel)
            })
            .collect();
        card_rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (name, rel) in &card_rows {
            out.push_str(&format!("- [{name}]({rel})\n"));
        }
    }
    out
}

/// llms.txt 写盘路径（产物根，与 _toc.md 同级）
pub fn llms_txt_path(output_dir: &std::path::Path) -> std::path::PathBuf {
    output_dir.join("llms.txt")
}

/// 在 render_all 收尾处调用：渲染并原子写盘（辅助产物，失败仅告警——
/// 与导出快照写失败同语义，调用方负责 warn）
pub fn write_llms_txt(
    output_dir: &std::path::Path,
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    config: &WikiConfig,
) -> Result<(), anyhow::Error> {
    // 仓库名从产物目录上级派生（AGENTS.md 生成同约定：output_dir 的上级
    // 即项目根，取其目录名）
    let repo_name = output_dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let languages = [crate::output::primary_language(config)];
    let path = llms_txt_path(output_dir);
    // v28 t07：写盘前检查既有产物新鲜度——旧产物即将被覆盖，过期提示
    // 必须在覆盖前发出（等价"docs deploy 即 re-parse 触发"）
    warn_if_stale(&path, "llms.txt");
    let content = render_llms_txt(&repo_name, documents, cards, &languages);
    crate::fs::write_file_atomic(&path, &content)
}

// ==================== llms-full.txt（v19 t05） ====================

/// 模块节：llms-full.txt 的一个 `## 模块` 区块（来源 = Knowledge Card，
/// 零 LLM 调用——卡片本身就是生成管道的结构化产物）
#[derive(Clone)]
struct ModuleSection {
    /// 模块路径（如 `src::fs`），节标题
    name: String,
    /// 模块职责一句话（card.summary 首个换行前内容）
    summary: String,
    /// 实体条目（签名级，按 name 排序保证确定性）
    entities: Vec<EntityEntry>,
}

/// 实体条目：由 EntitySummary 降级而来（裁剪时丢字段）
#[derive(Clone)]
struct EntityEntry {
    name: String,
    kind: String,
    visibility: String,
    /// 源码定位 "文件路径:起始行-结束行"（溯源价值最高，裁剪最后丢）
    source: Option<String>,
    /// LLM 生成的实体说明（描述性字段，信息量次于 source）
    doc: Option<String>,
}

impl From<&EntitySummary> for EntityEntry {
    fn from(e: &EntitySummary) -> Self {
        EntityEntry {
            name: e.name.clone(),
            kind: e.kind.clone(),
            visibility: e.visibility.clone(),
            source: e.source.clone(),
            doc: e.doc.clone(),
        }
    }
}

/// 从卡片构造模块节（确定性排序：module_name 字典序）
fn build_sections(cards: &[KnowledgeCard]) -> Vec<ModuleSection> {
    let mut cards: Vec<&KnowledgeCard> = cards.iter().collect();
    cards.sort_unstable_by_key(|c| c.module_name.as_str());
    cards
        .into_iter()
        .map(|c| ModuleSection {
            name: c.module_name.clone(),
            // 职责一句话：取 summary 首个换行前内容（超长截断防单节失控）
            summary: c
                .summary
                .split('\n')
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            entities: {
                let mut list: Vec<EntityEntry> =
                    c.key_entities.iter().map(EntityEntry::from).collect();
                list.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                list
            },
        })
        .collect()
}

/// token 估算（字符数/4；偏差只影响裁剪时机）
fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// 渲染单个模块节（完整形态：职责 + 实体签名行）
fn render_section(section: &ModuleSection, minimal: bool) -> String {
    let mut out = format!("## {}\n\n{}\n\n", section.name, section.summary);
    for e in &section.entities {
        if minimal {
            // ① 签名截断：只留名字与类型（丢定位/说明，换取预算）
            out.push_str(&format!("- {} {}\n", e.name, e.kind));
        } else {
            let mut line = format!("- {} {} ({})", e.name, e.kind, e.visibility);
            if let Some(src) = &e.source {
                line.push_str(&format!(" — 定位: {src}"));
            }
            if let Some(doc) = &e.doc {
                line.push_str(&format!(" — {doc}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// 渲染 llms-full.txt（确定性 + token 预算裁剪）
///
/// 裁剪启发式按序降级（llmstxt-gen 预算模式；每档检查预算，够则停）：
/// ② 丢常量级条目 → ③ 丢无源码定位的条目（溯源价值最低） →
/// ① 实体签名截断 → ④ 整模块丢弃（模块名永远保留，列在尾部省略节）。
/// 确定性保证：所有步骤按固定顺序作用于排序后的数据，无 HashMap 迭代序。
pub fn render_llms_full_txt(
    repo_name: &str,
    cards: &[KnowledgeCard],
    primary_lang: &str,
    token_budget: usize,
) -> String {
    // llms-full.txt 是模块实体索引：项目卡（Spec/TechStack）无源码实体，
    // 不混入模块节（实体清单/职责是模块语义，项目卡在 llms.txt 站点地图
    // 已有条目）。在 build_sections 前过滤，避免占用 token 预算。
    let module_cards: Vec<KnowledgeCard> = cards
        .iter()
        .filter(|c| c.card_kind == crate::model::CardKind::Module)
        .cloned()
        .collect();
    let sections = build_sections(&module_cards);
    let mut out = format!("# {repo_name} Wiki — 完整内容索引\n\n");
    out.push_str("> 模块职责与实体清单内联版（llms.txt 的超集，非官方规范，社区惯例格式）。\n");
    out.push_str(&format!(
        "> 模块卡片目录: cards/{primary_lang}/（实体详情以卡片为准）。\n"
    ));
    out.push_str(&format!(
        "> 由 code-repo-wiki v{} 生成；发现索引与工具版本不匹配时，请重新运行 generate。\n\n",
        env!("CARGO_PKG_VERSION")
    ));

    // 逐档降级渲染（每档检查预算）
    // 档 0：完整形态
    let mut content = sections
        .iter()
        .map(|s| render_section(s, false))
        .collect::<String>();
    if estimate_tokens(&out) + estimate_tokens(&content) <= token_budget {
        return format!("{out}{content}");
    }
    // 档 ②：丢常量级条目（kind == "constant"，模块内联数据，价值最低）
    let mut filtered: Vec<ModuleSection> = Vec::new();
    for mut s in sections.clone() {
        s.entities.retain(|e| e.kind != "constant");
        filtered.push(s);
    }
    content = filtered
        .iter()
        .map(|s| render_section(s, false))
        .collect::<String>();
    if estimate_tokens(&out) + estimate_tokens(&content) <= token_budget {
        return format!("{out}{content}");
    }
    // 档 ③：丢无源码定位的条目（无法溯源的信息价值最低）
    let mut located: Vec<ModuleSection> = Vec::new();
    for mut s in filtered {
        s.entities.retain(|e| e.source.is_some());
        located.push(s);
    }
    content = located
        .iter()
        .map(|s| render_section(s, false))
        .collect::<String>();
    if estimate_tokens(&out) + estimate_tokens(&content) <= token_budget {
        return format!("{out}{content}");
    }
    // 档 ①：实体签名截断（只留 名字+类型）
    let mut minimal: Vec<ModuleSection> = Vec::new();
    for mut s in located {
        s.entities
            .retain(|e| e.source.is_some() && e.kind != "constant");
        minimal.push(s);
    }
    content = minimal
        .iter()
        .map(|s| render_section(s, true))
        .collect::<String>();
    if estimate_tokens(&out) + estimate_tokens(&content) <= token_budget {
        return format!("{out}{content}");
    }
    // 档 ④：整模块丢弃（按节渲染体量降序尝试装入，装不下的模块名
    // 列尾部省略节；确定性：kept 排序稳定，贪心顺序固定）
    let mut kept: Vec<ModuleSection> = minimal;
    kept.sort_unstable_by(|a, b| {
        render_section(b, true)
            .len()
            .cmp(&render_section(a, true).len())
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut omitted: Vec<String> = Vec::new();
    let mut final_content = String::new();
    let mut remaining = token_budget.saturating_sub(estimate_tokens(&out));
    for s in kept {
        let section = render_section(&s, true);
        if estimate_tokens(&section) <= remaining {
            final_content.push_str(&section);
            remaining -= estimate_tokens(&section);
        } else {
            omitted.push(s.name.clone());
        }
    }
    let mut final_out = out;
    if !omitted.is_empty() {
        final_out.push_str(&format!(
            "## 省略模块（预算 {token_budget} tokens 内未展开）\n"
        ));
        for name in &omitted {
            final_out.push_str(&format!("- {name}\n"));
        }
        final_out.push('\n');
    }
    final_out.push_str(&final_content);
    final_out
}

/// llms-full.txt 写盘路径（产物根，与 llms.txt 同级）
pub fn llms_full_txt_path(output_dir: &std::path::Path) -> std::path::PathBuf {
    output_dir.join("llms-full.txt")
}

/// 渲染并原子写盘（与 write_llms_txt 同语义：辅助产物，失败仅告警）
pub fn write_llms_full_txt(
    output_dir: &std::path::Path,
    cards: &[KnowledgeCard],
    config: &WikiConfig,
) -> Result<(), anyhow::Error> {
    let repo_name = output_dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let primary_lang = crate::output::primary_language(config);
    let path = llms_full_txt_path(output_dir);
    // v28 t07：与 write_llms_txt 相同的新鲜度检查（旧产物覆盖前提示）
    warn_if_stale(&path, "llms-full.txt");
    let content = render_llms_full_txt(&repo_name, cards, &primary_lang, LLMS_FULL_TOKEN_BUDGET);
    crate::fs::write_file_atomic(&path, &content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentKind;

    fn make_doc(title: &str, kind: DocumentKind) -> WikiDocument {
        WikiDocument {
            title: title.into(),
            kind,
            content: String::new(),
            language: "zh".into(),
            module_path: Vec::new(),
            references: Vec::new(),
            parent: String::new(),
            last_updated: String::new(),
            // llms.txt 契约（llms_txt.rs 顶部）：内容禁止注入易变时间戳/基线
            based_on_commit: None,
            fingerprint: None,
        }
    }

    fn make_card(name: &str) -> KnowledgeCard {
        KnowledgeCard {
            module_name: name.into(),
            module_type: "module".into(),
            summary: String::new(),
            key_entities: vec![],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            design_rationale: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
            card_kind: crate::model::CardKind::Module,
            spec_categories: vec![],
        }
    }

    /// 确定性 + 内容结构：模块页/全局文档/卡片三类链接齐全，排序稳定
    #[test]
    fn test_render_llms_txt_deterministic_and_complete() {
        let docs = vec![
            make_doc("src::zebra", DocumentKind::WikiPage),
            make_doc("src::alpha", DocumentKind::WikiPage),
            make_doc("API Reference", DocumentKind::ApiReference),
        ];
        let cards = vec![make_card("src::alpha"), make_card("src::beta")];
        let langs = vec!["zh".to_string(), "en".to_string()];

        let first = render_llms_txt("demo", &docs, &cards, &langs);
        let second = render_llms_txt("demo", &docs, &cards, &langs);
        assert_eq!(first, second, "同输入两次渲染必须字节一致");

        assert!(first.contains("# demo Wiki"), "应含仓库名标题");
        assert!(
            first.contains(&format!("code-repo-wiki v{}", env!("CARGO_PKG_VERSION"))),
            "应含工具版本行（版本自检载体）"
        );
        assert!(first.contains("## Modules"), "应含模块页节");
        // 模块页：两个语言目录 × 两个模块页
        assert!(first.contains("wiki/zh/src_alpha.md"), "zh 模块页链接");
        assert!(first.contains("wiki/zh/src_zebra.md"), "zh 模块页排序稳定");
        assert!(first.contains("wiki/en/src_alpha.md"), "en 模块页链接");
        // 模块页排序：alpha 在 zebra 前（字典序）
        let alpha_pos = first.find("src_alpha.md").unwrap();
        let zebra_pos = first.find("src_zebra.md").unwrap();
        assert!(alpha_pos < zebra_pos, "模块页应按 title 字典序: {first}");
        // 全局文档与目录
        assert!(first.contains("## Global"), "应含全局文档节");
        assert!(first.contains("wiki/zh/api.md"), "api 链接");
        assert!(first.contains("wiki/en/overview.md"), "扩展语言全局链接");
        assert!(first.contains("(_toc.md)"), "目录链接");
        // 卡片节
        assert!(first.contains("## Cards"), "应含卡片节");
        assert!(first.contains("cards/zh/src_alpha.md"), "卡片链接");
        assert!(first.contains("cards/zh/src_beta.md"), "卡片链接排序稳定");
    }

    /// 空文档：仅头部与空节标题（不崩溃）
    #[test]
    fn test_render_llms_txt_empty_docs() {
        let out = render_llms_txt("demo", &[], &[], &["zh".to_string()]);
        assert!(out.contains("# demo Wiki"));
        assert!(!out.contains("## Modules"), "无模块页不应出现 Modules 节");
    }

    // ==================== llms-full.txt（v19 t05） ====================

    /// 构造带实体的卡片（kind/source/doc 可控，供裁剪测试）
    fn make_card_entities(
        name: &str,
        entities: Vec<(&str, Option<&str>, Option<&str>)>,
    ) -> KnowledgeCard {
        let mut card = make_card(name);
        card.summary = format!("{name} 模块职责一句话");
        card.key_entities = entities
            .into_iter()
            .map(|(n, src, doc)| crate::model::EntitySummary {
                name: n.into(),
                kind: "function".into(),
                visibility: "pub".into(),
                doc: doc.map(String::from),
                source: src.map(String::from),
            })
            .collect();
        card
    }

    /// 确定性 + 结构：模块节/职责/实体签名行齐全，两次渲染字节一致
    #[test]
    fn test_render_llms_full_txt_deterministic_and_complete() {
        let cards = vec![
            make_card_entities(
                "src::beta",
                vec![
                    ("zulu", Some("src/beta.rs:1-5"), Some("说明")),
                    ("alpha", Some("src/beta.rs:10-12"), None),
                ],
            ),
            make_card_entities(
                "src::alpha",
                vec![("server", Some("src/alpha.rs:1-3"), None)],
            ),
        ];

        let first = render_llms_full_txt("demo", &cards, "zh", 32_000);
        let second = render_llms_full_txt("demo", &cards, "zh", 32_000);
        assert_eq!(first, second, "同输入两次渲染必须字节一致");

        assert!(first.contains("# demo Wiki"), "应含仓库名标题");
        assert!(
            first.contains(&format!("code-repo-wiki v{}", env!("CARGO_PKG_VERSION"))),
            "应含工具版本行"
        );
        assert!(first.contains("## src::alpha"), "模块节标题");
        assert!(
            first.contains("src::alpha 模块职责一句话"),
            "模块职责一句话"
        );
        // 实体签名行：名字+类型+可见性+定位
        assert!(
            first.contains("- server function (pub) — 定位: src/alpha.rs:1-3"),
            "完整签名行"
        );
        // 确定性排序：alpha 节在 beta 节前（module_name 字典序）
        let a = first.find("## src::alpha").unwrap();
        let b = first.find("## src::beta").unwrap();
        assert!(a < b, "模块节应按 module_name 字典序: {first}");
    }

    /// 预算裁剪：完整形态超预算时逐档降级——
    /// ② 丢常量级 → ③ 丢无 source → ① 签名截断 → ④ 整模块省略（模块名保留）
    #[test]
    fn test_render_llms_full_txt_budget_trims() {
        // 大量实体撑爆完整形态；含常量级（kind=constant，② 档目标）、
        // 无 source 实体（③ 档目标）
        let mut card = make_card("src::alpha");
        card.summary = "alpha 模块".into();
        let mut entities = Vec::new();
        for i in 0..2000 {
            entities.push(crate::model::EntitySummary {
                name: format!("fn{i}"),
                kind: "function".into(),
                visibility: "pub".into(),
                doc: Some("说明".into()),
                source: Some(format!("src/alpha.rs:{}-{}", i + 1, i + 2)),
            });
        }
        entities.push(crate::model::EntitySummary {
            name: "const_x".into(),
            kind: "constant".into(),
            visibility: "pub".into(),
            doc: None,
            source: Some("src/alpha.rs:999".into()),
        });
        entities.push(crate::model::EntitySummary {
            name: "ghost".into(),
            kind: "function".into(),
            visibility: "pub".into(),
            doc: None,
            source: None,
        });
        card.key_entities = entities;
        let cards = vec![card];

        // 预算极小（仅能容纳头部）：走完 ②③① 后进入 ④，模块名保留。
        // 头部固定开销含 t07 信号行约 210 字符（≈52 tokens），预算须高于
        // 头部 + 省略节才能验证省略档（模块精简形态 ≈6000 tokens 装不下）
        let tiny = render_llms_full_txt("demo", &cards, "zh", 80);
        assert!(
            tiny.contains("## 省略模块") && tiny.contains("src::alpha"),
            "整模块省略时模块名必须保留: {tiny}"
        );
        assert!(
            estimate_tokens(&tiny) <= 80 + 1,
            "输出应落在预算内: {} tokens",
            estimate_tokens(&tiny)
        );

        // 中等预算（容得下 ① 精简形态但容不下完整形态）：
        // 完整形态 ≈2000×55 字符 ≈27.5K tokens，精简形态 ≈8.5K tokens；
        // 断言输出是精简行（无 定位: 前缀）、且不含常量级条目
        let mid = render_llms_full_txt("demo", &cards, "zh", 20_000);
        assert!(
            estimate_tokens(&mid) <= 20_000 + 1,
            "输出应落在预算内: {} tokens",
            estimate_tokens(&mid)
        );
        assert!(!mid.contains("定位:"), "③ 档后不应再有完整签名行");
        assert!(!mid.contains("const_x"), "② 档应已丢常量级条目");
    }

    /// 空输入：无卡片时仅头部 + 版本行（不崩溃、无节标题）
    #[test]
    fn test_render_llms_full_txt_empty_cards() {
        let out = render_llms_full_txt("demo", &[], "zh", 32_000);
        assert!(out.contains("# demo Wiki"));
        assert!(!out.contains("## "), "无卡片不应出现模块节");
    }

    // ==================== v28 t07 新鲜度判定（mtime 版） ====================

    /// 产物 mtime 距今 > 7 天 → 过期原因（阈值对齐 Synscribe 审计线）
    #[test]
    fn test_stale_by_age_older_than_seven_days() {
        let now = chrono::Utc::now();
        let old = now - chrono::TimeDelta::days(8);
        let reason = stale_by_age(old, now).unwrap();
        assert!(reason.contains("超过 7 天"), "应报时间过期: {reason}");
    }

    /// 产物 mtime 距今 1 天 → 新鲜（None）
    #[test]
    fn test_stale_by_age_recent_is_fresh() {
        let now = chrono::Utc::now();
        assert!(stale_by_age(now - chrono::TimeDelta::days(1), now).is_none());
    }

    /// 未来时间戳（时钟回拨）→ 新鲜（差值为负，不误报）
    #[test]
    fn test_stale_by_age_future_clock_is_fresh() {
        let now = chrono::Utc::now();
        assert!(
            stale_by_age(now + chrono::TimeDelta::days(1), now).is_none(),
            "未来时间戳不应误报过期"
        );
    }

    /// 写盘冒烟：temp 目录产物正常落盘（内容确定性契约——无时间戳注入）
    #[test]
    fn test_write_llms_txt_smoke_deterministic_content() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_llms_mtime_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = WikiConfig {
            output_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };
        write_llms_txt(&dir, &[], &[], &config).unwrap();
        let content = std::fs::read_to_string(llms_txt_path(&dir)).unwrap();
        assert!(
            !content.contains("> 生成时间:"),
            "内容禁止注入易变时间戳（确定性契约）: {content}"
        );
        assert!(
            content.starts_with("# ") && content.contains("Wiki"),
            "应含仓库名标题: {content}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
