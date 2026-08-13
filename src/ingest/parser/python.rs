use anyhow::Result;
use std::path::Path;
use tree_sitter::{Language, Node};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

pub struct PythonProcessor;

impl PythonProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// kind 映射表（差异点数据化）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::plain("class_definition", "class"),
    // Python 函数签名以冒号结尾（def foo(a: int) -> str:），截断符用 ':'
    KindRule::with_sig("function_definition", "function", ':'),
];

/// Python 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（import/import_from 文本解析）、docstring 关联钩子、正则 fallback。
/// 公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for PythonProcessor {
    fn language() -> &'static str {
        "Python"
    }
    fn grammar() -> Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn kinds() -> &'static [KindRule] {
        KINDS
    }

    fn handle_special(
        node: Node,
        bytes: &[u8],
        entities: &mut Vec<Entity>,
        imports: &mut Vec<ImportStmt>,
    ) {
        match node.kind() {
            "import_statement" => {
                if let Ok(text) = node.utf8_text(bytes) {
                    imports.push(ImportStmt {
                        source: text
                            .trim()
                            .strip_prefix("import ")
                            .unwrap_or(text.trim())
                            .to_string(),
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
            // U09：模块级常量补齐——Python 无 const 语法，模块常量即顶层赋值
            // 语句。判定：语句在顶层（缩进 0，column==0）且直接子节点是
            // assignment、左值为标识符——如 `MAX_SIZE = 100`。
            "expression_statement" if node.start_position().column == 0 => {
                let mut cur = node.walk();
                if cur.goto_first_child()
                    && cur.node().kind() == "assignment"
                    && let Some(left) = cur.node().child_by_field_name("left")
                    && left.kind() == "identifier"
                    && let Ok(name) = left.utf8_text(bytes)
                {
                    entities.push(Entity {
                        name: name.trim().to_string(),
                        kind: "constant".to_string(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        doc_comment: None,
                        signature: None,
                        visibility: None,
                    });
                }
            }
            _ => {}
        }
    }

    fn post_process(source: &str, entities: &mut Vec<Entity>) {
        Self::associate_docstrings(source, entities);
    }

    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1;
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("import ") {
                imports.push(ImportStmt {
                    source: rest.to_string(),
                    alias: None,
                    line: line_no,
                });
            } else if let Some(rest) = t.strip_prefix("from ") {
                imports.push(ImportStmt {
                    source: rest.to_string(),
                    alias: None,
                    line: line_no,
                });
            } else if let Some(name) = t
                .strip_prefix("class ")
                .and_then(|s| s.split(&['(', ':', ' '][..]).next())
            {
                entities.push(Entity {
                    name: name.to_string(),
                    kind: "class".into(),
                    line_start: line_no,
                    line_end: line_no,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                });
            } else if let Some(name) = t
                .strip_prefix("def ")
                .and_then(|s| s.split(&['(', ':', ' '][..]).next())
            {
                entities.push(Entity {
                    name: name.to_string(),
                    kind: "function".into(),
                    line_start: line_no,
                    line_end: line_no,
                    doc_comment: None,
                    signature: Some(t.to_string()),
                    visibility: None,
                });
            } else if let Some(name) = t
                .strip_prefix("async def ")
                .and_then(|s| s.split(&['(', ':', ' '][..]).next())
            {
                entities.push(Entity {
                    name: name.to_string(),
                    kind: "function".into(),
                    line_start: line_no,
                    line_end: line_no,
                    doc_comment: None,
                    signature: Some(t.to_string()),
                    visibility: None,
                });
            }
        }
        (entities, imports)
    }
}

impl PythonProcessor {
    /// 扫描实体起始行后 5 行内的 docstring 并关联为 doc_comment
    fn associate_docstrings(source: &str, entities: &mut [Entity]) {
        let lines: Vec<&str> = source.lines().collect();
        for e in entities.iter_mut() {
            // P0-7：docstring 在声明行的下一行——循环必须从 line_start+1 起扫，
            // 否则首轮取到声明行本身（非 docstring）立即 break，docstring 永不关联
            for i in (e.line_start + 1)..lines.len().min(e.line_start + 6) {
                let t = lines[i - 1].trim();
                let found = if t.starts_with("\"\"\"") {
                    let doc = t
                        .trim_start_matches("\"\"\"")
                        .trim_end_matches("\"\"\"")
                        .to_string();
                    Some(doc)
                } else if t.starts_with("'''") {
                    let doc = t
                        .trim_start_matches("'''")
                        .trim_end_matches("'''")
                        .to_string();
                    Some(doc)
                } else {
                    None
                };
                if let Some(doc) = found {
                    if !doc.is_empty() {
                        e.doc_comment = Some(doc);
                    }
                    break;
                } else if !t.is_empty() && !t.starts_with('#') {
                    break;
                }
            }
        }
    }
}

impl LanguageProcessor for PythonProcessor {
    fn name(&self) -> &'static str {
        Self::language()
    }
    fn extensions(&self) -> &[&str] {
        &[".py"]
    }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
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

    /// U09：Python 模块级常量补齐（顶层赋值，函数内赋值不误报）
    #[test]
    fn test_parse_python_module_constant() {
        let source = r#"MAX_SIZE = 100
API_BASE = "https://api.example.com"
DEFAULT_NAME = "x"

def helper():
    local_cache = 1
    return local_cache
"#;
        let proc = PythonProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.py")).unwrap();
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "MAX_SIZE" && e.kind == "constant"),
            "MAX_SIZE 应解析: {:?}",
            result.entities
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "API_BASE" && e.kind == "constant"),
            "API_BASE 应解析"
        );
        assert!(
            !result.entities.iter().any(|e| e.name == "local_cache"),
            "函数内赋值不应误报为常量: {:?}",
            result.entities
        );
    }

    /// P0-7：docstring 关联回归——从声明行下一行起扫，docstring 正确挂到实体
    #[test]
    fn test_python_docstring_associated() {
        let source = r#"class Person:
    """A person with a name."""
    pass

def greet(name: str) -> str:
    """Greets a person."""
    return f"hi {name}"
"#;
        let proc = PythonProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.py")).unwrap();
        let person = result
            .entities
            .iter()
            .find(|e| e.name == "Person")
            .expect("Person 应解析");
        assert_eq!(person.doc_comment.as_deref(), Some("A person with a name."));
        let greet = result
            .entities
            .iter()
            .find(|e| e.name == "greet")
            .expect("greet 应解析");
        assert_eq!(greet.doc_comment.as_deref(), Some("Greets a person."));
    }
}
