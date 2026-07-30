pub mod crossref;
pub mod markdown;
pub mod mermaid;
pub mod html;

use std::path::Path;

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::markdown::write_document;

/// 渲染所有文档到输出目录
///
/// 1. 创建输出目录结构
/// 2. 渲染并写入 Wiki 页面
/// 3. 渲染并写入 Knowledge Card
/// 4. 生成目录页
/// 5. 生成 Mermaid 关系图
pub fn render_all(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
) -> Result<()> {
    let output_dir = Path::new(&config.output.dir);
    let wiki_dir = output_dir.join("wiki");
    let cards_dir = output_dir.join("cards");
    let assets_dir = output_dir.join("assets");

    std::fs::create_dir_all(&wiki_dir)?;
    std::fs::create_dir_all(&cards_dir)?;
    std::fs::create_dir_all(&assets_dir)?;

    // 1. 写入 Wiki 页面
    for doc in documents {
        let doc_cards: Vec<&KnowledgeCard> = cards
            .iter()
            .filter(|c| doc.module_path.iter().any(|p| c.module_name.contains(p)))
            .collect();
        write_document(doc, &doc_cards, output_dir)?;
    }

    // 2. 生成 overview.md（第一个文档作为概览）
    if let Some(first) = documents.first() {
        let overview_content = format!("# 项目概览\n\n{}", first.content);
        std::fs::write(wiki_dir.join("overview.md"), overview_content)?;
    }

    // 3. 写入 Knowledge Card 索引（JSON 格式）
    let cards_index_json = serde_json::json!({
        "version": "1.0",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "cards": cards.iter().map(|c| {
            serde_json::json!({
                "name": c.module_name.replace("::", "_"),
                "title": c.module_name,
                "path": format!("cards/{}.md", c.module_name.replace("::", "_")),
            })
        }).collect::<Vec<_>>(),
    });
    let cards_index = output_dir.join("cards").join("_index.json");
    std::fs::write(&cards_index, serde_json::to_string_pretty(&cards_index_json)?)?;

    // 4. 生成目录页
    let toc = markdown::render_table_of_contents(documents);
    std::fs::write(output_dir.join("_toc.md"), toc)?;

    // 5. 生成 Mermaid 依赖图
    let diagrams_dir = assets_dir.join("diagrams");
    std::fs::create_dir_all(&diagrams_dir)?;
    let mermaid_content = mermaid::render_module_dependency_graph(graph);
    std::fs::write(diagrams_dir.join("module-deps.mermaid"), mermaid_content)?;

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
