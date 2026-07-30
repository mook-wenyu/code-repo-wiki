use tree_sitter::Parser;

use crate::search::ast::get_language;

/// AST 感知的代码分块
///
/// 按 tree-sitter 语法结构做细粒度切片，
/// 每个块携带 AST 路径和符号引用。
#[derive(Debug, Clone)]
pub struct AstChunk {
    /// 源码文本
    pub source: String,
    /// AST 路径（如 ["function", "body", "if_statement"]）
    pub path: Vec<String>,
    /// 关联的符号名
    pub symbol: Option<String>,
    /// 源码起止字节偏移
    pub span: (usize, usize),
    /// 嵌套子块
    pub children: Vec<AstChunk>,
}

/// 按 AST 结构对源码进行分块
///
/// 使用 tree-sitter parser 解析源码后，按函数/类等顶层结构切分。
pub fn chunk_by_ast(source: &str, language: &str) -> Vec<AstChunk> {
    if source.is_empty() { return Vec::new(); }
    let mut chunks = Vec::new();
    let lang = match get_language(language) {
        Ok(l) => l,
        Err(_) => {
            chunks.push(AstChunk {
                source: source.to_string(),
                path: vec!["root".into()],
                symbol: None,
                span: (0, source.len()),
                children: Vec::new(),
            });
            return chunks;
        }
    };

    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return chunks;
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return chunks,
    };
    let root = tree.root_node();

    // 对顶层定义节点遍历
    let def_types = match language {
        "rust" => &["function_item", "struct_item", "trait_item", "enum_item", "impl_item", "type_item", "const_item", "mod_item"][..],
        "python" => &["function_definition", "class_definition"][..],
        "javascript" | "typescript" => &["function_declaration", "class_declaration", "method_definition", "interface_declaration", "enum_declaration"][..],
        "go" => &["function_declaration", "type_declaration", "type_spec"][..],
        "csharp" => &["method_declaration", "class_declaration", "struct_declaration", "interface_declaration", "enum_declaration"][..],
        _ => &[][..],
    };

    let bytes = source.as_bytes();
    for i in 0..root.child_count() {
        let node = match root.child(i) {
            Some(n) => n,
            None => continue,
        };
        if !def_types.contains(&node.kind()) {
            // 非定义节点跳过
            continue;
        }
        let name = node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(|s| s.to_string());
        let text = node.utf8_text(bytes).unwrap_or("").to_string();
        let chunk = AstChunk {
            source: text,
            path: vec![node.kind().to_string()],
            symbol: name,
            span: (node.start_byte(), node.end_byte()),
            children: collect_nested(&node, source),
        };
        chunks.push(chunk);
    }

    if chunks.is_empty() {
        // 没有识别到任何定义，整个文件作为一个块
        chunks.push(AstChunk {
            source: source.to_string(),
            path: vec!["root".into()],
            symbol: None,
            span: (0, source.len()),
            children: Vec::new(),
        });
    }

    chunks
}

/// 收集节点内部的嵌套子结构
fn collect_nested(node: &tree_sitter::Node, source: &str) -> Vec<AstChunk> {
    let mut children = Vec::new();
    let bytes = source.as_bytes();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            let kind = child.kind();
            if (kind == "block" || kind == "body" || kind == "declaration_list") && let Ok(text) = child.utf8_text(bytes) {
                children.push(AstChunk {
                    source: text.to_string(),
                    path: vec!["block".into()],
                    symbol: None,
                    span: (child.start_byte(), child.end_byte()),
                    children: Vec::new(),
                });
            }
        }
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_rust_functions() {
        let source = "fn foo() {}\nfn bar() {}";
        let chunks = chunk_by_ast(source, "rust");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].symbol.as_deref(), Some("foo"));
    }

    #[test]
    fn test_chunk_empty_source() {
        let chunks = chunk_by_ast("", "rust");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_unsupported_lang() {
        let source = "fn foo() {}";
        let chunks = chunk_by_ast(source, "unknown");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].source.contains("foo"));
    }

    #[test]
    fn test_chunk_python() {
        let source = "def hello(): pass\nclass World: pass";
        let chunks = chunk_by_ast(source, "python");
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunk_no_definitions() {
        let source = "let x = 1;";
        let chunks = chunk_by_ast(source, "rust");
        assert!(!chunks.is_empty());
    }
}
