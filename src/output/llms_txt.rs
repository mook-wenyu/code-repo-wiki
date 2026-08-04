//! llms.txt 导出（v14 E 组，t07 拍板：仅 llms.txt，不做 llms-full.txt）
//!
//! 产物根 `output.dir/llms.txt`：面向 LLM 代理的站点地图（llmstxt.org
//! 社区规范），列出全部 wiki 页面（模块页 + 全局文档）与卡片路径，
//! 供 Agent 在生成上下文/搜索前快速发现文档位置。
//!
//! 确定性契约：页面按 title 排序、语言目录按名称排序——同输入两次
//! 渲染字节一致（与 _toc.md/index.md 同一确定性要求）。
//! 不参与人工修改保护（机器消费索引，确定性重生成覆盖）。

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, WikiDocument};

/// 渲染 llms.txt 内容（确定性：页面按语言目录 + title 排序）
pub fn render_llms_txt(
    repo_name: &str,
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    languages: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {repo_name} Wiki\n\n"));
    out.push_str("> repo-wiki 生成的代码仓库 Wiki 文档索引。\n\n");

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
    if !cards.is_empty() && let Some(primary) = languages.first() {
        out.push_str("## Cards\n\n");
        let mut card_names: Vec<&str> = cards.iter().map(|c| c.module_name.as_str()).collect();
        card_names.sort_unstable();
        for name in &card_names {
            out.push_str(&format!(
                "- [{name}](cards/{primary}/{}.md)\n",
                name.replace("::", "_")
            ));
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
    let languages = crate::output::wiki_languages(config);
    let content = render_llms_txt(&repo_name, documents, cards, &languages);
    crate::fs::write_file_atomic(&llms_txt_path(output_dir), &content)
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
            last_updated: String::new(),
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
            pending_manual_edits: vec![],
            features: Vec::new(),
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
}
