use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Parser};

use super::{Entity, FileInsight, ImportStmt, LanguageProcessor};

pub struct TypeScriptProcessor {
    ts_lang: Language,
}

impl TypeScriptProcessor {
    pub fn new() -> Result<Self> {
        let ts_lang: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let mut parser = Parser::new();
        parser.set_language(&ts_lang)?;
        Ok(Self { ts_lang })
    }

    fn walk(source: &str, lang: &Language) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let mut parser = Parser::new();
        if parser.set_language(lang).is_err() {
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
                "class_declaration" | "interface_declaration" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        let kind = if node.kind() == "class_declaration" { "class" } else { "interface" };
                        entities.push(Entity {
                            name: name.to_string(), kind: kind.to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: None, summary: None,
                        });
                    }
                }
                "function_declaration" | "method_definition" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        let sig = node.utf8_text(bytes).ok().and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: "function".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig, summary: None,
                        });
                    }
                }
                "type_alias_declaration" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        entities.push(Entity {
                            name: name.to_string(), kind: "type".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: None, summary: None,
                        });
                    }
                }
                "variable_declarator" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        entities.push(Entity {
                            name: name.to_string(), kind: "variable".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: None, summary: None,
                        });
                    }
                }
                "import_statement" => {
                    if let Some(src) = node.child_by_field_name("source").and_then(|n| n.utf8_text(bytes).ok()) {
                        imports.push(ImportStmt {
                            source: src.trim_matches(&['"', '\''][..]).to_string(),
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
                if let Some(from) = rest.find(" from ") {
                    let src = rest[from + 6..].trim().trim_matches(&['"', '\'', ';'][..]);
                    imports.push(ImportStmt { source: src.to_string(), alias: None, line: line_no });
                }
                continue;
            }
            let core = t.strip_prefix("export ").or_else(|| t.strip_prefix("export default ")).or_else(|| t.strip_prefix("export async ")).unwrap_or(t);
            if let Some(name) = core.strip_prefix("class ").and_then(|s| s.split(&['{', ' ', '<', '(', ';'][..]).next()).map(|s| s.trim()) {
                entities.push(Entity { name: name.to_string(), kind: "class".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None });
            } else if let Some(name) = core.strip_prefix("interface ").and_then(|s| s.split(&['{', ' ', '<', ';'][..]).next()).map(|s| s.trim()) {
                entities.push(Entity { name: name.to_string(), kind: "interface".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None });
            } else if let Some(name) = core.strip_prefix("function ").and_then(|s| s.split(&['(', ' ', '<', '{', ';'][..]).next()).map(|s| s.trim()) {
                entities.push(Entity { name: name.to_string(), kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), summary: None });
            }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for TypeScriptProcessor {
    fn name(&self) -> &'static str { "TypeScript" }
    fn extensions(&self) -> &[&str] { &[".ts", ".tsx"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        if source.is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: "TypeScript".into(), entities: vec![], imports: vec![], doc_comments: vec![], source: source.to_string() });
        }
        let (entities, imports) = Self::walk(source, &self.ts_lang);
        Ok(FileInsight { path: path.to_path_buf(), language: "TypeScript".into(), entities, imports, doc_comments: vec![], source: source.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_ts_basics() {
        let source = r#"import { Component } from "react";
interface Props { name: string; }
function greet(name: string): string { return "hello"; }
const x = 42;
"#;
        let proc = TypeScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.ts")).unwrap();
        assert_eq!(result.entities.len(), 3);
        assert!(result.entities.iter().any(|e| e.name == "Props"));
        assert!(result.entities.iter().any(|e| e.name == "greet"));
        assert!(result.entities.iter().any(|e| e.name == "x"));
        assert_eq!(result.imports[0].source, "react");
    }
}
