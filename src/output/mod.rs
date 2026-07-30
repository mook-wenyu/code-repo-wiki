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
        write_document(doc, &doc_cards, &output_dir)?;
    }

    // 2. 写入 Knowledge Card（YAML frontmatter 格式）
    let cards_content = cards
        .iter()
        .map(|c| markdown::render_knowledge_card(c))
        .collect::<Vec<_>>()
        .join("\n---\n");
    let cards_index = output_dir.join("cards").join("_index.md");
    std::fs::write(&cards_index, cards_content)?;

    // 3. 生成目录页
    let toc = markdown::render_table_of_contents(documents);
    std::fs::write(output_dir.join("_toc.md"), toc)?;

    // 4. 生成 Mermaid 依赖图
    let mermaid_content = mermaid::render_module_dependency_graph(graph);
    std::fs::write(assets_dir.join("module-deps.mermaid"), mermaid_content)?;

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
