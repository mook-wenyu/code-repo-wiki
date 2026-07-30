use std::collections::HashMap;
use tree_sitter::{Language, Parser};
use anyhow::{Result, anyhow};

/// AST 查询器：统一的 tree-sitter 查询接口
///
/// 封装多种语言的 tree-sitter parser，提供符号定位和引用查询。
pub struct AstQuery {
    language: String,
    parser: Parser,
}

/// 查询匹配结果
#[derive(Debug, Clone)]
pub struct QueryMatch {
    /// 捕获名称 → 匹配文本
    pub captures: HashMap<String, String>,
    /// 捕获名称 → 起始行号(1-based)
    pub capture_lines: HashMap<String, usize>,
    /// 完整匹配的起始行
    pub start_line: usize,
    /// 完整匹配的结束行
    pub end_line: usize,
}

impl AstQuery {
    /// 创建新的 AST 查询器
    pub fn new(language: &str) -> Result<Self> {
        let lang = get_language(language)?;
        let mut parser = Parser::new();
        parser.set_language(&lang).map_err(|e| anyhow!("设置语言失败: {}", e))?;
        Ok(Self { language: language.to_string(), parser })
    }

    /// 查找符号定义位置
    ///
    /// 手动遍历 AST，找到 name 与 symbol 匹配的顶层定义节点。
    /// 不依赖 tree-sitter Query API，兼容所有 tree-sitter 版本。
    pub fn find_definition(&mut self, source: &str, symbol: &str) -> Result<Option<QueryMatch>> {
        let tree = self.parser.parse(source, None)
            .ok_or_else(|| anyhow!("解析源码失败"))?;
        let bytes = source.as_bytes();
        let _root = tree.root_node();
        let mut result = None;

        // 定义节点类型列表：各语言中可能包含定义的节点类型
        let def_types = match self.language.as_str() {
            "rust" => &["function_item", "struct_item", "trait_item", "enum_item", "type_item", "const_item", "static_item", "impl_item", "mod_item"][..],
            "python" => &["function_definition", "class_definition", "assignment"][..],
            "javascript" | "typescript" => &["function_declaration", "class_declaration", "method_definition", "variable_declaration", "interface_declaration", "type_alias_declaration", "enum_declaration"][..],
            "go" => &["function_declaration", "type_declaration", "type_spec", "const_declaration", "var_declaration"][..],
            "csharp" => &["method_declaration", "class_declaration", "struct_declaration", "interface_declaration", "enum_declaration", "delegate_declaration", "property_declaration"][..],
            _ => return Ok(None),
        };

        // 对顶层节点做 BFS 遍历
        let mut cursor = tree.walk();
        'outer: loop {
            let node = cursor.node();
            if def_types.contains(&node.kind()) && let Some(name_node) = node.child_by_field_name("name") && let Ok(name) = name_node.utf8_text(bytes) && name == symbol {
                let mut captures = HashMap::new();
                let mut capture_lines = HashMap::new();
                if let Ok(text) = node.utf8_text(bytes) {
                    captures.insert("name".to_string(), text.to_string());
                }
                capture_lines.insert("name".to_string(), name_node.start_position().row + 1);
                result = Some(QueryMatch {
                    captures,
                    capture_lines,
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                });
                break 'outer;
            }

            if cursor.goto_first_child() { continue; }
            loop {
                if cursor.goto_next_sibling() { continue 'outer; }
                if !cursor.goto_parent() { break 'outer; }
            }
        }

        Ok(result)
    }

    /// 获取解析结果中的顶层实体名列表
    pub fn list_definitions(&mut self, source: &str) -> Result<Vec<String>> {
        let tree = self.parser.parse(source, None)
            .ok_or_else(|| anyhow!("解析源码失败"))?;
        let bytes = source.as_bytes();
        let mut defs = Vec::new();

        let def_types = match self.language.as_str() {
            "rust" => &["function_item", "struct_item", "trait_item", "enum_item", "type_item", "const_item", "impl_item"][..],
            "python" => &["function_definition", "class_definition"][..],
            "javascript" | "typescript" => &["function_declaration", "class_declaration", "method_definition", "interface_declaration"][..],
            "go" => &["function_declaration", "type_spec"][..],
            "csharp" => &["method_declaration", "class_declaration", "struct_declaration", "interface_declaration"][..],
            _ => return Ok(defs),
        };

        let mut cursor = tree.walk();
        'walk: loop {
            let node = cursor.node();
            if def_types.contains(&node.kind()) && let Some(name_node) = node.child_by_field_name("name") && let Ok(name) = name_node.utf8_text(bytes) {
                defs.push(name.to_string());
            }

            if cursor.goto_first_child() { continue; }
            loop {
                if cursor.goto_next_sibling() { continue 'walk; }
                if !cursor.goto_parent() { break 'walk; }
            }
        }

        defs.sort();
        defs.dedup();
        Ok(defs)
    }
}

/// 获取指定语言的 tree-sitter Language
pub fn get_language(name: &str) -> Result<Language> {
    match name {
        "rust" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "python" => Ok(tree_sitter_python::LANGUAGE.into()),
        "javascript" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" | "tsx" => Ok(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => Ok(tree_sitter_go::LANGUAGE.into()),
        "csharp" => Ok(tree_sitter_c_sharp::LANGUAGE.into()),
        _ => anyhow::bail!("不支持的语言: {}", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_definition_rust() {
        let mut q = AstQuery::new("rust").unwrap();
        let source = "struct Point { x: i32, y: i32 }\nfn add(a: i32, b: i32) -> i32 { a + b }";
        let found = q.find_definition(source, "add").unwrap();
        assert!(found.is_some());
        let m = found.unwrap();
        assert!(m.captures.contains_key("name"));
    }

    #[test]
    fn test_find_definition_not_found() {
        let mut q = AstQuery::new("rust").unwrap();
        let source = "fn foo() {}";
        let found = q.find_definition(source, "bar").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_list_definitions() {
        let mut q = AstQuery::new("rust").unwrap();
        let source = "struct A;\nfn b() {}\ntrait C {}";
        let defs = q.list_definitions(source).unwrap();
        assert!(defs.contains(&"A".to_string()));
        assert!(defs.contains(&"b".to_string()));
        assert!(defs.contains(&"C".to_string()));
    }

    #[test]
    fn test_python_definition() {
        let mut q = AstQuery::new("python").unwrap();
        let source = "def hello(): pass\nclass World: pass";
        let defs = q.list_definitions(source).unwrap();
        assert!(defs.contains(&"hello".to_string()));
        assert!(defs.contains(&"World".to_string()));
    }

    #[test]
    fn test_js_definition() {
        let mut q = AstQuery::new("javascript").unwrap();
        let source = "function add(a, b) { return a + b; }";
        let found = q.find_definition(source, "add").unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_list_definitions_empty() {
        let mut q = AstQuery::new("rust").unwrap();
        let defs = q.list_definitions("").unwrap();
        assert!(defs.is_empty());
    }
}
