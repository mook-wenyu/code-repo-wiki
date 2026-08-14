use std::path::Path;

use anyhow::{Context, Result};
use pulldown_cmark::{Options, Parser, html};

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, WikiDocument};
use crate::output::{ExportModuleSnapshot, card_page_path, wiki_page_html_path};

/// 将整个 Wiki 输出导出为 HTML 页面集合
///
/// 输出结构（与 markdown 产物同构，命名规则一致仅扩展名不同）：
///   {output.dir}/wiki/{lang}/{stem}.html — 每个文档的 HTML 页面（按语言分目录）
///   {output.dir}/index.html             — 目录页
///   {output.dir}/style.css              — 基本样式
///   {output.dir}/cards/{lang}/{stem}.html — Knowledge Card 页面（随关联文档语言落盘）
///   {output.dir}/assets/module-deps.html — Mermaid 模块依赖图
///
/// `modules` 为快照形态的模块摘要（export_modules 产出），不依赖完整图，
/// 使 export --skip-generate 可直接从导出快照构造调用。
pub fn export_html(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    modules: &[ExportModuleSnapshot],
    config: &WikiConfig,
) -> Result<()> {
    let output_dir = config.output_dir();
    let wiki_dir = output_dir.join("wiki");
    let cards_dir = output_dir.join("cards");
    let assets_dir = output_dir.join("assets");

    std::fs::create_dir_all(&wiki_dir)
        .with_context(|| format!("创建 wiki 目录失败: {}", wiki_dir.display()))?;
    std::fs::create_dir_all(&cards_dir)
        .with_context(|| format!("创建 cards 目录失败: {}", cards_dir.display()))?;
    std::fs::create_dir_all(&assets_dir)
        .with_context(|| format!("创建 assets 目录失败: {}", assets_dir.display()))?;

    // 每个 WikiDocument → wiki/{lang}/{stem}.html（命名与 markdown 同构；
    // 链接重写：正文中的内部 .md 相对链接在转换前改为 .html）
    for doc in documents {
        let body = md_to_html(&rewrite_md_links_to_html(&doc.content));
        let html = wrap_html(&doc.title, &body, "../../style.css");
        let path = wiki_page_html_path(output_dir, doc);
        write_html_file(&path, &html)?;
    }

    // 生成 index.html（目录页）：按模块分组（与 _toc.md 的 index 优先导航一致）。
    // 链接指向 wiki/{doc.language}/{stem}.html（与页面落盘路径同一规则）。
    let mut module_groups: std::collections::BTreeMap<String, Vec<&WikiDocument>> =
        Default::default();
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
            // 注入向量：wiki_file_name 清洗仅替换 /\:，引号（LLM 标题常见）保留进
            // 文件名，进而出现在 href 值里 → 可提前闭合属性注入任意 HTML。
            // 故 href 输出前同样经 escape_html（引号转 &quot;，属性注入被中和）。
            toc_items.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>\n",
                escape_html(&wiki_html_link(output_dir, doc)),
                escape_html(&doc.title)
            ));
        }
        toc_items.push_str("</ul>\n");
    }
    toc_items.push_str("<h2>模块</h2>\n");
    for (module, docs) in &module_groups {
        toc_items.push_str(&format!("<h3>{}</h3>\n<ul>\n", escape_html(module)));
        for doc in docs {
            toc_items.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>\n",
                escape_html(&wiki_html_link(output_dir, doc)),
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
    if !modules.is_empty() {
        let mut mermaid_lines = vec!["graph TD".to_string()];
        // U05/D9：节点名规范化与依赖边共用同一规则（非字母数字 → _），
        // 保证节点声明与边引用同名（此前只有节点行零边——快照无依赖字段）
        let node_id = |name: &str| name.replace(|c: char| !c.is_alphanumeric(), "_");
        for module in modules {
            if !module.files.is_empty() {
                // ponytail: 简化为模块级节点，不展开到每个实体
                mermaid_lines.push(format!(
                    "    {}[\"{}\"]",
                    node_id(&module.name),
                    escape_html(&module.name)
                ));
            }
        }
        // 依赖边：模块 → 依赖模块（BTreeSet 字典序，输出确定性）
        for module in modules {
            for dep in &module.dependencies {
                mermaid_lines.push(format!(
                    "    {} --> {}",
                    node_id(&module.name),
                    node_id(dep)
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

    // KnowledgeCard → cards/{lang}/{stem}.html（精确关联：卡片随其模块文档
    // 的语言落盘，与 render_all 的写盘规则一致；无匹配文档的卡片不写盘）
    for doc in documents {
        let doc_module = doc.module_path.join("::");
        for card in cards {
            if card.module_name != doc_module {
                continue;
            }
            let html = wrap_html(
                &card.module_name,
                &render_card_body(card),
                "../../style.css",
            );
            let path =
                card_page_path(output_dir, &doc.language, &card.module_name).with_extension("html");
            write_html_file(&path, &html)?;
        }
    }

    Ok(())
}

/// 渲染卡片正文（标题、摘要、实体、依赖、模式、人工修改待同步）
fn render_card_body(card: &KnowledgeCard) -> String {
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
                escape_html(entity.doc.as_deref().unwrap_or(""))
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

    body
}

/// 文档的 HTML 页面相对链接（相对 output_dir，正斜杠分隔，供 index.html 使用）
fn wiki_html_link(output_dir: &Path, doc: &WikiDocument) -> String {
    wiki_page_html_path(output_dir, doc)
        .strip_prefix(output_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// 将 markdown 文本中的内部 .md 相对链接重写为 .html（HTML 导出用，纯函数）
///
/// 只重写 `](target)` 形态的链接且 target 含 ".md"（相对链接，如
/// `wiki/zh/a.md`、`a.md#锚点`——锚点保留）；外部链接（含 `://`，如
/// `https://x.com/a.md`）与源码定位链接（`[源码:path]`，无 .md 后缀）不重写。
pub fn rewrite_md_links_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut rest = md;
    while let Some(start) = rest.find("](") {
        out.push_str(&rest[..start + 2]);
        let after = &rest[start + 2..];
        let end = after.find(')').unwrap_or(after.len());
        let target = &after[..end];
        if !target.contains("://") {
            if let Some(md_end) = target.find(".md") {
                out.push_str(&target[..md_end]);
                out.push_str(".html");
                out.push_str(&target[md_end + 3..]);
            } else {
                out.push_str(target);
            }
        } else {
            out.push_str(target);
        }
        // 只移动搜索起点，剩余段由循环后的 out.push_str(rest) 统一收尾——
        // 这里若再 push 剩余段会与最终收尾重复累积。
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Mermaid 渲染脚本（U05/D9）：CDN 加载 mermaid.js 后初始化；
/// CDN 不可达（离线/网络隔离）时 onerror 降级——.mermaid div 的源码
/// 转为 pre 文本展示，图不渲染但信息不丢失（与生成层 degrade 语义一致）。
const MERMAID_SCRIPT: &str = r#"<script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"
 onerror="window.__repoWikiMermaidFallback&&window.__repoWikiMermaidFallback()"></script>
<script>
function __repoWikiMermaidFallback() {
  document.querySelectorAll('.mermaid').forEach(function (el) {
    var pre = document.createElement('pre');
    pre.textContent = el.textContent;
    el.replaceWith(pre);
  });
}
window.__repoWikiMermaidFallback = __repoWikiMermaidFallback;
if (window.mermaid) { mermaid.initialize({ startOnLoad: true }); }
</script>"#;

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
{MERMAID_SCRIPT}
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
///
/// U05/D9：mermaid 围栏块不输出为 <pre><code> 源码，改为
/// `<div class="mermaid">` 容器（由 wrap_html 注入的 mermaid.js 渲染）；
/// 其余事件逐条委托 pulldown-cmark 默认渲染。
fn md_to_html(markdown: &str) -> String {
    use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};

    // v52 T08a：pulldown-cmark 0.12 无 ENABLE_RAW_HTML 开关（HTML 解析为默认行为），
    // 且 push_html 对 Event::Html/InlineHtml 原样透传——XSS 防护在事件级转义
    //（下方 Event::Html/InlineHtml 分支），此处保持全特性。
    let options = Options::all();
    let parser = Parser::new_ext(markdown, options);
    let mut html_output = String::new();
    let mut mermaid_buf: Option<String> = None;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.eq_ignore_ascii_case("mermaid") =>
            {
                mermaid_buf = Some(String::new());
            }
            Event::Text(text) if mermaid_buf.is_some() => {
                mermaid_buf.as_mut().unwrap().push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) if mermaid_buf.is_some() => {
                let buf = mermaid_buf.take().unwrap_or_default();
                html_output.push_str(&format!(
                    "<div class=\"mermaid\">\n{}\n</div>\n",
                    escape_html(&buf)
                ));
            }
            // v52 T08a：raw HTML 事件原样透传（pulldown-cmark 0.12 html.rs:122-123
            // 直接 write 不转义）——源码 doc 注释/LLM 输出中的 <script> 等会直接
            // 注入导出页面。显式转义为实体，阻断 XSS。
            Event::Html(text) | Event::InlineHtml(text) => {
                html_output.push_str(&escape_html(&text));
            }
            other => {
                // 逐事件委托默认渲染（含 mermaid 外的普通围栏）
                let mut tmp = String::new();
                html::push_html(&mut tmp, std::iter::once(other));
                html_output.push_str(&tmp);
            }
        }
    }
    html_output
}

/// 写入 HTML 文件到磁盘，自动创建父目录
fn write_html_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("写入文件失败: {}", path.display()))
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
    use crate::config::schema::WikiConfig;
    use crate::model::{EntitySummary, KnowledgeCard, WikiDocument};

    fn test_config() -> WikiConfig {
        WikiConfig {
            output_dir: Some(std::path::PathBuf::from(".code-repo-wiki")),
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
        // title 必须转义（页面本身含 mermaid 脚本标签，断言不能再用
        // 全页 contains("<script>")——改为精确匹配 title 位置的转义形态）
        assert!(
            html.contains("<title>&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;</title>"),
            "title 中的 HTML 应被转义, 实际: {html}"
        );
        assert!(
            html.contains("mermaid.min.js"),
            "页面应引入 mermaid.js（U05/D9）"
        );
        assert!(
            html.contains("__repoWikiMermaidFallback"),
            "应含离线降级脚本（U05/D9）"
        );
    }

    /// U05/D9：mermaid 围栏渲染为 div.mermaid 容器（内容转义），
    /// 普通代码围栏保持 <pre> 原样
    #[test]
    fn test_md_to_html_renders_mermaid_container() {
        let result = md_to_html("```mermaid\nflowchart LR\nA[Start] --> B[End]\n```\n");
        assert!(
            result.contains("<div class=\"mermaid\">"),
            "mermaid 应渲染为 div 容器: {result}"
        );
        assert!(result.contains("flowchart LR"), "内容应保留");
        assert!(
            !result.contains("<pre>"),
            "mermaid 不应输出为 pre 代码块: {result}"
        );
        assert!(
            result.contains("A[Start] --&gt; B[End]"),
            "内容应 HTML 转义: {result}"
        );
    }

    #[test]
    fn test_md_to_html_plain_code_block_unchanged() {
        let result = md_to_html("```rust\nfn main() {}\n```\n");
        assert!(result.contains("<pre>"), "普通代码块应保持 pre: {result}");
        assert!(
            !result.contains("mermaid"),
            "普通代码块不应触发 mermaid 渲染: {result}"
        );
    }

    #[test]
    fn test_md_to_html_renders_paragraph() {
        let result = md_to_html("Hello **world**");
        assert!(result.contains("<p>"), "应该生成 p 标签");
        assert!(result.contains("<strong>"), "应该生成 strong 标签");
        assert!(result.contains("world"), "应该保留文本内容");
    }

    /// v52 T08a：raw HTML 注入防护——<script> 等原始 HTML 不再透传
    #[test]
    fn test_md_to_html_escapes_raw_html() {
        let html = md_to_html("<script>alert(1)</script>");
        assert!(!html.contains("<script>"), "原始 <script> 不应透传: {html}");
        assert!(html.contains("&lt;script&gt;"), "应转义为实体: {html}");
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
    fn test_escape_html_escapes_special_chars() {
        assert_eq!(escape_html("<>&'\""), "&lt;&gt;&amp;&#39;&quot;");
        assert_eq!(escape_html("plain text"), "plain text");
    }

    /// 链接重写纯函数：内部 .md 相对链接 → .html；外部链接与源码定位不重写
    #[test]
    fn test_rewrite_md_links_to_html() {
        let md = "见 [B](wiki/zh/b.md) 与 [C](a.md#锚点)，外部 [D](https://x.com/a.md)，源码 [E](src/lib.rs:12)";
        let rewritten = rewrite_md_links_to_html(md);
        assert!(
            rewritten.contains("](wiki/zh/b.html)"),
            "wiki/zh/b.md 应重写为 .html, 实际: {rewritten}"
        );
        assert!(
            rewritten.contains("](a.html#锚点)"),
            "带锚点的 .md 链接应保留锚点, 实际: {rewritten}"
        );
        assert!(
            rewritten.contains("](https://x.com/a.md)"),
            "外部链接不应重写, 实际: {rewritten}"
        );
        assert!(
            rewritten.contains("](src/lib.rs:12)"),
            "源码定位链接不应重写, 实际: {rewritten}"
        );
        // 内部 .md 链接必须全部重写（外部/源码定位天然含 .md，不在此断言内）
        assert!(
            !rewritten.contains("](wiki/zh/b.md)") && !rewritten.contains("](a.md"),
            "内部 .md 链接应全部重写, 实际: {rewritten}"
        );
    }

    #[test]
    fn test_export_html_creates_files() -> Result<()> {
        let dir = std::env::temp_dir().join("code-repo-wiki-test-html-export");
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = test_config();
        config.output_dir = Some((dir).to_path_buf());

        // 文档与卡片同模块（精确关联的前提）：module_path ["核心","模块"] 与
        // module_name "核心::模块" 精确相等，卡片随文档语言落盘 cards/zh/
        let doc = WikiDocument {
            title: "核心模块".to_string(),
            kind: crate::model::DocumentKind::WikiPage,
            content: "# 测试\n\nHello world.".to_string(),
            language: "zh".to_string(),
            module_path: vec!["核心".to_string(), "模块".to_string()],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01".to_string(),
            based_on_commit: None,
            fingerprint: None,
        };

        let card = KnowledgeCard {
            module_name: "核心::模块".to_string(),
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
            design_rationale: None,
            pending_manual_edits: vec![
                "人工修改待同步: wiki/zh/核心_模块.md 内容摘要: 手动改".into(),
            ],
            features: Vec::new(),
        };

        // 快照形态的模块摘要（含文件列表才会出现在 Mermaid 图中）
        let modules = vec![ExportModuleSnapshot {
            name: "核心::模块".to_string(),
            files: vec!["src/core/mod.rs".to_string()],
            cohesion: 0.8,
            coupling: 0.2,
            features: vec![],
            dependencies: vec![],
        }];

        export_html(&[doc], &[card], &modules, &config)?;

        assert!(dir.join("index.html").exists(), "index.html 应该存在");
        assert!(dir.join("style.css").exists(), "style.css 应该存在");
        assert!(
            dir.join("wiki").join("zh").join("核心_模块.html").exists(),
            "wiki 页面应写到 wiki/zh/ 语言目录（与 markdown 命名同构）"
        );
        assert!(
            dir.join("cards").join("zh").join("核心_模块.html").exists(),
            "card 页面应随文档语言写到 cards/zh/"
        );
        assert!(
            dir.join("assets").join("module-deps.html").exists(),
            "Mermaid 页面应该存在"
        );

        let index = std::fs::read_to_string(dir.join("index.html"))?;
        assert!(index.contains("核心模块"), "目录页应该包含文档标题");
        assert!(
            index.contains("wiki/zh/核心_模块.html"),
            "目录页链接应指向语言目录下的 .html, 实际: {index}"
        );

        let card_html =
            std::fs::read_to_string(dir.join("cards").join("zh").join("核心_模块.html"))?;
        assert!(
            card_html.contains("人工修改待同步"),
            "卡片 HTML 应包含人工修改待同步节"
        );
        assert!(card_html.contains("手动改"), "卡片 HTML 应包含记录内容");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Phase 13 回归防线（12.x 审查 MEDIUM）：全局文档 href 由 wiki_file_name
    /// 产出（仅替换 /\:，引号保留进文件名）→ 引号可提前闭合 href="..." 属性
    /// 注入任意 HTML。escape_html 必须将引号实体化：删除 index.html 生成处
    /// （:71/:83）的 escape_html 调用或去掉引号转义，本测试即失败。
    /// 纯内存断言——避免 Windows 上引号文件名写盘失败（恶意标题不落盘）。
    #[test]
    fn test_export_html_escapes_href_quote() {
        let doc = WikiDocument {
            title: r#"x" onmouseover="alert(1)"#.to_string(),
            kind: crate::model::DocumentKind::WikiPage,
            content: "# 测试\n\nHello world.".to_string(),
            language: "zh".to_string(),
            module_path: vec![], // 全局文档 → 文件名来自 title
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01".to_string(),
            based_on_commit: None,
            fingerprint: None,
        };
        let href = wiki_html_link(std::path::Path::new("out"), &doc);
        // 前提：wiki_file_name 保留引号，载荷确实进入 href（否则测试空转）
        assert!(href.contains('"'), "wiki_file_name 应保留引号: {href}");
        let escaped = escape_html(&href);
        assert!(escaped.contains("&quot;"), "href 应转义引号: {escaped}");
        assert!(
            !escaped.contains('"'),
            "href 不应残留裸引号（属性注入向量）: {escaped}"
        );
        assert!(
            !escaped.contains(r#"onmouseover=""#),
            "onmouseover 不得以裸引号赋值: {escaped}"
        );
    }
}
