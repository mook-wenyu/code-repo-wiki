use std::path::Path;

use anyhow::Result;

use crate::model::{DocumentKind, KnowledgeCard, KnowledgeGraph, WikiDocument};
use crate::output::crossref::render_cite_link;

/// 渲染 WikiDocument 为 Markdown 字符串
pub fn render_wiki_page(doc: &WikiDocument) -> String {
    let mut output = String::new();

    // 标题
    output.push_str(&format!("# {}\n\n", doc.title));

    // 元信息
    output.push_str(&format!("> 最后更新: {}\n\n", doc.last_updated));

    // v32 10.2：git 基线行（仅 git 仓库有 HEAD 时输出；非 git 仓库省略，
    // 页面仍只有「最后更新」时间戳）。HEAD 非易变信号——同一提交下多次
    // 生成值不变，与 test_determinism 内容级哈希兼容（时间戳才需归一化）。
    if let Some(commit) = &doc.based_on_commit {
        output.push_str(&format!("> 基于提交: {}\n\n", commit));
    }

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
                "- {} — {}\n",
                render_cite_link(&reference.target_title, &reference.target_path), rel
            ));
        }
    }

    output
}

/// 渲染 API 参考页（按模块分组，每实体一行）
///
/// 输出 `签名 — 文档注释 — 文件:行` 格式，供 api-ref 模板的页面使用。
/// 只收录代码实体节点，跳过 project/module/file 容器节点。
pub fn render_api_reference(graph: &KnowledgeGraph) -> WikiDocument {
    let mut lines = vec!["# API 参考".to_string(), String::new()];

    for module in &graph.modules {
        lines.push(format!("## {}", module.name));
        lines.push(String::new());
        for nid in &module.node_ids {
            let Some(node) = graph.graph.node_weight(*nid) else {
                continue;
            };
            // 容器节点没有 API 形态，跳过
            if matches!(
                node.kind,
                crate::model::NodeKind::Project
                    | crate::model::NodeKind::Module
                    | crate::model::NodeKind::File
            ) {
                continue;
            }
            // 签名优先，缺失时退回实体名
            let signature = node.signature.as_deref().unwrap_or(node.name.as_str());
            // 文档注释多行时只取首行，保持一行一实体
            let doc = node
                .doc_comment
                .as_deref()
                .map(|d| d.lines().next().unwrap_or(""))
                .unwrap_or("");
            // v21 G 组：实体行增强标注——追加类型中文名与可见性修饰符
            // （如 `- \`pub fn load(...)\` (函数, pub) — ...`），使文档读者
            // 一眼可知实体形态与可访问性。可见性由解析器按行级文本提取
            // （Entity.visibility → CodeNode.visibility），缺失时省略标注。
            let kind_zh = kind_label(&node.kind);
            let vis_part = node
                .visibility
                .as_deref()
                .map(|v| format!(", {v}"))
                .unwrap_or_default();
            let mut line = format!("- `{}` ({kind_zh}{vis_part}) — {}", signature, doc);
            // 文件:行定位（无行号信息时省略）
            if let Some(file) = node.file_path.as_deref() {
                if let Some((start, _)) = node.line_range {
                    line.push_str(&format!(" — {}:{}", file, start));
                } else {
                    line.push_str(&format!(" — {}", file));
                }
            }
            lines.push(line);
        }
        lines.push(String::new());
    }

    WikiDocument {
        title: "API 参考".into(),
        kind: DocumentKind::ApiReference,
        content: lines.join("\n"),
        language: String::new(),
        module_path: vec![],
        references: vec![],
        last_updated: chrono::Utc::now().to_rfc3339(),
        // API 参考页由代码图渲染（非 LLM 页），不带 git 基线行
        based_on_commit: None,
        fingerprint: None,
    }
}

/// 实体类型的中文标签（api.md 实体行增强标注用）
fn kind_label(kind: &crate::model::NodeKind) -> &'static str {
    match kind {
        crate::model::NodeKind::Project => "项目",
        crate::model::NodeKind::Module => "模块",
        crate::model::NodeKind::File => "文件",
        crate::model::NodeKind::Struct => "结构体",
        crate::model::NodeKind::Enum => "枚举",
        crate::model::NodeKind::Function => "函数",
        crate::model::NodeKind::Trait => "Trait",
        crate::model::NodeKind::Impl => "实现",
        crate::model::NodeKind::Type => "类型别名",
        crate::model::NodeKind::Constant => "常量",
        crate::model::NodeKind::Variable => "变量",
        crate::model::NodeKind::Interface => "接口",
        crate::model::NodeKind::Class => "类",
        crate::model::NodeKind::Macro => "宏",
    }
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
    if !card.tech_stack.is_empty() {
        output.push_str(&format!("tech_stack: [{}]\n", card.tech_stack.join(", ")));
    }
    output.push_str("---\n");

    // 内容
    output.push_str(&format!("# {}\n\n", card.module_name));
    output.push_str(&format!("## 摘要\n\n{}\n\n", card.summary));

    // 关键实体（source 为回填的源码定位反向链接，T3.3）
    if !card.key_entities.is_empty() {
        output.push_str("## 关键实体\n\n");
        for entity in &card.key_entities {
            let doc = entity.doc.as_deref().unwrap_or("");
            if let Some(src) = &entity.source {
                output.push_str(&format!(
                    "- `{}` ({}) — {} [源码:{}]\n",
                    entity.name, entity.kind, doc, src
                ));
            } else {
                output.push_str(&format!(
                    "- `{}` ({}) — {}\n",
                    entity.name, entity.kind, doc
                ));
            }
        }
        output.push('\n');
    }

    // 相关文件（来自 chunk 源文件列表，非 LLM 输出）
    if !card.related_files.is_empty() {
        output.push_str("## 相关文件\n\n");
        for f in &card.related_files {
            output.push_str(&format!("- `{}`\n", f));
        }
        output.push('\n');
    }

    // 编码规范
    if let Some(spec) = &card.coding_spec {
        output.push_str(&format!("## 编码规范\n\n{}\n\n", spec));
    }

    // 架构说明
    if let Some(arch) = &card.architecture {
        output.push_str(&format!("## 架构说明\n\n{}\n\n", arch));
    }

    // 待办事项
    if !card.todo_notes.is_empty() {
        output.push_str("## 待办事项\n\n");
        for note in &card.todo_notes {
            output.push_str(&format!("- [ ] {}\n", note));
        }
        output.push('\n');
    }

    // 特征追溯（演进计划 T3.3：本模块参与的实体级特征，非空时渲染）
    if !card.features.is_empty() {
        output.push_str("## 特征追溯\n\n");
        for f in &card.features {
            output.push_str(&format!("- `{}`\n", f));
        }
        output.push('\n');
    }

    // 人工修改待同步（增量管道注入的记录，仅非空时渲染避免空节）
    if !card.pending_manual_edits.is_empty() {
        output.push_str("## 人工修改待同步\n\n");
        for note in &card.pending_manual_edits {
            output.push_str(&format!("- {}\n", note));
        }
        output.push('\n');
    }

    output
}

/// 渲染目录页 _toc.md
///
/// 按模块分组（Karpathy LLM Wiki 的"index 优先导航"最佳实践）：
/// 模块文档按 module_path 前缀分组展示，全局文档（架构/概览/API/目录）单列。
/// 链接路径与写盘命名保持一致（wiki/{doc.language}/{file}）。
pub fn render_table_of_contents(documents: &[WikiDocument]) -> String {
    let mut output = String::new();
    output.push_str("# Wiki 文档目录\n\n");
    output.push_str(&format!("> 共 {} 个页面\n\n", documents.len()));

    // 按 module_path 前缀分组:模块文档归入各自模块,全局文档单列
    let mut module_docs: std::collections::BTreeMap<String, Vec<&WikiDocument>> = Default::default();
    let mut global_docs: Vec<&WikiDocument> = Vec::new();
    for doc in documents {
        if doc.module_path.is_empty() {
            global_docs.push(doc);
        } else {
            module_docs
                .entry(doc.module_path.join("::"))
                .or_default()
                .push(doc);
        }
    }

    // 全局文档（架构/概览/API/目录）优先
    if !global_docs.is_empty() {
        output.push_str("## 全局文档\n\n");
        for doc in &global_docs {
            output.push_str(&render_toc_line(doc));
        }
        output.push('\n');
    }

    // 模块文档按模块分组
    output.push_str("## 模块\n\n");
    for (module, docs) in &module_docs {
        output.push_str(&format!("### {module}\n\n"));
        for doc in docs {
            output.push_str(&render_toc_line(doc));
        }
        output.push('\n');
    }

    output
}

/// 渲染 TOC 单行:链接 + 类型标签 + 模块路径
fn render_toc_line(doc: &WikiDocument) -> String {
    let kind = match doc.kind {
        DocumentKind::WikiPage => "模块文档",
        DocumentKind::ArchitectureOverview => "架构概览",
        DocumentKind::ProjectOverview => "项目概览",
        DocumentKind::TableOfContents => "目录",
        DocumentKind::KnowledgeCard => "知识卡片",
        DocumentKind::ApiReference => "API 参考",
        DocumentKind::DatabaseSchema => "数据库 Schema",
    };
    // 链接的文件名必须与 write_document 的落盘命名保持一致，否则 TOC 就是断链：
    // 1. 所有文档都写在 wiki/{doc.language}/ 语言目录下，链接必须带语言前缀；
    // 2. 架构概览固定写为 architecture.md（见 write_document），不能走 module_path 派生；
    // 3. 项目概览固定写为 overview.md，与 wiki_page_path 特判保持一致；
    // 4. 其余文档用 wiki_file_name（模块路径或标题派生，覆盖 Database Schema 等无模块路径文档）。
    let file = match doc.kind {
        DocumentKind::ArchitectureOverview => "architecture.md".to_string(),
        DocumentKind::ProjectOverview => "overview.md".to_string(),
        _ => wiki_file_name(doc),
    };
    let module_path = if doc.module_path.is_empty() {
        "根".to_string()
    } else {
        doc.module_path.join(" > ")
    };
    format!(
        "- [{}](wiki/{}/{}) `[{}]` — {}\n",
        doc.title, doc.language, file, kind, module_path
    )
}

/// 计算 Wiki 页面文件名
///
/// module_path 为空时用标题，标题中的路径分隔符与 Windows 非法字符（/ \ :）
/// 替换为 '-'，避免生成嵌套目录或写盘失败（如 Database Schema 文档标题含路径）。
pub fn wiki_file_name(doc: &WikiDocument) -> String {
    if doc.module_path.is_empty() {
        format!("{}.md", doc.title.replace(['/', '\\', ':'], "-"))
    } else {
        format!("{}.md", doc.module_path.join("_"))
    }
}

/// 写文件到磁盘
///
/// 将 WikiDocument 渲染后写入 `{output_dir}/wiki/{language}/{module_path}.md`。
/// 路径统一由 output::wiki_page_path 产出，与 render_all 的保护判定路径
/// 同一规则，保证命名不会漂移。
///
/// v22 修复（Unity 实测）：Knowledge Card 写盘原先绑定在本函数（页面成功
/// 才写卡片）——模块页面 LLM 生成失败时卡片也丢失，产出「快照/_index
/// 有、磁盘无」的不一致。卡片写盘已移至 render_all 独立循环（页面失败
/// 不影响卡片落盘），本函数只负责页面。
pub fn write_document(doc: &WikiDocument, output_dir: &Path, language: &str) -> Result<()> {
    let wiki_dir = output_dir.join("wiki").join(language);
    std::fs::create_dir_all(&wiki_dir)?;

    let wiki_path = crate::output::wiki_page_path(output_dir, language, doc);
    let content = render_wiki_page(doc);
    crate::fs::write_file_atomic(&wiki_path, &content)?;

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
            language: "zh".into(),
            module_path: vec!["crate".into(), title.to_lowercase()],
            references: vec![Reference {
                target_title: "bar".into(),
                target_path: "wiki/bar.md".into(),
                relation: "depends_on".into(),
            }],
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
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
                source: None,
            }],
            dependencies: vec!["serde".into()],
            dependents: vec![],
            design_patterns: vec!["Builder".into()],
            todo_notes: vec!["增加环境变量支持".into()],
            related_files: vec!["src/config.rs".into()],
            coding_spec: Some("遵循 rustfmt".into()),
            tech_stack: vec!["serde".into()],
            architecture: Some("分层".into()),
            pending_manual_edits: vec!["人工修改待同步: wiki/zh/src_config.md 内容摘要: 手动改".into()],
            features: Vec::new(),
        };

        let output = render_knowledge_card(&card);
        assert!(output.starts_with("---"));
        assert!(output.contains("module_name: crate::config"));
        assert!(output.contains("dependencies: [serde]"));
        assert!(output.contains("design_patterns: [Builder]"));
        assert!(output.contains("tech_stack: [serde]"));
        assert!(output.contains("## 摘要"));
        assert!(output.contains("配置管理模块"));
        assert!(output.contains("`Config`"));
        assert!(output.contains("增加环境变量支持"));
        assert!(output.contains("## 相关文件"));
        assert!(output.contains("src/config.rs"));
        assert!(output.contains("## 编码规范"));
        assert!(output.contains("遵循 rustfmt"));
        assert!(output.contains("## 架构说明"));
        assert!(output.contains("分层"));
        assert!(output.contains("## 人工修改待同步"));
        assert!(output.contains("内容摘要: 手动改"));
    }

    #[test]
    fn test_render_knowledge_card_skips_empty_pending_edits() {
        let card = KnowledgeCard {
            module_name: "crate::config".into(),
            module_type: "module".into(),
            summary: "配置管理模块".into(),
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
        };

        let output = render_knowledge_card(&card);
        assert!(!output.contains("人工修改待同步"), "无记录时不应渲染空节");
    }

    #[test]
    fn test_render_table_of_contents() {
        let mut docs = vec![make_test_doc("Config"), make_test_doc("Server")];
        // 架构概览无模块路径，链接固定指向 architecture.md（与 write_document 命名一致）
        docs.push(WikiDocument {
            title: "架构概览".into(),
            kind: DocumentKind::ArchitectureOverview,
            content: String::new(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        });
        let output = render_table_of_contents(&docs);

        assert!(output.contains("# Wiki 文档目录"));
        assert!(output.contains("Config"));
        assert!(output.contains("Server"));
        assert!(output.contains("3 个页面"));
        // 链接必须带语言目录前缀，与实际落盘路径 wiki/{lang}/{file}.md 一致
        assert!(output.contains("](wiki/zh/crate_config.md)"));
        assert!(output.contains("](wiki/zh/crate_server.md)"));
        assert!(output.contains("](wiki/zh/architecture.md)"));
    }

    /// TOC 按模块分组（index 优先导航）：全局文档与模块文档分节
    #[test]
    fn test_render_table_of_contents_groups_by_module() {
        let mut docs = vec![make_test_doc("Config"), make_test_doc("Server")];
        docs.push(WikiDocument {
            title: "项目概览".into(),
            kind: DocumentKind::ProjectOverview,
            content: String::new(),
            language: "zh".into(),
            module_path: vec![],
            references: vec![],
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        });
        let output = render_table_of_contents(&docs);

        // 全局文档节与模块节并存
        assert!(output.contains("## 全局文档"), "应有全局文档节");
        assert!(output.contains("## 模块"), "应有模块节");
        // 模块文档归入 module_path 分组头
        assert!(
            output.contains("### crate::config"),
            "模块文档应按 module_path 分组, 实际: {output}"
        );
        // 全局文档在全局节内(项目概览 title 出现), 且不在模块节内
        let global_section = output.split("## 模块").next().unwrap_or("");
        assert!(
            global_section.contains("项目概览"),
            "项目概览应在全局文档节"
        );
    }

    #[test]
    fn test_render_api_reference() {
        // 构造含容器节点 + 实体的图
        let mut g = petgraph::stable_graph::StableDiGraph::<
            crate::model::CodeNode,
            crate::model::CodeEdge,
        >::new();
        let file_id = g.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: crate::model::NodeKind::File,
            name: "config.rs".into(),
            file_path: Some("src/config.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec!["crate".into(), "config".into()],
        });
        let fn_id = g.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(1),
            kind: crate::model::NodeKind::Function,
            name: "load".into(),
            file_path: Some("src/config.rs".into()),
            line_range: Some((12, 20)),
            doc_comment: Some("加载配置\n\n多行注释".into()),
            signature: Some("pub fn load(path: &str) -> Result<Config>".into()), visibility: Some("pub".into()),
            module_path: vec!["crate".into(), "config".into()],
        });
        let graph = KnowledgeGraph {
            graph: g,
            modules: vec![crate::model::ModuleCluster {
                name: "crate::config".into(),
                node_ids: vec![file_id, fn_id],
                cohesion: 0.8,
                coupling: 0.2,
                description: None,
            }],
            features: Vec::new(),
        };

        let doc = render_api_reference(&graph);
        assert_eq!(doc.kind, DocumentKind::ApiReference);
        // 容器节点被跳过，只输出函数实体
        assert!(doc.content.contains("## crate::config"));
        // v21 G 组：实体行带类型中文标注 + 可见性修饰符
        assert!(doc.content.contains("- `pub fn load(path: &str) -> Result<Config>` (函数, pub) — 加载配置 — src/config.rs:12"));
        assert!(!doc.content.contains("config.rs` —"));
    }

    #[test]
    fn test_render_api_reference_omits_visibility_when_absent() {
        // 无可见性信息（如 Python/Go 无修饰符语法）时标注省略可见性段
        let mut g = petgraph::stable_graph::StableDiGraph::<
            crate::model::CodeNode,
            crate::model::CodeEdge,
        >::new();
        let fn_id = g.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: crate::model::NodeKind::Function,
            name: "run".into(),
            file_path: Some("src/main.py".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: Some("def run()".into()), visibility: None,
            module_path: vec!["main".into()],
        });
        let graph = KnowledgeGraph {
            graph: g,
            modules: vec![crate::model::ModuleCluster {
                name: "main".into(),
                node_ids: vec![fn_id],
                cohesion: 1.0,
                coupling: 0.0,
                description: None,
            }],
            features: Vec::new(),
        };
        let doc = render_api_reference(&graph);
        assert!(doc.content.contains("- `def run()` (函数) —  — src/main.py:1"));
        assert!(!doc.content.contains(", pub)"));
    }

    #[test]
    fn test_write_document_roundtrip() {
        let doc = make_test_doc("TestModule");

        let dir = std::env::temp_dir().join("repo-wiki-test-markdown");
        let _ = std::fs::remove_dir_all(&dir);

        write_document(&doc, &dir, "zh").unwrap();

        assert!(dir.join("wiki").join("zh").join("crate_testmodule.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
