use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Node, Parser};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

pub struct TypeScriptProcessor;

impl TypeScriptProcessor {
    pub fn new() -> Result<Self> {
        // 保持原行为：启动时校验 tree-sitter-typescript 语法可用
        let lang: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let mut parser = Parser::new();
        parser.set_language(&lang)?;
        Ok(Self)
    }
}

/// kind 映射表（差异点数据化）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::plain("class_declaration", "class"),
    KindRule::plain("interface_declaration", "interface"),
    KindRule::with_sig("function_declaration", "function", '{'),
    KindRule::with_sig("method_definition", "function", '{'),
    KindRule::plain("type_alias_declaration", "type"),
    KindRule::plain("variable_declarator", "variable"),
];

/// TypeScript 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（import_statement 文本解析）、正则 fallback。
/// 公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for TypeScriptProcessor {
    fn language() -> &'static str { "TypeScript" }
    fn grammar() -> Language { tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into() }

    fn kinds() -> &'static [KindRule] {
        KINDS
    }

    fn handle_special(node: Node, bytes: &[u8], _entities: &mut Vec<Entity>, imports: &mut Vec<ImportStmt>) {
        if node.kind() == "import_statement"
            && let Some(src) = node.child_by_field_name("source").and_then(|n| n.utf8_text(bytes).ok())
        {
            imports.push(ImportStmt {
                source: src.trim_matches(&['"', '\''][..]).to_string(),
                alias: None,
                line: node.start_position().row + 1,
            });
        }
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
    fn name(&self) -> &'static str { Self::language() }
    fn extensions(&self) -> &[&str] { &[".ts", ".tsx"] }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
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
