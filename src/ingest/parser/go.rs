use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Node};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

pub struct GoProcessor;

impl GoProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// kind 映射表（差异点数据化）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::with_sig("function_declaration", "function", '{'),
    KindRule::with_sig("method_declaration", "function", '{'),
];

/// Go 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（type_spec 动态 kind / import_spec）、正则 fallback。
/// 公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for GoProcessor {
    fn language() -> &'static str { "Go" }
    fn grammar() -> Language { tree_sitter_go::LANGUAGE.into() }

    fn kinds() -> &'static [KindRule] {
        KINDS
    }

    fn handle_special(node: Node, bytes: &[u8], entities: &mut Vec<Entity>, imports: &mut Vec<ImportStmt>) {
        match node.kind() {
            "type_spec" => {
                let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                if let Some(name) = name {
                    let kind = if node.child_by_field_name("type").map(|t| t.kind()) == Some("struct_type") { "struct" }
                        else if node.child_by_field_name("type").map(|t| t.kind()) == Some("interface_type") { "interface" }
                        else { "type" };
                    entities.push(Entity {
                        name: name.to_string(), kind: kind.to_string(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        doc_comment: None, signature: None, summary: None,
                    });
                }
            }
            "import_spec" => {
                if let Some(path) = node.child_by_field_name("path").and_then(|n| n.utf8_text(bytes).ok()) {
                    imports.push(ImportStmt {
                        source: path.trim_matches('"').to_string(),
                        alias: None,
                        line: node.start_position().row + 1,
                    });
                }
            }
            _ => {}
        }
    }

    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1; let t = line.trim();
            if let Some(rest) = t.strip_prefix("import ") {
                let path = rest.trim().trim_matches(&['"', '(', ')'][..]);
                imports.push(ImportStmt { source: path.to_string(), alias: None, line: line_no });
            }
            if let Some(rest) = t.strip_prefix("type ") {
                let name = rest.split_whitespace().next().unwrap_or("").to_string();
                if name.is_empty() { continue; }
                let kind = if rest.contains("struct") { "struct" } else if rest.contains("interface") { "interface" } else { "type" };
                entities.push(Entity { name, kind: kind.to_string(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None });
            } else if let Some(n) = t.strip_prefix("func ").and_then(|s| {
                s.split('(').next().and_then(|first| {
                    if first.contains(' ') { first.split_whitespace().last().map(|s| s.to_string()) }
                    else { Some(first.split_whitespace().next().unwrap_or("").to_string()) }
                })
            }) {
                entities.push(Entity { name: n, kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), summary: None });
            }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for GoProcessor {
    fn name(&self) -> &'static str { Self::language() }
    fn extensions(&self) -> &[&str] { &[".go"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_go_basics() {
        let source = r#"package main
import "fmt"
type Point struct { X int }
type Writer interface { Write([]byte) (int, error) }
func add(a int, b int) int { return a + b }
"#;
        let proc = GoProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.go")).unwrap();
        assert_eq!(result.entities.len(), 3);
        assert!(result.entities.iter().any(|e| e.name == "Point"));
        assert!(result.entities.iter().any(|e| e.name == "Writer"));
        assert!(result.entities.iter().any(|e| e.name == "add"));
    }
}
