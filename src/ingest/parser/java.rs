use anyhow::Result;
use std::path::Path;
use tree_sitter::{Language, Node};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

pub struct JavaProcessor;

impl JavaProcessor {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// kind 映射表（差异点数据化）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::with_sig("class_declaration", "class", '{'),
    KindRule::with_sig("interface_declaration", "interface", '{'),
    KindRule::with_sig("enum_declaration", "enum", '{'),
    KindRule::with_sig("record_declaration", "class", '{'),
    KindRule::with_sig("method_declaration", "function", '{'),
    // U09：Java 构造器补齐（constructor_declaration 此前未映射，构造器被遗漏）
    KindRule::with_sig("constructor_declaration", "function", '{'),
    // P3-3：@interface（注解类型）此前未映射——注解声明是 Java 公共 API
    // 的一部分（Spring 注解等），缺失导致注解类型不进文档/图谱
    KindRule::with_sig("annotation_type_declaration", "interface", '{'),
];

/// Java 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（field_declaration 子遍历 / import_declaration）、正则 fallback。
/// 公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for JavaProcessor {
    fn language() -> &'static str {
        "Java"
    }
    fn grammar() -> Language {
        tree_sitter_java::LANGUAGE.into()
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
            "field_declaration" => {
                let mut cur = node.walk();
                if cur.goto_first_child() {
                    loop {
                        if cur.node().kind() == "variable_declarator"
                            && let Some(name) = cur
                                .node()
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(bytes).ok())
                        {
                            entities.push(Entity {
                                name: name.to_string(),
                                kind: "variable".to_string(),
                                line_start: node.start_position().row + 1,
                                line_end: node.end_position().row + 1,
                                doc_comment: None,
                                signature: None,
                                visibility: None,
                            });
                        }
                        if !cur.goto_next_sibling() {
                            break;
                        }
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
    }

    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1;
            let t = line.trim();

            if let Some(rest) = t.strip_prefix("import ") {
                let path = rest.trim_end_matches(';').trim();
                imports.push(ImportStmt {
                    source: path.to_string(),
                    alias: None,
                    line: line_no,
                });
                continue;
            }

            for prefix in &["class ", "interface ", "enum ", "record "] {
                if let Some(rest) = t.find(prefix).map(|pos| &t[pos..]) {
                    let name = rest
                        .strip_prefix(prefix)
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
                            name: name.to_string(),
                            kind: kind.to_string(),
                            line_start: line_no,
                            line_end: line_no,
                            doc_comment: None,
                            signature: Some(t.to_string()),
                            visibility: None,
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
    fn name(&self) -> &'static str {
        Self::language()
    }
    fn extensions(&self) -> &[&str] {
        &[".java"]
    }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
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
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Calculator" && e.kind == "class")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Runnable" && e.kind == "interface")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "add" && e.kind == "function")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "count" && e.kind == "variable")
        );
        assert!(result.imports.iter().any(|i| i.source == "java.util.List"));
        assert!(result.imports.iter().any(|i| i.source == "java.io.File"));
    }

    /// U09：Java 构造器补齐（constructor_declaration 此前未映射）
    #[test]
    fn test_parse_java_constructor() {
        let source = r#"public class Calculator {
    public Calculator() { this(0); }
    public Calculator(int seed) { this.seed = seed; }
    private int seed;
}
"#;
        let proc = JavaProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("Test.java")).unwrap();
        let ctors: Vec<&str> = result
            .entities
            .iter()
            .filter(|e| e.name == "Calculator" && e.kind == "function")
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            !ctors.is_empty(),
            "构造器应解析为 function: {:?}",
            result.entities
        );
    }

    /// P3-3：Java @interface（注解类型）此前未映射，补齐映射后应解析为 interface
    #[test]
    fn test_java_annotation_type_parsed() {
        let source = r#"@interface MyAnno {
    String value();
}
"#;
        let proc = JavaProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("MyAnno.java")).unwrap();
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "MyAnno" && e.kind == "interface"),
            "注解类型应解析: {:?}",
            result.entities
        );
    }
}
