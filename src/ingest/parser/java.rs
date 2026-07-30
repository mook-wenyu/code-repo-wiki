use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Parser};

use super::{Entity, FileInsight, ImportStmt, LanguageProcessor};

pub struct JavaProcessor;

impl JavaProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    fn walk(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let language: Language = tree_sitter_java::LANGUAGE.into();
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
                "class_declaration" | "interface_declaration"
                | "enum_declaration" | "record_declaration" => {
                    let kind = match node.kind() {
                        "class_declaration" => "class",
                        "interface_declaration" => "interface",
                        "enum_declaration" => "enum",
                        "record_declaration" => "class",
                        _ => "class",
                    };
                    if let Some(name) = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok()) {
                        let sig = node.utf8_text(bytes).ok()
                            .and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: kind.to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig, summary: None,
                        });
                    }
                }
                "method_declaration" => {
                    if let Some(name) = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok()) {
                        let sig = node.utf8_text(bytes).ok()
                            .and_then(|t| t.split('{').next().map(|s| s.trim().to_string()));
                        entities.push(Entity {
                            name: name.to_string(), kind: "function".to_string(),
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            doc_comment: None, signature: sig, summary: None,
                        });
                    }
                }
                "field_declaration" => {
                    let mut cur = node.walk();
                    if cur.goto_first_child() {
                        loop {
                            if cur.node().kind() == "variable_declarator"
                                && let Some(name) = cur.node().child_by_field_name("name")
                                    .and_then(|n| n.utf8_text(bytes).ok())
                            {
                                entities.push(Entity {
                                    name: name.to_string(), kind: "variable".to_string(),
                                    line_start: node.start_position().row + 1,
                                    line_end: node.end_position().row + 1,
                                    doc_comment: None, signature: None, summary: None,
                                });
                            }
                            if !cur.goto_next_sibling() { break; }
                        }
                    }
                }
                "import_declaration" => {
                    if let Ok(text) = node.utf8_text(bytes) {
                        let name = text
                            .strip_prefix("import ")
                            .and_then(|s| s.strip_suffix(';'))
                            .map(|s| s.trim())
                            .unwrap_or(text.trim());
                        imports.push(ImportStmt {
                            source: name.to_string(),
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
            let line_no = i + 1;
            let t = line.trim();

            if let Some(rest) = t.strip_prefix("import ") {
                let path = rest.trim_end_matches(';').trim();
                imports.push(ImportStmt { source: path.to_string(), alias: None, line: line_no });
                continue;
            }

            for prefix in &["class ", "interface ", "enum ", "record "] {
                if let Some(rest) = t.find(prefix).map(|pos| &t[pos..]) {
                    let name = rest.strip_prefix(prefix)
                        .and_then(|s| s.split(&['{', ' ', '<', ';', '(', '}'][..]).next())
                        .map(|s| s.trim());
                    if let Some(name) = name
                        && !name.is_empty()
                    {
                        let kind = match *prefix {
                            "class " | "record " => "class",
                            "interface " => "interface",
                            "enum " => "enum",
                            _ => "class",
                        };
                        entities.push(Entity {
                            name: name.to_string(), kind: kind.to_string(),
                            line_start: line_no, line_end: line_no,
                            doc_comment: None, signature: Some(t.to_string()), summary: None,
                        });
                        break;
                    }
                }
            }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for JavaProcessor {
    fn name(&self) -> &'static str { "Java" }
    fn extensions(&self) -> &[&str] { &[".java"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        if source.is_empty() {
            return Ok(FileInsight {
                path: path.to_path_buf(), language: "Java".into(),
                source: source.to_string(), entities: vec![], imports: vec![], doc_comments: vec![],
            });
        }
        let (entities, imports) = Self::walk(source);
        Ok(FileInsight {
            path: path.to_path_buf(), language: "Java".into(),
            source: source.to_string(), entities, imports, doc_comments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_java_basics() {
        let source = r#"import java.util.List;
import java.io.File;

class Calculator {
    public int add(int a, int b) { return a + b; }
    private int count;
}

interface Runnable {
    void run();
}
"#;
        let proc = JavaProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("Test.java")).unwrap();
        assert!(result.entities.iter().any(|e| e.name == "Calculator" && e.kind == "class"));
        assert!(result.entities.iter().any(|e| e.name == "Runnable" && e.kind == "interface"));
        assert!(result.entities.iter().any(|e| e.name == "add" && e.kind == "function"));
        assert!(result.entities.iter().any(|e| e.name == "count" && e.kind == "variable"));
        assert!(result.imports.iter().any(|i| i.source == "java.util.List"));
        assert!(result.imports.iter().any(|i| i.source == "java.io.File"));
    }
}
