use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Parser};

use super::{Entity, FileInsight, ImportStmt, LanguageProcessor};

pub struct PythonProcessor;

impl PythonProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    fn walk(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let language: Language = tree_sitter_python::LANGUAGE.into();
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
                "class_definition" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        entities.push(Entity {
                            name: name.to_string(), kind: "class".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: None, summary: None,
                        });
                    }
                }
                "function_definition" => {
                    let name = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok());
                    if let Some(name) = name {
                        let sig = node.utf8_text(bytes).ok().and_then(|t| t.split(':').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: "function".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig, summary: None,
                        });
                    }
                }
                "import_statement" => {
                    if let Ok(text) = node.utf8_text(bytes) {
                        imports.push(ImportStmt {
                            source: text.trim().strip_prefix("import ").unwrap_or(text.trim()).to_string(),
                            alias: None,
                            line: node.start_position().row + 1,
                        });
                    }
                }
                "import_from_statement" => {
                    if let Ok(text) = node.utf8_text(bytes) {
                        imports.push(ImportStmt {
                            source: text.trim().to_string(),
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

        Self::associate_docstrings(source, &mut entities);
        (entities, imports)
    }

    fn associate_docstrings(source: &str, entities: &mut [Entity]) {
        let lines: Vec<&str> = source.lines().collect();
        for e in entities.iter_mut() {
            for i in e.line_start..lines.len().min(e.line_start + 5) {
                let t = lines[i - 1].trim();
                let found = if t.starts_with("\"\"\"") {
                    let doc = t.trim_start_matches("\"\"\"").trim_end_matches("\"\"\"").to_string();
                    Some(doc)
                } else if t.starts_with("'''") {
                    let doc = t.trim_start_matches("'''").trim_end_matches("'''").to_string();
                    Some(doc)
                } else { None };
                if let Some(doc) = found {
                    if !doc.is_empty() { e.doc_comment = Some(doc); }
                    break;
                } else if !t.is_empty() && !t.starts_with('#') {
                    break;
                }
            }
        }
    }

    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1; let t = line.trim();
            if let Some(rest) = t.strip_prefix("import ") { imports.push(ImportStmt { source: rest.to_string(), alias: None, line: line_no }); }
            else if let Some(rest) = t.strip_prefix("from ") { imports.push(ImportStmt { source: rest.to_string(), alias: None, line: line_no }); }
            else if let Some(name) = t.strip_prefix("class ").and_then(|s| s.split(&['(', ':', ' '][..]).next()) { entities.push(Entity { name: name.to_string(), kind: "class".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, summary: None }); }
            else if let Some(name) = t.strip_prefix("def ").and_then(|s| s.split(&['(', ':', ' '][..]).next()) { entities.push(Entity { name: name.to_string(), kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), summary: None }); }
            else if let Some(name) = t.strip_prefix("async def ").and_then(|s| s.split(&['(', ':', ' '][..]).next()) { entities.push(Entity { name: name.to_string(), kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), summary: None }); }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for PythonProcessor {
    fn name(&self) -> &'static str { "Python" }
    fn extensions(&self) -> &[&str] { &[".py"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        if source.is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: "Python".into(), entities: vec![], imports: vec![], doc_comments: vec![], source: source.to_string() });
        }
        let (entities, imports) = Self::walk(source);
        Ok(FileInsight { path: path.to_path_buf(), language: "Python".into(), entities, imports, doc_comments: vec![], source: source.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_python_basics() {
        let source = r#"import os
from typing import List

class Person:
    """A person with a name."""
    pass

def greet(name: str) -> str:
    return f"hello {name}"
"#;
        let proc = PythonProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.py")).unwrap();
        assert_eq!(result.entities.len(), 2);
        assert!(result.entities.iter().any(|e| e.name == "Person"));
        assert!(result.entities.iter().any(|e| e.name == "greet"));
        assert!(result.imports.len() >= 2);
    }
}
