use std::path::Path;

use anyhow::{Context, Result};
use pulldown_cmark::{Options, Parser, html};

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

/// 将整个 Wiki 输出导出为 HTML 页面集合
///
/// 输出结构：
///   {output.dir}/wiki/{name}.html       — 每个文档的 HTML 页面
///   {output.dir}/index.html             — 目录页
///   {output.dir}/style.css              — 基本样式
///   {output.dir}/cards/{name}.html      — Knowledge Card 页面
///   {output.dir}/assets/module-deps.html — Mermaid 模块依赖图
pub fn export_html(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
) -> Result<()> {
    let output_dir = Path::new(&config.output.dir);
    let wiki_dir = output_dir.join("wiki");
    let cards_dir = output_dir.join("cards");
    let assets_dir = output_dir.join("assets");

    std::fs::create_dir_all(&wiki_dir)
        .with_context(|| format!("创建 wiki 目录失败: {}", wiki_dir.display()))?;
    std::fs::create_dir_all(&cards_dir)
        .with_context(|| format!("创建 cards 目录失败: {}", cards_dir.display()))?;
    std::fs::create_dir_all(&assets_dir)
        .with_context(|| format!("创建 assets 目录失败: {}", assets_dir.display()))?;

    // 每个 WikiDocument → wiki/{name}.html
    for doc in documents {
        let body = md_to_html(&doc.content);
        let html = wrap_html(&doc.title, &body, "../style.css");
        let file_name = sanitize_filename(&doc.title);
        let path = wiki_dir.join(format!("{}.html", file_name));
        write_html_file(&path, &html)?;
    }

    // 生成 index.html（目录页）：按模块分组（与 _toc.md 的 index 优先导航一致）
    let mut module_groups: std::collections::BTreeMap<String, Vec<&WikiDocument>> = Default::default();
    let mut global_docs: Vec<&WikiDocument> = Vec::new();
    for doc in documents {
        if doc.module_path.is_empty() {
            global_docs.push(doc);
        } else {
            module_groups
                .entry(doc.module_path.join("::"))
                .or_default()
                .push(doc);
        }
    }
    let mut toc_items = String::new();
    if !global_docs.is_empty() {
        toc_items.push_str("<h2>全局文档</h2>\n<ul>\n");
        for doc in &global_docs {
            let file_name = sanitize_filename(&doc.title);
            toc_items.push_str(&format!(
                "<li><a href=\"wiki/{}.html\">{}</a></li>\n",
                file_name,
                escape_html(&doc.title)
            ));
        }
        toc_items.push_str("</ul>\n");
    }
    toc_items.push_str("<h2>模块</h2>\n");
    for (module, docs) in &module_groups {
        toc_items.push_str(&format!("<h3>{}</h3>\n<ul>\n", escape_html(module)));
        for doc in docs {
            let file_name = sanitize_filename(&doc.title);
            toc_items.push_str(&format!(
                "<li><a href=\"wiki/{}.html\">{}</a></li>\n",
                file_name,
                escape_html(&doc.title)
            ));
        }
        toc_items.push_str("</ul>\n");
    }
    let toc_body = format!(
        "<h1>Wiki 目录</h1>\n<p>共 {} 个文档</p>\n{}\n",
        documents.len(),
        toc_items
    );
    let toc_html = wrap_html("Wiki 目录", &toc_body, "style.css");
    write_html_file(&output_dir.join("index.html"), &toc_html)?;

    // 生成 style.css
    let css = r#"body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    line-height: 1.6;
    max-width: 960px;
    margin: 0 auto;
    padding: 20px;
    color: #333;
}
h1, h2, h3, h4 { margin-top: 1.5em; margin-bottom: 0.5em; }
code { background: #f4f4f4; padding: 2px 6px; border-radius: 3px; font-size: 0.9em; }
pre { background: #f4f4f4; padding: 16px; border-radius: 6px; overflow-x: auto; }
pre code { background: none; padding: 0; }
table { border-collapse: collapse; width: 100%; margin: 1em 0; }
th, td { border: 1px solid #ddd; padding: 8px 12px; text-align: left; }
th { background: #f8f8f8; }
a { color: #0366d6; text-decoration: none; }
a:hover { text-decoration: underline; }
ul, ol { padding-left: 24px; }
blockquote { border-left: 4px solid #ddd; margin: 0; padding: 0 16px; color: #666; }
"#;
    write_html_file(&output_dir.join("style.css"), css)?;

    // 生成 assets/module-deps.html（通过 CDN 嵌入 Mermaid）
    if !graph.modules.is_empty() {
        let mut mermaid_lines = vec!["graph TD".to_string()];
        for module in &graph.modules {
            if let Some(_dep) = module.node_ids.first() {
                // ponytail: 简化为模块级节点，不展开到每个实体
                mermaid_lines.push(format!(
                    "    {}[\"{}\"]",
                    module.name.replace(|c: char| !c.is_alphanumeric(), "_"),
                    escape_html(&module.name)
                ));
            }
        }
        let mermaid_code = mermaid_lines.join("\n");

        let mermaid_html = format!(
            r#"<!DOCTYPE html>
<html lang="zh">
<head><meta charset="utf-8"><title>模块依赖图</title>
<script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
<script>mermaid.initialize({{startOnLoad:true}});</script>
<style>body{{font-family:sans-serif;padding:20px;}}</style>
</head>
<body>
<h1>模块依赖图</h1>
<div class="mermaid">
{}
</div>
</body>
</html>"#,
            mermaid_code
        );
        write_html_file(&assets_dir.join("module-deps.html"), &mermaid_html)?;
    }

    // KnowledgeCard → cards/{name}.html
    for card in cards {
        let mut body = format!(
            "<h1>{}</h1>\n<p><strong>类型:</strong> {}</p>\n<p>{}</p>\n",
            escape_html(&card.module_name),
            escape_html(&card.module_type),
            escape_html(&card.summary)
        );

        if !card.key_entities.is_empty() {
            body.push_str("<h2>关键实体</h2>\n<ul>\n");
            for entity in &card.key_entities {
                body.push_str(&format!(
                    "  <li><strong>{}</strong> ({}) — {}</li>\n",
                    escape_html(&entity.name),
                    escape_html(&entity.kind),
                    entity.doc.as_deref().unwrap_or("")
                ));
            }
            body.push_str("</ul>\n");
        }

        if !card.dependencies.is_empty() {
            body.push_str(&format!(
                "<h2>依赖</h2>\n<p>{}</p>\n",
                card.dependencies
                    .iter()
                    .map(|d| escape_html(d))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        if !card.design_patterns.is_empty() {
            body.push_str(&format!(
                "<h2>设计模式</h2>\n<ul>\n<li>{}</li>\n</ul>\n",
                card.design_patterns
                    .iter()
                    .map(|p| escape_html(p))
                    .collect::<Vec<_>>()
                    .join("</li>\n<li>")
            ));
        }

        // 人工修改待同步（与 markdown 渲染对应，仅非空时输出）
        if !card.pending_manual_edits.is_empty() {
            body.push_str("<h2>人工修改待同步</h2>\n<ul>\n");
            for note in &card.pending_manual_edits {
                body.push_str(&format!("  <li>{}</li>\n", escape_html(note)));
            }
            body.push_str("</ul>\n");
        }

        let html = wrap_html(&card.module_name, &body, "../style.css");
        let file_name = sanitize_filename(&card.module_name);
        let path = cards_dir.join(format!("{}.html", file_name));
        write_html_file(&path, &html)?;
    }

    Ok(())
}

/// 生成完整 HTML 文档，包含 <!DOCTYPE>、<head> 和样式链接
/// 包装完整 HTML 文档。
///
/// `css_href` 为样式表相对路径：index.html 位于产物根（与 style.css 同目录，
/// 用 `style.css`）；wiki/ 与 cards/ 下的页面在子目录（用 `../style.css`）。
/// 硬编码单一路径会让 index.html 引用错误位置（样式失效）。
fn wrap_html(title: &str, body: &str, css_href: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="{css_href}">
</head>
<body>
{body}
</body>
</html>"#,
        title = escape_html(title),
        body = body,
        css_href = css_href
    )
}

/// 使用 pulldown-cmark 将 Markdown 文本转换为 HTML
fn md_to_html(markdown: &str) -> String {
    let options = Options::all();
    let parser = Parser::new_ext(markdown, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
}

/// 写入 HTML 文件到磁盘，自动创建父目录
fn write_html_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("写入文件失败: {}", path.display()))
}

/// 将标题转为安全的文件名（去除非字母数字字符）
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// HTML 转义
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntitySummary, ModuleCluster, KnowledgeCard, KnowledgeGraph, WikiDocument};
    use crate::config::schema::{WikiConfig, OutputSection};

    fn test_config() -> WikiConfig {
        WikiConfig {
            output: OutputSection {
                dir: ".repo-wiki".to_string(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_wrap_html_has_doctype_and_closing_tags() {
        let html = wrap_html("测试页面", "<p>Hello</p>", "style.css");
        assert!(html.starts_with("<!DOCTYPE html>"), "应该以 DOCTYPE 开头");
        assert!(html.contains("<title>测试页面</title>"), "应该包含 title");
        assert!(html.contains("</html>"), "应该包含闭合 html 标签");
        assert!(html.contains("</body>"), "应该包含闭合 body 标签");
        assert!(html.contains("style.css"), "应该引用 style.css");
    }

    /// 样式表相对路径随文档层级变化：index.html 在产物根用 style.css，
    /// wiki/cards 子目录页面用 ../style.css（样式才能正确加载）
    #[test]
    fn test_wrap_html_css_href_follows_depth() {
        let index = wrap_html("目录", "<p>x</p>", "style.css");
        assert!(
            index.contains(r#"href="style.css""#),
            "index 应引用 style.css, 实际: {index}"
        );
        let page = wrap_html("页面", "<p>x</p>", "../style.css");
        assert!(
            page.contains(r#"href="../style.css""#),
            "子目录页面应引用 ../style.css, 实际: {page}"
        );
    }

    #[test]
    fn test_wrap_html_escapes_title() {
        let html = wrap_html("<script>alert('xss')</script>", "<p>body</p>", "style.css");
        assert!(!html.contains("<script>"), "title 中的 HTML 应该被转义");
        assert!(html.contains("&lt;script&gt;"), "title 中的 < 应该被转义为 &lt;");
    }

    #[test]
    fn test_md_to_html_renders_paragraph() {
        let result = md_to_html("Hello **world**");
        assert!(result.contains("<p>"), "应该生成 p 标签");
        assert!(result.contains("<strong>"), "应该生成 strong 标签");
        assert!(result.contains("world"), "应该保留文本内容");
    }

    #[test]
    fn test_md_to_html_renders_code_block() {
        let result = md_to_html("```rust\nfn main() {}\n```");
        assert!(result.contains("<pre>"), "代码块应该生成 pre 标签");
        // pulldown-cmark 输出为 <code class=language-xxx>...</code>，<code> 后有 class 属性
        assert!(result.contains("<code "), "代码块应该生成 code 标签");
    }

    #[test]
    fn test_md_to_html_renders_table() {
        let result = md_to_html("| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert!(result.contains("<table>"), "表格应该生成 table 标签");
    }

    #[test]
    fn test_sanitize_filename_replaces_special_chars() {
        assert_eq!(sanitize_filename("Hello World"), "Hello_World");
        assert_eq!(sanitize_filename("a/b:c"), "a_b_c");
        assert_eq!(sanitize_filename("normal"), "normal");
    }

    #[test]
    fn test_escape_html_escapes_special_chars() {
        assert_eq!(escape_html("<>&'\""), "&lt;&gt;&amp;&#39;&quot;");
        assert_eq!(escape_html("plain text"), "plain text");
    }

    #[test]
    fn test_export_html_creates_files() -> Result<()> {
        let dir = std::env::temp_dir().join("repo-wiki-test-html-export");
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = test_config();
        config.output.dir = dir.to_str().unwrap().to_string();

        let doc = WikiDocument {
            title: "测试模块".to_string(),
            kind: crate::model::DocumentKind::WikiPage,
            content: "# 测试\n\nHello world.".to_string(),
            language: "zh".to_string(),
            module_path: vec!["test".to_string()],
            references: vec![],
            last_updated: "2025-01-01".to_string(),
            fingerprint: None,
        };

        let card = KnowledgeCard {
            module_name: "核心模块".to_string(),
            module_type: "库".to_string(),
            summary: "负责核心功能".to_string(),
            key_entities: vec![EntitySummary {
                name: "run".to_string(),
                kind: "函数".to_string(),
                visibility: "pub".to_string(),
                doc: Some("入口函数".to_string()),
                source: None,
            }],
            dependencies: vec!["serde".to_string()],
            dependents: vec![],
            design_patterns: vec!["工厂模式".to_string()],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            pending_manual_edits: vec!["人工修改待同步: wiki/zh/核心模块.md 内容摘要: 手动改".into()],
            features: Vec::new(),
        };

        let graph = KnowledgeGraph {
            graph: petgraph::stable_graph::StableDiGraph::new(),
            modules: vec![ModuleCluster {
                name: "核心".to_string(),
                node_ids: vec![],
                cohesion: 0.8,
                coupling: 0.2,
                description: None,
            }],
            features: Vec::new(),
        };

        export_html(&[doc], &[card], &graph, &config)?;

        assert!(dir.join("index.html").exists(), "index.html 应该存在");
        assert!(dir.join("style.css").exists(), "style.css 应该存在");
        assert!(dir.join("wiki").join("测试模块.html").exists(), "wiki 页面应该存在");
        assert!(dir.join("cards").join("核心模块.html").exists(), "card 页面应该存在");
        assert!(dir.join("assets").join("module-deps.html").exists(), "Mermaid 页面应该存在");

        let index = std::fs::read_to_string(dir.join("index.html"))?;
        assert!(index.contains("测试模块"), "目录页应该包含文档标题");

        let card_html = std::fs::read_to_string(dir.join("cards").join("核心模块.html"))?;
        assert!(card_html.contains("人工修改待同步"), "卡片 HTML 应包含人工修改待同步节");
        assert!(card_html.contains("手动改"), "卡片 HTML 应包含记录内容");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
