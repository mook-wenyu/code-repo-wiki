use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Parser};

use super::{Entity, FileInsight, ImportStmt, LanguageProcessor};

pub struct GoProcessor;

impl GoProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    fn walk(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let language: Language = tree_sitter_go::LANGUAGE.into();
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            return Self::fallback(source);
        }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Self::fallback(source),
        };

        let mut cursor = tree.walk();
        if !cursor.goto_first_child() { return (entities, imports); }

        'walk: loop {
            let node = cursor.node();
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
                            doc_comment: None, signature: None,
                        });
                    }
                }
                "function_declaration" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        let sig = node.utf8_text(bytes).ok().and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: "function".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig,
                        });
                    }
                }
                "method_declaration" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        let sig = node.utf8_text(bytes).ok().and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: "function".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig,
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

            if cursor.goto_first_child() { continue; }
            loop {
                if cursor.goto_next_sibling() { continue 'walk; }
                if !cursor.goto_parent() { break 'walk; }
            }
        }

        (entities, imports)
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
                entities.push(Entity { name, kind: kind.to_string(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None });
            } else if let Some(n) = t.strip_prefix("func ").and_then(|s| {
                s.split('(').next().and_then(|first| {
                    if first.contains(' ') { first.split_whitespace().last().map(|s| s.to_string()) }
                    else { Some(first.split_whitespace().next().unwrap_or("").to_string()) }
                })
            }) {
                entities.push(Entity { name: n, kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()) });
            }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for GoProcessor {
    fn name(&self) -> &'static str { "Go" }
    fn extensions(&self) -> &[&str] { &[".go"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        if source.is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: "Go".into(), entities: vec![], imports: vec![], doc_comments: vec![] });
        }
        let (entities, imports) = Self::walk(source);
        Ok(FileInsight { path: path.to_path_buf(), language: "Go".into(), entities, imports, doc_comments: vec![] })
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
