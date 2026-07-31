pub mod crossref;
pub mod markdown;
pub mod mermaid;
pub mod html;

use std::path::Path;

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::markdown::write_document;

/// 生效的 wiki 语言列表（主语言 + 扩展语言），与生成层保持一致
pub fn wiki_languages(config: &WikiConfig) -> Vec<String> {
    crate::generate::collect_languages(config)
}

/// 渲染所有文档到输出目录
///
/// 1. 创建输出目录结构（主语言 + 扩展语言）
/// 2. 按文档自身语言渲染并写入 Wiki 页面（多语言独立生成，不再按语言循环复制）
/// 3. 渲染并写入 Knowledge Card
/// 4. 生成目录页与概览页（主语言）
/// 5. 生成 Mermaid 关系图
///
/// `protected` 为人工修改保护集（路径字符串），命中路径跳过写盘。
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
        // 路径计算与 markdown::write_document 保持一致（人工修改保护判定依据）
        let wiki_path = if doc.kind == crate::model::DocumentKind::ArchitectureOverview {
            output_dir.join("wiki").join(&doc.language).join("architecture.md")
        } else {
            output_dir
                .join("wiki")
                .join(&doc.language)
                .join(markdown::wiki_file_name(doc))
        };
        if protected.contains(&wiki_path.to_string_lossy().to_string()) {
            continue;
        }
        let doc_cards: Vec<&KnowledgeCard> = cards
            .iter()
            .filter(|c| doc.module_path.iter().any(|p| c.module_name.contains(p)))
            .collect();
        write_document(doc, &doc_cards, output_dir, &doc.language)?;
    }

    // 1.5 写入 API 参考页（按模块分组的实体清单，每种语言独立目录）
    for lang in &languages {
        let api_doc = markdown::render_api_reference(graph);
        std::fs::write(
            output_dir.join("wiki").join(lang).join("api.md"),
            api_doc.content,
        )?;
    }

    // 2. 生成 overview.md（第一个文档作为概览，写入主语言目录）
    if let Some(first) = documents.first() {
        let overview_content = format!("# 项目概览\n\n{}", first.content);
        std::fs::write(output_dir.join("wiki").join(&languages[0]).join("overview.md"), overview_content)?;
    }

    // 3. 写入 Knowledge Card 索引（JSON 格式，写入主语言目录）
    let primary_lang = &languages[0];
    let cards_index_json = serde_json::json!({
        "version": "1.0",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "cards": cards.iter().map(|c| {
            serde_json::json!({
                "name": c.module_name.replace("::", "_"),
                "title": c.module_name,
                "path": format!("cards/{}/{}.md", primary_lang, c.module_name.replace("::", "_")),
            })
        }).collect::<Vec<_>>(),
    });
    let cards_index = output_dir.join("cards").join(primary_lang).join("_index.json");
    std::fs::write(&cards_index, serde_json::to_string_pretty(&cards_index_json)?)?;

    // 4. 生成目录页
    let toc = markdown::render_table_of_contents(documents);
    std::fs::write(output_dir.join("_toc.md"), toc)?;

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
