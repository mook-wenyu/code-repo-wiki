pub mod crossref;
pub mod markdown;
pub mod mermaid;
pub mod html;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::markdown::write_document;

/// 生效的 wiki 语言列表（主语言 + 扩展语言），与生成层保持一致
pub fn wiki_languages(config: &WikiConfig) -> Vec<String> {
    crate::generate::collect_languages(config)
}

/// API 参考页写盘路径：`{output_dir}/wiki/{lang}/api.md`（每种语言独立一份）
///
/// render_all 写盘与状态层指纹记录共用本函数产出路径，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致（同一规则，防止两处漂移）。
pub fn api_doc_path(output_dir: &Path, lang: &str) -> PathBuf {
    output_dir.join("wiki").join(lang).join("api.md")
}

/// 概览页写盘路径：`{output_dir}/wiki/{lang}/overview.md`（仅主语言一份）
pub fn overview_doc_path(output_dir: &Path, lang: &str) -> PathBuf {
    output_dir.join("wiki").join(lang).join("overview.md")
}

/// 目录页写盘路径：`{output_dir}/_toc.md`（输出目录根一份）
pub fn toc_doc_path(output_dir: &Path) -> PathBuf {
    output_dir.join("_toc.md")
}

/// 卡片文件名主体（不含 .md 后缀）：`module.replace("::", "_")`
///
/// 卡片写盘、卡片索引与删除清理共用本函数，保证卡片命名单一来源。
pub(crate) fn card_file_stem(module: &str) -> String {
    module.replace("::", "_")
}

/// 卡片写盘路径：`{output_dir}/cards/{lang}/{module.replace("::","_")}.md`
///
/// render_all 写盘、卡片指纹记录与删除清理共用本函数产出路径，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致（防止两处漂移）。
pub(crate) fn card_page_path(output_dir: &Path, lang: &str, module: &str) -> PathBuf {
    output_dir
        .join("cards")
        .join(lang)
        .join(format!("{}.md", card_file_stem(module)))
}

/// Wiki 页面写盘路径：`{output_dir}/wiki/{lang}/{file}.md`
///
/// 文件名复用 markdown::wiki_file_name（ArchitectureOverview 特判写 architecture.md）。
/// render_all 写盘、write_document 落盘与状态层指纹记录共用本函数，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致。
pub(crate) fn wiki_page_path(output_dir: &Path, lang: &str, doc: &WikiDocument) -> PathBuf {
    if doc.kind == crate::model::DocumentKind::ArchitectureOverview {
        output_dir.join("wiki").join(lang).join("architecture.md")
    } else if doc.kind == crate::model::DocumentKind::ProjectOverview {
        output_dir.join("wiki").join(lang).join("overview.md")
    } else {
        output_dir.join("wiki").join(lang).join(markdown::wiki_file_name(doc))
    }
}

/// 由模块名派生的 wiki 页文件名（与 wiki_file_name 的 module_path.join("_") 等价）
///
/// 用于拿不到 WikiDocument 的场景（如删除清理时被删文件已不在图中），
/// 只依赖模块名本身，保证与 render_all 的落盘命名规则一致。
pub(crate) fn module_page_file_name(module: &str) -> String {
    format!("{}.md", card_file_stem(module))
}

/// 渲染所有文档到输出目录
///
/// 1. 创建输出目录结构（主语言 + 扩展语言）
/// 2. 按文档自身语言渲染并写入 Wiki 页面（多语言独立生成，不再按语言循环复制；
///    项目概览与架构概览由生成层产出，经 wiki_page_path 特判写 overview.md / architecture.md）
/// 3. 渲染并写入 Knowledge Card
/// 4. 生成 API 参考页（只写主语言）与目录页
/// 5. 生成 Mermaid 关系图
///
/// `protected` 为人工修改保护集（路径字符串），命中路径跳过写盘，
/// 覆盖 Wiki 页面与三个全局文档（api.md / overview.md / _toc.md）。
pub fn render_all(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    protected: &std::collections::HashSet<String>,
) -> Result<()> {
    let output_dir = Path::new(&config.output.dir);
    let assets_dir = output_dir.join("assets");
    let languages = wiki_languages(config);

    // 按语言创建目录（扩展语言无文档时也保留空目录，保持目录结构稳定）
    for lang in &languages {
        std::fs::create_dir_all(output_dir.join("wiki").join(lang))?;
        std::fs::create_dir_all(output_dir.join("cards").join(lang))?;
    }
    std::fs::create_dir_all(&assets_dir)?;

    // 1. 写入 Wiki 页面（按文档自身语言分组写入对应目录）
    for doc in documents {
        // 路径计算与 write_document 落盘共用 wiki_page_path（人工修改保护判定依据）
        let wiki_path = wiki_page_path(output_dir, &doc.language, doc);
        let doc_cards: Vec<&KnowledgeCard> = cards
            .iter()
            .filter(|c| doc.module_path.iter().any(|p| c.module_name.contains(p)))
            // 卡片与 wiki 页同规则保护：命中保护集的卡片跳过写盘，
            // 保留人工编辑版本（人工编辑过的卡片由指纹检测纳入保护集）
            .filter(|c| {
                !protected.contains(
                    &card_page_path(output_dir, &doc.language, &c.module_name)
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .collect();
        if protected.contains(&wiki_path.to_string_lossy().to_string()) {
            // 页面受人工修改保护：跳过页面写盘（保留人工版），但关联卡片
            // 仍写盘——人工修改记录（pending_manual_edits）随本次生成注入
            // 卡片，若一并跳过则反向同步永远无法落盘
            for card in &doc_cards {
                let card_path = card_page_path(output_dir, &doc.language, &card.module_name);
                std::fs::write(&card_path, markdown::render_knowledge_card(card))?;
            }
            continue;
        }
        write_document(doc, &doc_cards, output_dir, &doc.language)?;
    }

    // 1.5 写入 API 参考页（按模块分组的实体清单；内容与语言无关，只写主语言一份；
    // 命中保护集跳过写盘。指纹记录按同一规则：state.rs 对未落盘的 en/api.md 不记指纹）
    let primary_lang = &config.wiki.language;
    for lang in &languages {
        if lang != primary_lang {
            continue;
        }
        let api_path = api_doc_path(output_dir, lang);
        if protected.contains(&api_path.to_string_lossy().to_string()) {
            continue;
        }
        let api_doc = markdown::render_api_reference(graph);
        std::fs::write(api_path, api_doc.content)?;
    }

    // 3. 写入 Knowledge Card 索引（JSON 格式，写入主语言目录）
    let primary_lang = &languages[0];
    let cards_index_json = serde_json::json!({
        "version": "1.0",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "cards": cards.iter().map(|c| {
            serde_json::json!({
                "name": card_file_stem(&c.module_name),
                "title": c.module_name,
                "path": format!("cards/{}/{}.md", primary_lang, card_file_stem(&c.module_name)),
            })
        }).collect::<Vec<_>>(),
    });
    let cards_index = output_dir.join("cards").join(primary_lang).join("_index.json");
    std::fs::write(&cards_index, serde_json::to_string_pretty(&cards_index_json)?)?;

    // 4. 生成目录页（命中保护集跳过写盘）
    let toc_path = toc_doc_path(output_dir);
    if !protected.contains(&toc_path.to_string_lossy().to_string()) {
        let toc = markdown::render_table_of_contents(documents);
        std::fs::write(toc_path, toc)?;
    }

    // 5. 生成 Mermaid 依赖图
    let diagrams_dir = assets_dir.join("diagrams");
    std::fs::create_dir_all(&diagrams_dir)?;
    let mermaid_content = mermaid::render_module_dependency_graph(graph);
    std::fs::write(diagrams_dir.join("module-deps.mermaid"), mermaid_content)?;

    // 5.1 模块级调用关系图（Calls 边按模块聚合）
    let call_graph_content = mermaid::render_module_call_graph(graph);
    std::fs::write(diagrams_dir.join("call-graph.mermaid"), call_graph_content)?;

    // 5. 生成交叉引用索引
    let crossref = crossref::CrossRefIndex::build(documents);
    let broken = crossref.validate(documents);
    if !broken.is_empty() {
        tracing::warn!("发现 {} 个断链", broken.len());
        for link in &broken {
            tracing::warn!(
                "  断链: {} -> {} ({})",
                link.source_doc,
                link.broken_target,
                link.link_text
            );
        }
    }

    tracing::info!(
        "输出完成: {} 个页面, {} 个卡片, {} 个模块, 目录: {}",
        documents.len(),
        cards.len(),
        graph.modules.len(),
        config.output.dir
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DocumentKind, KnowledgeCard, WikiDocument};

    fn make_doc(language: &str) -> WikiDocument {
        WikiDocument {
            title: "TestModule".into(),
            kind: DocumentKind::WikiPage,
            content: "## 概述\n\n内容".into(),
            language: language.into(),
            module_path: vec!["src".into(), "testmodule".into()],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
            fingerprint: None,
        }
    }

    fn make_card() -> KnowledgeCard {
        KnowledgeCard {
            module_name: "src::testmodule".into(),
            module_type: "module".into(),
            summary: "摘要".into(),
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
        }
    }

    /// A4：wiki 页与卡片的路径规则收敛后，路径计算必须与
    /// render_all/write_document 的落盘命名完全一致（单测锁死规则，防止漂移）
    #[test]
    fn test_wiki_and_card_path_rules() {
        let doc = make_doc("zh");
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &doc),
            Path::new("out").join("wiki").join("zh").join("src_testmodule.md")
        );
        // ArchitectureOverview 特判写 architecture.md
        let arch = WikiDocument {
            kind: DocumentKind::ArchitectureOverview,
            ..make_doc("zh")
        };
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &arch),
            Path::new("out").join("wiki").join("zh").join("architecture.md")
        );
        // ProjectOverview 特判写 overview.md
        let overview = WikiDocument {
            kind: DocumentKind::ProjectOverview,
            ..make_doc("zh")
        };
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &overview),
            Path::new("out").join("wiki").join("zh").join("overview.md")
        );
        // 卡片命名：module.replace("::","_")，与 card.rs 的 card_path 一致
        assert_eq!(
            card_page_path(Path::new("out"), "zh", "src::testmodule"),
            Path::new("out").join("cards").join("zh").join("src_testmodule.md")
        );
        assert_eq!(module_page_file_name("src::testmodule"), "src_testmodule.md");
    }

    /// A3：人工编辑过的卡片进入保护集后，全量 generate 不覆盖（保留人工编辑版）
    #[test]
    fn test_render_all_skips_protected_card() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_protected_card_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();

        let card = make_card();
        let doc = make_doc("zh");
        let graph = KnowledgeGraph::default();

        // 预写"人工编辑版"卡片（与 render_all 落盘路径一致）
        let card_file = dir.join("cards").join("zh").join("src_testmodule.md");
        std::fs::create_dir_all(card_file.parent().unwrap()).unwrap();
        std::fs::write(&card_file, "人工编辑的内容").unwrap();

        // 保护集命中卡片路径 → 写盘跳过，人工编辑版保留
        let protected: std::collections::HashSet<String> =
            [card_file.to_string_lossy().to_string()].into_iter().collect();
        render_all(std::slice::from_ref(&doc), std::slice::from_ref(&card), &graph, &config, &protected).unwrap();
        let kept = std::fs::read_to_string(&card_file).unwrap();
        assert_eq!(kept, "人工编辑的内容", "被保护的卡片不应被全量 generate 覆盖");

        // 无保护时卡片正常写盘（保护语义开关验证）
        let _ = std::fs::remove_file(&card_file);
        let empty = std::collections::HashSet::new();
        render_all(&[doc], &[card], &graph, &config, &empty).unwrap();
        assert!(card_file.exists(), "未保护的卡片应正常写盘");
        assert!(
            dir.join("wiki").join("zh").join("src_testmodule.md").exists(),
            "wiki 页应正常写盘"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
