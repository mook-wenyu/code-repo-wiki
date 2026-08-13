use anyhow::Result;
use std::path::Path;
use tree_sitter::{Language, Node};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

/// JavaScript 语言处理器。
///
/// 处理 .js/.jsx/.mjs/.cjs 文件，使用 tree-sitter-javascript 语法解析树。
/// 当 tree-sitter 解析失败时降级到正则表达式 fallback。
pub struct JavaScriptProcessor;

impl JavaScriptProcessor {
    /// 创建新的 JavaScript 处理器。
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// kind 映射表（差异点数据化）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::plain("class_declaration", "class"),
    KindRule::with_sig("function_declaration", "function", '{'),
    KindRule::with_sig("method_definition", "function", '{'),
];

/// JavaScript 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（variable_declarator 箭头函数动态 kind / import/export 文本解析）、
/// 正则 fallback。公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for JavaScriptProcessor {
    fn language() -> &'static str {
        "JavaScript"
    }
    fn grammar() -> Language {
        tree_sitter_javascript::LANGUAGE.into()
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
            "variable_declarator" => {
                // P1-15：解构（const {x,y} = obj）不产出实体——"{x, y}" 是伪
                // 实体名；仅 identifier 形态（const fn = ... / const x = ...）记录
                let name_node = node.child_by_field_name("name");
                if name_node.is_some_and(|n| n.kind() == "identifier")
                    && let Some(name) = name_node.and_then(|n| n.utf8_text(bytes).ok())
                {
                    // 检查是否为箭头函数（const fn = () => {}）
                    let is_arrow = node
                        .child_by_field_name("value")
                        .map(|v| v.kind() == "arrow_function")
                        .unwrap_or(false);
                    let kind = if is_arrow { "function" } else { "variable" };
                    entities.push(Entity {
                        name: name.to_string(),
                        kind: kind.to_string(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        doc_comment: None,
                        signature: None,
                        visibility: None,
                    });
                }
            }
            "import_statement" => {
                if let Some(src) = node
                    .child_by_field_name("source")
                    .and_then(|n| n.utf8_text(bytes).ok())
                {
                    imports.push(ImportStmt {
                        source: src.trim_matches(&['"', '\''][..]).to_string(),
                        alias: None,
                        line: node.start_position().row + 1,
                    });
                }
            }
            "export_statement" => {
                // 处理重导出：export { x } from "mod" 或 export * from "mod"
                if let Some(src) = node
                    .child_by_field_name("source")
                    .and_then(|n| n.utf8_text(bytes).ok())
                {
                    imports.push(ImportStmt {
                        source: src.trim_matches(&['"', '\''][..]).to_string(),
                        alias: None,
                        line: node.start_position().row + 1,
                    });
                }
                // 直接导出（export function/class）的子节点会通过 DFS 被常规匹配
            }
            _ => {}
        }
    }

    /// tree-sitter 解析失败时的正则降级方案。
    ///
    /// 逐行扫描，使用简单的字符串匹配和正则提取基本信息。
    /// 在 AST 处理不可用时保证基本功能可用。
    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1;
            let t = line.trim();

            // 导入：import ... from '...'
            if let Some(rest) = t.strip_prefix("import ") {
                if let Some(from_pos) = rest.find(" from ") {
                    let src = rest[from_pos + 6..]
                        .trim()
                        .trim_matches(&['"', '\'', ';'][..]);
                    imports.push(ImportStmt {
                        source: src.to_string(),
                        alias: None,
                        line: line_no,
                    });
                }
                continue;
            }

            // 导出（重导出部分）
            if let Some(rest) = t.strip_prefix("export ")
                && let Some(from_pos) = rest.find(" from ")
            {
                let src = rest[from_pos + 6..]
                    .trim()
                    .trim_matches(&['"', '\'', ';'][..]);
                imports.push(ImportStmt {
                    source: src.to_string(),
                    alias: None,
                    line: line_no,
                });
            }

            // 去掉 export 前缀后匹配实体
            let core = t
                .strip_prefix("export ")
                .or_else(|| t.strip_prefix("export default "))
                .or_else(|| t.strip_prefix("export async "))
                .or_else(|| t.strip_prefix("async "))
                .unwrap_or(t);
            let core = core.trim();

            if let Some(name) = core
                .strip_prefix("class ")
                .and_then(|s| s.split(&['{', ' ', '<', '(', ';', '}'][..]).next())
                .map(|s| s.trim())
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
            } else if let Some(name) = core
                .strip_prefix("function ")
                .and_then(|s| s.split(&['(', ' ', '<', '{', ';', '}'][..]).next())
                .map(|s| s.trim())
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
            } else if let Some(name) = core
                .strip_prefix("const ")
                .or_else(|| core.strip_prefix("let "))
                .or_else(|| core.strip_prefix("var "))
                .and_then(|s| s.split(&['=', ':', ' ', ';'][..]).next())
                .map(|s| s.trim())
            {
                // 检查是否为箭头函数
                let is_arrow = core.contains("=>");
                let kind = if is_arrow { "function" } else { "variable" };
                entities.push(Entity {
                    name: name.to_string(),
                    kind: kind.into(),
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

impl LanguageProcessor for JavaScriptProcessor {
    fn name(&self) -> &'static str {
        Self::language()
    }
    fn extensions(&self) -> &[&str] {
        &[".js", ".jsx", ".mjs", ".cjs"]
    }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_js_basics() {
        let source = r#"import { useState } from "react";
import "./style.css";

function greet(name) { return "hello " + name; }
class MyClass { constructor() {} }
const helper = () => 42;
let x = 1;
"#;
        let proc = JavaScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.js")).unwrap();
        // 预期 5 个实体：greet, MyClass, constructor(类内方法), helper, x
        // 注：constructor 是类内方法定义，tree-sitter 会将其作为独立实体检测
        assert_eq!(result.entities.len(), 5);
        assert!(result.entities.iter().any(|e| e.name == "greet"));
        assert!(result.entities.iter().any(|e| e.name == "MyClass"));
        assert!(result.entities.iter().any(|e| e.name == "constructor"));
        assert!(result.entities.iter().any(|e| e.name == "helper"));
        assert!(result.entities.iter().any(|e| e.name == "x"));
        assert_eq!(result.imports[0].source, "react");
        assert_eq!(result.imports[1].source, "./style.css");
    }

    #[test]
    fn test_parse_js_arrow_function_is_function_kind() {
        let source = r#"const add = (a, b) => a + b;
const greet = name => `hello ${name}`;
"#;
        let proc = JavaScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.js")).unwrap();
        let add = result.entities.iter().find(|e| e.name == "add").unwrap();
        assert_eq!(add.kind, "function");
        let greet = result.entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(greet.kind, "function");
    }

    /// P1-15：JS 解构不得产出伪实体名
    #[test]
    fn test_js_destructuring_no_entity() {
        let source = r#"const { a, b } = obj;
const [c, d] = arr;
const fn = (x) => x * 2;
"#;
        let proc = JavaScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.js")).unwrap();
        assert!(
            !result.entities.iter().any(|e| e.name.contains('{')
                || e.name.contains('}')
                || e.name.contains('[')
                || e.name.contains(']')),
            "解构不得产出伪实体名: {:?}",
            result.entities
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "fn" && e.kind == "function"),
            "箭头函数仍归类 function"
        );
    }

    #[test]
    fn test_parse_js_export_reexport() {
        let source = r#"export function sum(a, b) { return a + b; }
export { Component } from "react";
export class Button {}
"#;
        let proc = JavaScriptProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.js")).unwrap();
        // export function / export class → 实体
        assert!(result.entities.iter().any(|e| e.name == "sum"));
        assert!(result.entities.iter().any(|e| e.name == "Button"));
        // export { ... } from → 当作导入
        assert!(result.imports.iter().any(|i| i.source == "react"));
    }
}
