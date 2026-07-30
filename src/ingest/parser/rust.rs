use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Node, Parser};

use super::{Entity, FileInsight, ImportStmt, LanguageProcessor};

pub struct RustProcessor;

impl RustProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    fn extract(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let language: Language = tree_sitter_rust::LANGUAGE.into();
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
                "struct_item" => Self::record_entity(node, bytes, "struct", &mut entities, None),
                "function_item" => {
                    let sig = node.utf8_text(bytes).ok().and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                    Self::record_entity(node, bytes, "function", &mut entities, sig);
                }
                "trait_item" => Self::record_entity(node, bytes, "trait", &mut entities, None),
                "impl_item" => {
                    let sig = node.utf8_text(bytes).ok().and_then(|t| t.lines().next().map(|s| s.to_string()));
                    if let Some(name) = node.child_by_field_name("type").and_then(|n| n.utf8_text(bytes).ok()) {
                        entities.push(Entity {
                            name: name.to_string(), kind: "impl".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig, summary: None,
                        });
                    }
                }
                "enum_item" => Self::record_entity(node, bytes, "enum", &mut entities, None),
                "type_item" => Self::record_entity(node, bytes, "type", &mut entities, None),
                "const_item" => Self::record_entity(node, bytes, "const", &mut entities, None),
                "static_item" => Self::record_entity(node, bytes, "static", &mut entities, None),
                "use_declaration" => {
                    if let Ok(text) = node.utf8_text(bytes) {
                        Self::parse_use_stmt(text, node.start_position().row + 1, &mut imports);
                    }
                }
                "mod_item"
                    if node.child_by_field_name("body").is_none() => {
                        Self::record_entity(node, bytes, "mod", &mut entities, None);
                    }
                _ => {}
            }

            if cursor.goto_first_child() { continue; }
            loop {
                if cursor.goto_next_sibling() { continue 'walk; }
                if !cursor.goto_parent() { break 'walk; }
            }
        }

        let doc_comments = Self::collect_doc_comments(source);
        for entity in &mut entities {
            Self::associate_doc(entity, &doc_comments);
        }
        (entities, imports)
    }

    fn record_entity(node: Node, bytes: &[u8], kind: &str, entities: &mut Vec<Entity>, signature: Option<String>) {
        let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
        if let Some(name) = name {
            entities.push(Entity {
                name: name.to_string(), kind: kind.to_string(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                doc_comment: None, signature, summary: None,
            });
        }
    }

    fn parse_use_stmt(text: &str, line: usize, out: &mut Vec<ImportStmt>) {
        let inner = text.trim().strip_prefix("use ").unwrap_or(text.trim()).strip_suffix(';').unwrap_or(text.trim());
        if let Some(pos) = inner.find(" as ") {
            out.push(ImportStmt { source: inner[..pos].trim().to_string(), alias: Some(inner[pos + 4..].trim().to_string()), line });
        } else {
            out.push(ImportStmt { source: inner.trim().to_string(), alias: None, line });
        }
    }

    fn collect_doc_comments(source: &str) -> Vec<(usize, String)> {
        let mut docs = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let t = line.trim();
            if let Some(d) = t.strip_prefix("///") { docs.push((i + 1, d.trim_start().to_string())); }
            else if let Some(d) = t.strip_prefix("//!") { docs.push((i + 1, d.trim_start().to_string())); }
        }
        docs
    }

    fn associate_doc(entity: &mut Entity, docs: &[(usize, String)]) {
        let mut collected = Vec::new();
        for &(line, ref text) in docs {
            if line >= entity.line_start { break; }
            if entity.line_start - line <= 3 && (collected.is_empty() || line == collected.last().map(|&(l, _)| l).unwrap_or(0) + 1) {
                collected.push((line, text.clone()));
            } else if line < entity.line_start - 3 || (!collected.is_empty() && line != collected.last().map(|&(l, _)| l).unwrap_or(0) + 1) { collected.clear(); }
        }
        if !collected.is_empty() {
            entity.doc_comment = Some(collected.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n"));
        }
    }

    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1; let t = line.trim();
            if let Some(rest) = t.strip_prefix("use ") {
                let inner = rest.strip_suffix(';').unwrap_or(rest);
                imports.push(ImportStmt { source: inner.split(" as ").next().unwrap_or(inner).trim().to_string(), alias: inner.split(" as ").nth(1).map(|s| s.trim().to_string()), line: line_no });
                continue;
            }
            let core = t.strip_prefix("pub ").or_else(|| t.strip_prefix("pub(crate) ")).or_else(|| t.strip_prefix("pub(super) ")).unwrap_or(t);
            let n = |s: &str, delim: &[char]| s.split(delim).next().map(|s| s.trim().to_string());
            if let Some(name) = core.strip_prefix("struct ").and_then(|s| n(s, &['{', ' ', '<', ';'])) { entities.push(Entity { name, kind: "struct".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None }); }
            else if let Some(name) = core.strip_prefix("fn ").and_then(|s| n(s, &['(', ' ', '<'])) { entities.push(Entity { name, kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), summary: None }); }
            else if let Some(name) = core.strip_prefix("trait ").and_then(|s| n(s, &['{', ' ', '<'])) { entities.push(Entity { name, kind: "trait".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None }); }
            else if let Some(name) = core.strip_prefix("enum ").and_then(|s| n(s, &['{', ' ', '<'])) { entities.push(Entity { name, kind: "enum".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None }); }
            else if let Some(name) = core.strip_prefix("mod ").and_then(|s| s.strip_suffix(';')).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) { entities.push(Entity { name, kind: "mod".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None }); }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for RustProcessor {
    fn name(&self) -> &'static str { "Rust" }
    fn extensions(&self) -> &[&str] { &[".rs"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        if source.is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: "Rust".into(), entities: vec![], imports: vec![], doc_comments: vec![], source: source.to_string() });
        }
        let (entities, imports) = Self::extract(source);
        if entities.is_empty() && !source.trim().is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: "Rust".into(), entities: Self::fallback(source).0, imports: Self::fallback(source).1, doc_comments: vec![], source: source.to_string() });
        }
        Ok(FileInsight { path: path.to_path_buf(), language: "Rust".into(), entities, imports, doc_comments: vec![], source: source.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_rust_basics() {
        let source = r#"/// A point
struct Point { x: i32, y: i32 }

fn add(a: i32, b: i32) -> i32 { a + b }

trait Shape { fn area(&self) -> f64; }

use std::collections::HashMap as Map;

const MAX: usize = 100;
"#;
        let proc = RustProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.rs")).unwrap();
        assert_eq!(result.entities.len(), 4);
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"add"));
        assert!(names.contains(&"Shape"));
        assert!(names.contains(&"MAX"));
        assert!(result.entities.iter().find(|e| e.name == "Point").unwrap().doc_comment.is_some());
        assert_eq!(result.imports[0].alias.as_deref(), Some("Map"));
    }
}
