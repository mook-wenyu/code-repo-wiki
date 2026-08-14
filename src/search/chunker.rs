//! 结构感知分块器——把源码切成「顶层定义」粒度块
//!
//! ## 设计决策（v0.7.2）
//!
//! - **块粒度 = tree-sitter 顶层定义节点**（函数/结构体/类/trait/impl/enum 等），
//!   嵌套方法并入父块不拆（防向量爆炸：一个 impl 的全部方法共享一个块，
//!   语义上下文完整且不产生几百个近重复向量）。
//! - **块文本 = 模块路径 + 作用域链 + 文件行 + 可见性 + 签名(≤160字符) +
//!   doc 首段 + body 源码(截断)**——对应 T2「作用域前缀 + 签名 + body」，
//!   避免裸函数体向量退化为词袋。
//! - 不支持的解析语言走 `FileChunker`（整文件一块兜底），不丢索引覆盖。

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::model::NodeKind;
use crate::search::ast::get_language;
use crate::search::block::{Block, EntityRef, build_embed_text};

/// 分块器抽象：按语言选择 AstChunker（结构感知）或 FileChunker（整文件）
///
/// `&mut self`：tree-sitter Parser::parse 需要可变借用（单文件单线程调用，
/// 分块器每文件新建，不跨线程共享）
pub trait Chunker {
    fn chunk(&mut self, source: &str, file_path: &str, language: &str) -> Result<Vec<Block>>;
}

/// 按语言返回分块器：tree-sitter 支持的语言走 AstChunker，其余 FileChunker
pub fn chunker_for(language: &str) -> Box<dyn Chunker> {
    match get_language(language) {
        Ok(_) => Box::new(AstChunker::new(language).expect("语言已确认支持，构造不应失败")),
        Err(_) => Box::new(FileChunker),
    }
}

/// tree-sitter 结构感知分块器（绑定单一语言 parser）
pub struct AstChunker {
    parser: Parser,
}

impl AstChunker {
    pub fn new(language: &str) -> Result<Self> {
        let lang = get_language(language)?;
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .context("设置 tree-sitter 语言失败")?;
        Ok(Self { parser })
    }
}

impl Chunker for AstChunker {
    fn chunk(&mut self, source: &str, file_path: &str, language: &str) -> Result<Vec<Block>> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter 解析源码失败: {file_path}"))?;
        let defs = top_level_defs(language);
        let module_path = derive_module_path(file_path);
        let root = tree.root_node();
        let mut blocks = Vec::new();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            // 只切顶层定义节点；嵌套（impl/class 内方法）作为父块 body 保留
            if defs.contains(&child.kind())
                && let Some(block) =
                    block_from_node(&child, source, file_path, language, &module_path)
            {
                blocks.push(block);
            }
        }
        Ok(blocks)
    }
}

/// 整文件兜底分块器（不支持的语言）：整个文件一块
pub struct FileChunker;

impl Chunker for FileChunker {
    fn chunk(&mut self, source: &str, file_path: &str, language: &str) -> Result<Vec<Block>> {
        let module_path = derive_module_path(file_path);
        let line_count = source.lines().count().max(1);
        // 无结构信息：name 取文件名 stem，kind 用 Function 占位（不参与
        // 语义检索的精细匹配，仅保证整文件可被索引与检索）
        let name = file_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(file_path)
            .to_string();
        let text = build_embed_text(
            &module_path,
            &name,
            NodeKind::File,
            &[],
            file_path,
            1,
            line_count,
            None,
            "",
            None,
            source,
        );
        Ok(vec![Block {
            id: format!("{file_path}#1-{line_count}"),
            file_path: file_path.to_string(),
            language: language.to_string(),
            module_path,
            kind: NodeKind::File,
            name,
            line_range: (1, line_count),
            signature: String::new(),
            visibility: None,
            doc_comment: None,
            text,
            entity: EntityRef {
                name: file_path.to_string(),
                file_path: file_path.to_string(),
                line_range: (1, line_count),
            },
        }])
    }
}

/// 各语言顶层定义节点类型（与 parser 实体 kind 对齐，见 ingest/parser/*）
fn top_level_defs(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &[
            "function_item",
            "struct_item",
            "trait_item",
            "enum_item",
            "type_item",
            "const_item",
            "static_item",
            "impl_item",
        ][..],
        "python" => &["function_definition", "class_definition"][..],
        "javascript" | "typescript" | "tsx" => &[
            "function_declaration",
            "class_declaration",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ][..],
        "go" => &[
            "function_declaration",
            "type_declaration",
            "type_spec",
            "const_declaration",
            "var_declaration",
        ][..],
        "csharp" => &[
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_declaration",
            "record_declaration",
            "delegate_declaration",
        ][..],
        // Java（P2 补全唯一缺的受支持语言）：节点名经 tree-sitter-java
        // node-types.json 核证（class/interface/enum/record/annotation_type
        // 为顶层声明；method 嵌套在类内，含入列表不匹配根级子节点，无害）
        "java" => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "method_declaration",
            "annotation_type_declaration",
        ][..],
        _ => &[][..],
    }
}

/// 从顶层定义节点构造块（返回 None = 无法提取名称/kind，跳过该块）
fn block_from_node(
    node: &Node,
    source: &str,
    file_path: &str,
    language: &str,
    module_path: &[String],
) -> Option<Block> {
    let bytes = source.as_bytes();
    let start = node.start_position().row + 1;
    let end = node.end_position().row + 1;
    let (name, kind) = extract_name_kind(node, bytes)?;
    let signature = header_text(node, source);
    let body = body_text(node, source);
    let visibility = extract_visibility(node, source);
    let doc = extract_doc(source, start);
    let text = build_embed_text(
        module_path,
        &name,
        kind.clone(), // 块结构同时持有 kind（NodeKind 非 Copy，克隆一份给文本）
        &[],          // 顶层块作用域链为空（嵌套并入父块，无独立块）
        file_path,
        start,
        end,
        visibility.as_deref(),
        &signature,
        doc.as_deref(),
        &body,
    );
    Some(Block {
        id: format!("{file_path}#{start}-{end}"),
        file_path: file_path.to_string(),
        language: language.to_string(),
        module_path: module_path.to_vec(),
        kind,
        name: name.clone(),
        line_range: (start, end),
        signature,
        visibility,
        doc_comment: doc,
        text,
        entity: EntityRef {
            name,
            file_path: file_path.to_string(),
            line_range: (start, end),
        },
    })
}

/// 提取块名称与 NodeKind；无 name 字段（如 Go const 组）返回 None 由调用方跳过
fn extract_name_kind(node: &Node, bytes: &[u8]) -> Option<(String, NodeKind)> {
    let kind = kind_from_ts(node.kind())?;
    // Rust impl 无 name 字段，取 type（目标类型）作为块名（与 parser 一致）
    let name = if node.kind() == "impl_item" {
        node.child_by_field_name("type")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(|s| s.to_string())
    } else {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(|s| s.to_string())
    };
    name.map(|n| (n, kind))
}

/// tree-sitter 节点 kind → NodeKind
fn kind_from_ts(kind: &str) -> Option<NodeKind> {
    match kind {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "method_declaration"
        | "method_definition" => Some(NodeKind::Function),
        "struct_item" | "struct_declaration" => Some(NodeKind::Struct),
        "trait_item" => Some(NodeKind::Trait),
        "enum_item" | "enum_declaration" => Some(NodeKind::Enum),
        "type_item" | "type_spec" | "type_alias_declaration" | "type_declaration" => {
            Some(NodeKind::Type)
        }
        "const_item" | "const_declaration" | "static_item" => Some(NodeKind::Constant),
        "impl_item" => Some(NodeKind::Impl),
        "class_definition" | "class_declaration" => Some(NodeKind::Class),
        // Java annotation_type_declaration 语义即「注解接口」，归 Interface
        "interface_declaration" | "annotation_type_declaration" => Some(NodeKind::Interface),
        "var_declaration" => Some(NodeKind::Variable),
        "record_declaration" => Some(NodeKind::Struct),
        "delegate_declaration" => Some(NodeKind::Function),
        _ => None,
    }
}

/// 声明头：body 字段开始前的节点文本（含签名；Python 的 def/class 头、
/// Rust/Go/C# 的 brace 前部分）。无 body 字段的节点（unit struct/type alias）
/// 取节点文本首行。
fn header_text(node: &Node, source: &str) -> String {
    if let Some(body) = node.child_by_field_name("body") {
        // 从节点起点切片（doc 注释在节点外，混入会污染签名/可见性判定）
        let start = node.start_byte();
        let end = body.start_byte();
        source[start..end].trim().to_string()
    } else {
        node.utf8_text(source.as_bytes())
            .map(|t| t.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default()
    }
}

/// body 源码：body 字段文本；无 body 字段时取整个节点文本
fn body_text(node: &Node, source: &str) -> String {
    if let Some(body) = node.child_by_field_name("body") {
        body.utf8_text(source.as_bytes()).unwrap_or("").to_string()
    } else {
        node.utf8_text(source.as_bytes()).unwrap_or("").to_string()
    }
}

/// 可见性：优先 tree-sitter 字段（Rust visibility_modifier），否则检查
/// 声明头首 token 是否 pub/public/export
fn extract_visibility(node: &Node, source: &str) -> Option<String> {
    if let Some(s) = node
        .child_by_field_name("visibility_modifier")
        .and_then(|v| v.utf8_text(source.as_bytes()).ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(s.to_string());
    }
    let header = header_text(node, source);
    for kw in ["pub ", "public ", "export "] {
        if header.starts_with(kw) {
            return Some(kw.trim().to_string());
        }
    }
    None
}

/// 提取块上方的 doc 注释**首段**（从块定义行向上扫描连续注释行，
/// 遇空注释行「///」「#」即段落边界；取离块最近的首段）
///
/// 注意：Rust doc 注释 `/// 首段\n///\n/// 第二段` 中「首段」是距块最远的
/// 那段（阅读顺序自上而下），因此先收集**全部**连续注释行再截到首个
/// 空注释行——只向上收集到首个分隔符会拿到末段（离块最近），语义反了。
fn extract_doc(source: &str, start_line: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    if start_line < 2 || lines.is_empty() {
        // 块在首行则上方无注释；无行可扫描直接 None
        return None;
    }
    // 从块定义行的上一行开始（0-based：start_line - 2）
    let mut i = start_line.saturating_sub(2);
    let mut collected: Vec<&str> = Vec::new();
    // 跳过注释与定义之间的空行（允许注释与定义间有空白）
    while lines[i].trim().is_empty() && i > 0 {
        i -= 1;
    }
    // 向上收集连续注释行（含段落分隔的空注释行，之后统一截首段）
    while is_comment_line(lines[i]) {
        collected.push(lines[i]);
        if i == 0 {
            break;
        }
        i -= 1;
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    // 首段 = 到第一个空注释行（/// 或 # 单独成行）为止
    let first_paragraph: Vec<&str> = collected
        .iter()
        .take_while(|l| !is_empty_comment_line(l))
        .copied()
        .collect();
    if first_paragraph.is_empty() {
        return None;
    }
    Some(
        first_paragraph
            .iter()
            .map(|l| strip_comment_markers(l))
            .collect::<Vec<String>>()
            .join("\n"),
    )
}

fn is_comment_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("///")
        || t.starts_with("//!")
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with('#')
        || t.starts_with("\"\"\"")
        || t.starts_with('\'')
}

fn is_empty_comment_line(line: &str) -> bool {
    let t = line.trim();
    matches!(
        t,
        "///" | "//!" | "//" | "#" | "*" | "/*" | "/**" | "\"\"\"" | "'''"
    )
}

fn strip_comment_markers(line: &str) -> String {
    let t = line.trim();
    t.strip_prefix("///")
        .or_else(|| t.strip_prefix("//!"))
        .or_else(|| t.strip_prefix("//"))
        .or_else(|| t.strip_prefix('#'))
        .unwrap_or(t)
        .trim()
        .to_string()
}

/// 模块路径派生：文件路径的目录段 + 文件名 stem（与 graph.rs 的
/// dir_segments + stem 同规则，保证块与图 CodeNode.module_path 对齐）
fn derive_module_path(file_path: &str) -> Vec<String> {
    let mut parts: Vec<String> = file_path
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if let Some(last) = parts.last_mut() {
        // 去扩展名作为模块名（如 user.rs → user；main.rs → main）
        if let Some(dot) = last.rfind('.') {
            last.truncate(dot);
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_rust(source: &str) -> Vec<Block> {
        let mut chunker = AstChunker::new("rust").unwrap();
        chunker.chunk(source, "src/app.rs", "rust").unwrap()
    }

    #[test]
    fn test_rust_top_level_blocks_and_names() {
        let source = r#"pub struct Point { x: i32 }
impl Point {
    pub fn new(x: i32) -> Self { Self { x } }
    fn get(&self) -> i32 { self.x }
}
pub fn area(p: &Point) -> i32 { p.x * p.x }
enum Shape { Circle }
"#;
        let blocks = chunk_rust(source);
        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["Point", "Point", "area", "Shape"]);
        // impl 块：方法并入父块（new/get 不产生独立块）
        let impl_block = blocks.iter().find(|b| b.kind == NodeKind::Impl).unwrap();
        assert!(impl_block.text.contains("fn get"), "impl 块 body 应含方法");
        assert_eq!(impl_block.line_range.0, 2, "impl 起始行");
    }

    #[test]
    fn test_rust_line_ranges_and_module_path() {
        let source = "fn a() {}\n\nfn b() {}\n";
        let blocks = chunk_rust(source);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_range, (1, 1));
        assert_eq!(blocks[1].line_range, (3, 3));
        assert_eq!(
            blocks[0].module_path,
            vec!["src".to_string(), "app".to_string()]
        );
        assert!(blocks[0].text.contains("src::app::a Function"));
    }

    #[test]
    fn test_rust_visibility_and_doc() {
        let source = "/// 计算面积\n///\n/// 第二段说明\npub fn area(w: i32) -> i32 { w * w }\n";
        let blocks = chunk_rust(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].visibility.as_deref(), Some("pub"));
        let doc = blocks[0].doc_comment.as_deref().unwrap_or("");
        assert!(doc.contains("计算面积"), "doc 首段应提取: {doc}");
        assert!(
            !doc.contains("第二段说明"),
            "空注释行分隔的段落不应混入: {doc}"
        );
        assert!(blocks[0].text.contains("pub fn area(w: i32) -> i32"));
    }

    #[test]
    fn test_python_class_and_function_blocks() {
        let source = "class Foo:\n    def method(self):\n        pass\n\ndef top():\n    pass\n";
        let mut chunker = AstChunker::new("python").unwrap();
        let blocks = chunker.chunk(source, "src/app.py", "python").unwrap();
        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        // 类内 method 并入类块，不产生独立块
        assert_eq!(names, vec!["Foo", "top"]);
        let class_block = blocks[0].clone();
        assert_eq!(class_block.kind, NodeKind::Class);
        assert!(
            class_block.text.contains("def method"),
            "类块 body 应含方法"
        );
    }

    #[test]
    fn test_file_chunker_fallback() {
        let mut chunker = FileChunker;
        let blocks = chunker
            .chunk("some plain text\n", "notes/readme.txt", "text")
            .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].line_range, (1, 1));
        assert_eq!(blocks[0].language, "text");
        assert!(blocks[0].text.contains("some plain text"));
    }

    /// P2：Java 结构感知分块——类/接口/枚举/record 各成块，类内方法与
    /// 字段并入类块不拆（与 Rust impl 同语义：防向量爆炸）
    #[test]
    fn test_java_top_level_blocks() {
        let source = r#"public class Point {
    private int x;
    public int getX() { return x; }
}
interface Shape {}
enum Color { RED, GREEN }
record Pair(int a, int b) {}
"#;
        let mut chunker = AstChunker::new("java").unwrap();
        let blocks = chunker.chunk(source, "src/Point.java", "java").unwrap();
        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        // 类内方法 getX 并入类块，不产生独立块
        assert_eq!(names, vec!["Point", "Shape", "Color", "Pair"]);
        let class_block = blocks.iter().find(|b| b.kind == NodeKind::Class).unwrap();
        assert!(
            class_block.text.contains("getX"),
            "类块 body 应含方法: {}",
            class_block.text
        );
        assert_eq!(class_block.language, "java");
        let shape = blocks.iter().find(|b| b.name == "Shape").unwrap();
        assert_eq!(shape.kind, NodeKind::Interface);
    }

    #[test]
    fn test_derive_module_path() {
        assert_eq!(
            derive_module_path("src/auth/user.rs"),
            vec!["src", "auth", "user"]
        );
        assert_eq!(derive_module_path("main.rs"), vec!["main"]);
        assert_eq!(derive_module_path("a\\b\\c.go"), vec!["a", "b", "c"]);
    }
}
