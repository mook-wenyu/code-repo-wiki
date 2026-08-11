use std::path::Path;
use anyhow::Result;
use tree_sitter::{Language, Node, Parser};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

pub struct TypeScriptProcessor;

impl TypeScriptProcessor {
    pub fn new() -> Result<Self> {
        // 保持原行为：启动时校验 tree-sitter-typescript 语法可用
        let lang: Language = tree_sitter_typescript::LANGUAGE_TSX.into();
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
    // U09：TypeScript 枚举补齐（enum_declaration 此前未映射，枚举被遗漏）
    KindRule::with_sig("enum_declaration", "enum", '{'),
];

/// TypeScript 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（import_statement 文本解析）、正则 fallback。
/// 公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for TypeScriptProcessor {
    fn language() -> &'static str { "TypeScript" }
    /// P0-6：.tsx 含 JSX 语法，用 TS 语法解析必然 has_error=true 导致实体全丢。
    /// TSX 语法是 TS 的超集（tree-sitter-typescript 官方说明），单一语法覆盖
    /// .ts/.tsx 两扩展。0.23.2 语言 crate 的 TSX 常量名为 LANGUAGE_TSX。
    fn grammar() -> Language { tree_sitter_typescript::LANGUAGE_TSX.into() }

    fn kinds() -> &'static [KindRule] {
        KINDS
    }

    fn handle_special(node: Node, bytes: &[u8], entities: &mut Vec<Entity>, imports: &mut Vec<ImportStmt>) {
        // P1-15：variable_declarator 动态 kind——箭头函数（const fn = () => {}）
        // 归类 function 而非 variable（React 函数组件依赖此语义进调用图/聚类）；
        // 解构（const {x,y} = obj / const [a,b] = arr）不产出实体，"{x, y}" 是伪
        // 实体名，进图谱只会制造噪音。name 字段非 identifier 即解构模式。
        if node.kind() == "variable_declarator" {
            let name_node = node.child_by_field_name("name");
            let is_identifier = name_node.is_some_and(|n| n.kind() == "identifier");
            if is_identifier
                && let Some(name) = name_node.and_then(|n| n.utf8_text(bytes).ok())
            {
                let is_arrow = node
                    .child_by_field_name("value")
                    .is_some_and(|v| v.kind() == "arrow_function");
                let kind = if is_arrow { "function" } else { "variable" };
                entities.push(Entity {
                    name: name.to_string(), kind: kind.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    doc_comment: None, signature: None, visibility: None,
                });
            }
            return;
        }
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
                entities.push(Entity { name: name.to_string(), kind: "class".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, visibility: None });
            } else if let Some(name) = core.strip_prefix("interface ").and_then(|s| s.split(&['{', ' ', '<', ';'][..]).next()).map(|s| s.trim()) {
                entities.push(Entity { name: name.to_string(), kind: "interface".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: None, visibility: None });
            } else if let Some(name) = core.strip_prefix("function ").and_then(|s| s.split(&['(', ' ', '<', '{', ';'][..]).next()).map(|s| s.trim()) {
                entities.push(Entity { name: name.to_string(), kind: "function".into(), line_start: line_no, line_end: line_no, doc_comment: None, signature: Some(t.to_string()), visibility: None });
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

    /// P0-6：TSX 语法解析回归——JSX 函数与 interface 均被解析
    #[test]
    fn test_tsx_jsx_entities_parsed() {
        let source = r#"interface Props { name: string; }

export function Greeting({ name }: Props) {
  return <div>Hello {name}</div>;
}
"#;
        let proc = TypeScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.tsx")).unwrap();
        assert!(result.entities.iter().any(|e| e.name == "Greeting" && e.kind == "function"), "Greeting 应解析: {:?}", result.entities);
        assert!(result.entities.iter().any(|e| e.name == "Props" && e.kind == "interface"), "Props 应解析: {:?}", result.entities);
    }

    /// P1-15：TS 箭头函数归类 function（非 variable）；解构不产出实体
    #[test]
    fn test_ts_arrow_function_and_destructuring() {
        let source = r#"export const Greeting = ({ name }: Props) => {
  return <div>Hello {name}</div>;
};
const { x, y } = obj;
const plain = 42;
"#;
        let proc = TypeScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("comp.tsx")).unwrap();
        assert!(result.entities.iter().any(|e| e.name == "Greeting" && e.kind == "function"), "箭头函数应归类 function: {:?}", result.entities);
        assert!(result.entities.iter().any(|e| e.name == "plain" && e.kind == "variable"), "普通变量仍为 variable");
        assert!(!result.entities.iter().any(|e| e.name.contains('{') || e.name.contains('}')), "解构不得产出伪实体名: {:?}", result.entities);
    }
}
