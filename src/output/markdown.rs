use std::path::Path;

use anyhow::Result;

use crate::model::{DocumentKind, KnowledgeCard, WikiDocument};

/// 渲染 WikiDocument 为 Markdown 字符串
pub fn render_wiki_page(doc: &WikiDocument) -> String {
    let mut output = String::new();

    // 标题
    output.push_str(&format!("# {}\n\n", doc.title));

    // 元信息
    output.push_str(&format!("> 最后更新: {}\n\n", doc.last_updated));

    // 内容（LLM 生成的主体部分）
    output.push_str(&doc.content);

    // 交叉引用
    if !doc.references.is_empty() {
        output.push_str("\n\n## 交叉引用\n\n");
        for reference in &doc.references {
            let rel = match reference.relation.as_str() {
                "depends_on" => "依赖",
                "used_by" => "被使用",
                "related" => "相关",
                _ => &reference.relation,
            };
            output.push_str(&format!(
                "- [{}]({}) — {}\n",
                reference.target_title, reference.target_path, rel
            ));
        }
    }

    output
}

/// 渲染 KnowledgeCard 为 Markdown（YAML frontmatter 格式）
pub fn render_knowledge_card(card: &KnowledgeCard) -> String {
    let mut output = String::new();

    // YAML frontmatter
    output.push_str("---\n");
    output.push_str(&format!("module_name: {}\n", card.module_name));
    output.push_str(&format!("module_type: {}\n", card.module_type));
    if !card.dependencies.is_empty() {
        output.push_str(&format!(
            "dependencies: [{}]\n",
            card.dependencies.join(", ")
        ));
    }
    if !card.dependents.is_empty() {
        output.push_str(&format!(
            "dependents: [{}]\n",
            card.dependents.join(", ")
        ));
    }
    if !card.design_patterns.is_empty() {
        output.push_str(&format!(
            "design_patterns: [{}]\n",
            card.design_patterns.join(", ")
        ));
    }
    output.push_str("---\n");

    // 内容
    output.push_str(&format!("# {}\n\n", card.module_name));
    output.push_str(&format!("## 摘要\n\n{}\n\n", card.summary));

    // 关键实体
    if !card.key_entities.is_empty() {
        output.push_str("## 关键实体\n\n");
        for entity in &card.key_entities {
            let doc = entity.doc.as_deref().unwrap_or("");
            output.push_str(&format!(
                "- `{}` ({}) — {}\n",
                entity.name, entity.kind, doc
            ));
        }
        output.push('\n');
    }

    // 待办事项
    if !card.todo_notes.is_empty() {
        output.push_str("## 待办事项\n\n");
        for note in &card.todo_notes {
            output.push_str(&format!("- [ ] {}\n", note));
        }
        output.push('\n');
    }

    output
}

/// 渲染目录页 _toc.md
pub fn render_table_of_contents(documents: &[WikiDocument]) -> String {
    let mut output = String::new();
    output.push_str("# Wiki 文档目录\n\n");
    output.push_str(&format!(
        "> 共 {} 个页面\n\n",
        documents.len()
    ));

    for doc in documents {
        let module_path = if doc.module_path.is_empty() {
            "根".to_string()
        } else {
            doc.module_path.join(" > ")
        };
        let kind = match doc.kind {
            DocumentKind::WikiPage => "模块文档",
            DocumentKind::ArchitectureOverview => "架构概览",
            DocumentKind::TableOfContents => "目录",
            DocumentKind::KnowledgeCard => "知识卡片",
            DocumentKind::ModuleDoc => "模块文档",
        };
        output.push_str(&format!(
            "- [{}](wiki/{}.md) `[{}]` — {}\n",
            doc.title,
            doc.module_path.join("_"),
            kind,
            module_path
        ));
    }

    output
}

/// 写文件到磁盘
///
/// 将 WikiDocument 渲染后写入 `{output_dir}/wiki/{module_path}.md`，
/// 关联的 Knowledge Card 写入 `{output_dir}/cards/{module_name}.md`。
pub fn write_document(doc: &WikiDocument, cards: &[&KnowledgeCard], output_dir: &Path) -> Result<()> {
    let wiki_dir = output_dir.join("wiki");
    std::fs::create_dir_all(&wiki_dir)?;

    // Wiki 页面
    let wiki_file_name = if doc.module_path.is_empty() {
        format!("{}.md", doc.title)
    } else {
        format!("{}.md", doc.module_path.join("_"))
    };
    let wiki_path = wiki_dir.join(&wiki_file_name);
    let content = render_wiki_page(doc);
    std::fs::write(&wiki_path, content)?;

    // 关联的 Knowledge Card
    for card in cards {
        let cards_dir = output_dir.join("cards");
        std::fs::create_dir_all(&cards_dir)?;
        let card_path = cards_dir.join(format!("{}.md", card.module_name.replace("::", "_")));
        let card_content = render_knowledge_card(card);
        std::fs::write(&card_path, card_content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntitySummary;
    use crate::model::Reference;

    fn make_test_doc(title: &str) -> WikiDocument {
        WikiDocument {
            title: title.into(),
            kind: DocumentKind::WikiPage,
            content: format!("## 概述\n\n这是 {} 的文档。\n\n## 核心实体\n\n- `Foo` — 核心结构体", title),
            module_path: vec!["crate".into(), title.to_lowercase()],
            references: vec![Reference {
                target_title: "bar".into(),
                target_path: "wiki/bar.md".into(),
                relation: "depends_on".into(),
            }],
            last_updated: "2025-01-01T00:00:00Z".into(),
            fingerprint: None,
        }
    }

    #[test]
    fn test_render_wiki_page() {
        let doc = make_test_doc("Config");
        let output = render_wiki_page(&doc);

        assert!(output.contains("# Config"));
        assert!(output.contains("## 概述"));
        assert!(output.contains("`Foo`"));
        assert!(output.contains("交叉引用"));
        assert!(output.contains("bar"));
    }

    #[test]
    fn test_render_knowledge_card() {
        let card = KnowledgeCard {
            module_name: "crate::config".into(),
            module_type: "module".into(),
            summary: "配置管理模块".into(),
            key_entities: vec![EntitySummary {
                name: "Config".into(),
                kind: "struct".into(),
                visibility: "public".into(),
                doc: Some("配置结构体".into()),
            }],
            dependencies: vec!["serde".into()],
            dependents: vec![],
            design_patterns: vec!["Builder".into()],
            todo_notes: vec!["增加环境变量支持".into()],
        };

        let output = render_knowledge_card(&card);
        assert!(output.starts_with("---"));
        assert!(output.contains("module_name: crate::config"));
        assert!(output.contains("dependencies: [serde]"));
        assert!(output.contains("design_patterns: [Builder]"));
        assert!(output.contains("## 摘要"));
        assert!(output.contains("配置管理模块"));
        assert!(output.contains("`Config`"));
        assert!(output.contains("增加环境变量支持"));
    }

    #[test]
    fn test_render_table_of_contents() {
        let docs = vec![make_test_doc("Config"), make_test_doc("Server")];
        let output = render_table_of_contents(&docs);

        assert!(output.contains("# Wiki 文档目录"));
        assert!(output.contains("Config"));
        assert!(output.contains("Server"));
        assert!(output.contains("2 个页面"));
    }

    #[test]
    fn test_write_document_roundtrip() {
        let doc = make_test_doc("TestModule");
        let card = KnowledgeCard {
            module_name: "crate::test_module".into(),
            module_type: "module".into(),
            summary: "测试".into(),
            key_entities: vec![],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
        };

        let dir = std::env::temp_dir().join("repo-wiki-test-markdown");
        let _ = std::fs::remove_dir_all(&dir);

        write_document(&doc, &[&card], &dir).unwrap();

        assert!(dir.join("wiki").join("crate_testmodule.md").exists());
        assert!(dir.join("cards").join("crate_test_module.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
