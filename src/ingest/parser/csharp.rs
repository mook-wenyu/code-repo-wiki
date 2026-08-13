use anyhow::Result;
use std::path::Path;
use tree_sitter::{Language, Node};

use super::{Entity, FileInsight, ImportStmt, KindRule, LanguageProcessor, SharedProcessor};

/// C# 语言处理器。
///
/// 处理 .cs 文件，使用 tree-sitter-c-sharp 语法解析树。
/// 支持类、结构体、接口、枚举、record、方法、构造器、属性和字段的提取。
pub struct CSharpProcessor;

impl CSharpProcessor {
    /// 创建新的 C# 处理器。
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    /// 获取声明节点的名称（所有 C# 声明都有 name 字段）。
    fn node_name<'a>(node: &tree_sitter::Node, bytes: &'a [u8]) -> Option<&'a str> {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
    }
}

/// kind 映射表（差异点数据化，含原 kind_map 闭包的 class/record→class、
/// struct/interface/enum 一一映射）：const item 保证 &'static 生命周期
const KINDS: &[KindRule] = &[
    KindRule::with_sig("class_declaration", "class", '{'),
    KindRule::with_sig("record_declaration", "class", '{'),
    KindRule::with_sig("struct_declaration", "struct", '{'),
    KindRule::with_sig("interface_declaration", "interface", '{'),
    KindRule::with_sig("enum_declaration", "enum", '{'),
    KindRule::with_sig("method_declaration", "function", '{'),
    KindRule::with_sig("constructor_declaration", "function", '{'),
    KindRule::plain("property_declaration", "property"),
    KindRule::plain("namespace_declaration", "mod"),
    // P3-3：delegate/event 此前未映射——委托类型与事件是 C# 公共 API 的
    // 核心成员（事件驱动代码的结构入口），缺失导致不进文档/图谱。
    // event_declaration（带 accessors 的事件）有 name 字段走表提取；
    // event_field_declaration（简单事件字段）无 name 字段，走 handle_special
    KindRule::with_sig("delegate_declaration", "delegate", '('),
    KindRule::plain("event_declaration", "event"),
];

/// C# 差异点实现：语法常量、kinds 映射表（纯映射分支）、
/// 无法表化的特殊分支（field_declaration 子遍历 / using_directive 文本解析）、
/// 启发式正则 fallback。公共 walk/fallback 触发/FileInsight 组装走 SharedProcessor 默认实现。
impl SharedProcessor for CSharpProcessor {
    fn language() -> &'static str {
        "C#"
    }
    fn grammar() -> Language {
        tree_sitter_c_sharp::LANGUAGE.into()
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
            "event_field_declaration" => {
                // P3-3/F3：简单事件字段（`public event EventHandler Changed;`）的
                // 节点类型，无 name 字段（node-types.json 实证 fields:{}）——
                // record_by_rule 提取不到 name 会静默丢弃。下钻
                // variable_declaration→variable_declarator 逐项取 name，
                // 覆盖多声明事件 `public event EventHandler A, B;`（F3：
                // 文本末词提取只取 B、A 静默丢失）。
                let mut efd_cursor = node.walk();
                for decl in node.children(&mut efd_cursor) {
                    if decl.kind() != "variable_declaration" {
                        continue;
                    }
                    let mut decl_cursor = decl.walk();
                    for child in decl.children(&mut decl_cursor) {
                        if child.kind() != "variable_declarator" {
                            continue;
                        }
                        if let Some(name) = Self::node_name(&child, bytes) {
                            entities.push(Entity {
                                name: name.to_string(),
                                kind: "event".to_string(),
                                line_start: child.start_position().row + 1,
                                line_end: child.end_position().row + 1,
                                doc_comment: None,
                                signature: None,
                                visibility: None,
                            });
                        }
                    }
                }
            }
            "field_declaration" => {
                // t07：tree-sitter-c-sharp 0.23 的普通字段声明是
                // field_declaration → variable_declaration → variable_declarator
                // 三层结构——旧实现只在 field_declaration 的直接子节点里找
                // variable_declarator，中间多出的 variable_declaration 层使
                // **字段恒不提取**（Unity 项目的 SerializeField 字段全部丢失）。
                // 事件字段（public event …）在 0.23 是独立节点
                // event_field_declaration（无 name 字段），由 handle_special
                // 上方分支处理，不走此分支（F4：删除与 node-types 矛盾的
                // is_event 前缀检测死代码）。
                // 改为两层遍历（variable_declaration 下可有多个 declarator，
                // 如 `int a, b;`）；tree-sitter 0.25 无 descendants API，用
                // 显式两层 children 遍历。
                let mut fd_cursor = node.walk();
                for decl in node.children(&mut fd_cursor) {
                    if decl.kind() != "variable_declaration" {
                        continue;
                    }
                    let mut decl_cursor = decl.walk();
                    for child in decl.children(&mut decl_cursor) {
                        if child.kind() != "variable_declarator" {
                            continue;
                        }
                        // 优先取 name 字段；缺失时退化为 declarator 全文
                        //（跨 tree-sitter 版本兼容，字段名是文档价值所在）
                        let name = Self::node_name(&child, bytes)
                            .map(|s| s.to_string())
                            .or_else(|| child.utf8_text(bytes).ok().map(|s| s.trim().to_string()));
                        if let Some(name) = name {
                            entities.push(Entity {
                                name: name.to_string(),
                                kind: "variable".to_string(),
                                line_start: child.start_position().row + 1,
                                line_end: child.end_position().row + 1,
                                doc_comment: None,
                                signature: None,
                                visibility: None,
                            });
                        }
                    }
                }
            }
            "using_directive" => {
                // 使用 node 全文提取命名空间（tree-sitter-c-sharp 的 using_directive 没有 name 字段）
                if let Ok(text) = node.utf8_text(bytes) {
                    let name = text
                        .strip_prefix("using ")
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

    /// tree-sitter 解析失败时的正则降级方案。
    ///
    /// 逐行扫描 C# 源码，使用字符串匹配和启发式识别合法提取关键信息。
    /// 在缺少 tree-sitter-c-sharp 依赖时保证基本功能可用。
    /// 与其他语言不同，C# 的方法/属性识别是启发式规则（tokens 分析 + 关键字排除），
    /// 无法表化为简单前缀规则，故保留为语言内实现。
    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let mut entities = Vec::new();
        let mut imports = Vec::new();
        for (i, line) in source.lines().enumerate() {
            let line_no = i + 1;
            let t = line.trim();

            // using 指令
            if let Some(rest) = t.strip_prefix("using ") {
                let ns = rest.trim_end_matches(';').trim();
                // 排除 using static / using alias = 等高级语法，只匹配基本 using
                if !ns.contains('=') && !ns.starts_with("static ") {
                    imports.push(ImportStmt {
                        source: ns.to_string(),
                        alias: None,
                        line: line_no,
                    });
                }
                continue;
            }

            // namespace 声明
            if let Some(rest) = t.strip_prefix("namespace ") {
                let name = rest.split(&['{', ' ', ';'][..]).next().unwrap_or("").trim();
                if !name.is_empty() {
                    entities.push(Entity {
                        name: name.to_string(),
                        kind: "mod".into(),
                        line_start: line_no,
                        line_end: line_no,
                        doc_comment: None,
                        signature: None,
                        visibility: None,
                    });
                }
                continue;
            }

            // 类型声明：class / struct / interface / enum / record
            for prefix in &["class ", "struct ", "interface ", "enum ", "record "] {
                if let Some(rest) = t.find(prefix).map(|pos| &t[pos..]) {
                    let name = rest
                        .strip_prefix(prefix)
                        .and_then(|s| s.split(&['{', ' ', ':', '<', ';', '(', '}'][..]).next())
                        .map(|s| s.trim());
                    if let Some(name) = name
                        && !name.is_empty()
                        && name
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic() || c == '_')
                            .unwrap_or(false)
                    {
                        let kind = match *prefix {
                            "class " | "record " => "class",
                            "struct " => "struct",
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

            // 方法声明：访问修饰符 + 返回类型 + 方法名 + (
            // 匹配模式：public/private/protected/internal ... Name(...)
            if !entities.iter().any(|e| e.line_start == line_no) {
                let method_candidate = t.split(&['(', '{'][..]).next().unwrap_or("");
                let tokens: Vec<&str> = method_candidate.split_whitespace().collect();
                // 查找可能的返回类型和方法名模式
                if tokens.len() >= 2 {
                    // 最后一个 token 可能是方法名（紧接括号前）
                    if method_candidate.contains('(') {
                        let name_token = tokens.last().unwrap_or(&"");
                        let name = name_token.trim_end_matches('(');
                        if name
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic() || c == '_')
                            .unwrap_or(false)
                            && ![
                                "class",
                                "struct",
                                "interface",
                                "enum",
                                "namespace",
                                "using",
                                "if",
                                "while",
                                "for",
                                "foreach",
                                "switch",
                                "return",
                            ]
                            .contains(&name)
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
                }
            }

            // 属性声明：type Name { get; set; }
            if !entities.iter().any(|e| e.line_start == line_no)
                && t.contains("{")
                && !t.contains("(")
            {
                let tokens: Vec<&str> = t.split_whitespace().collect();
                if tokens.len() >= 2 {
                    let name = tokens
                        .iter()
                        .position(|s| s.contains('{'))
                        .and_then(|pos| tokens.get(pos - 1))
                        .map(|s| s.trim())
                        .filter(|s| {
                            s.chars()
                                .next()
                                .map(|c| c.is_alphabetic() || c == '_')
                                .unwrap_or(false)
                        });
                    if let Some(name) = name {
                        entities.push(Entity {
                            name: name.to_string(),
                            kind: "property".into(),
                            line_start: line_no,
                            line_end: line_no,
                            doc_comment: None,
                            signature: Some(t.to_string()),
                            visibility: None,
                        });
                    }
                }
            }
        }
        (entities, imports)
    }
}

impl LanguageProcessor for CSharpProcessor {
    fn name(&self) -> &'static str {
        Self::language()
    }
    fn extensions(&self) -> &[&str] {
        &[".cs"]
    }

    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight> {
        Self::parse_file(source, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csharp_basics() {
        let source = r#"using System;
using System.Collections.Generic;

namespace MyApp {
    class Player { }
    struct Point { public int X; }
    interface ILogger { void Log(string msg); }
    enum Color { Red, Green, Blue }
}
"#;
        let proc = CSharpProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.cs")).unwrap();
        // using 指令
        assert!(result.imports.iter().any(|i| i.source == "System"));
        assert!(
            result
                .imports
                .iter()
                .any(|i| i.source == "System.Collections.Generic")
        );
        // 实体
        assert!(result.entities.iter().any(|e| e.name == "MyApp"));
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Player" && e.kind == "class")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Point" && e.kind == "struct")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "ILogger" && e.kind == "interface")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Color" && e.kind == "enum")
        );
    }

    #[test]
    fn test_parse_csharp_method_and_property() {
        let source = r#"class Calculator {
    public int Add(int a, int b) { return a + b; }
    public string Name { get; set; }
    private int _count;
}
"#;
        let proc = CSharpProcessor::new().unwrap();
        let result = proc.parse(source, Path::new("test.cs")).unwrap();
        assert!(result.entities.iter().any(|e| e.name == "Calculator"));
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Add" && e.kind == "function")
        );
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == "Name" && e.kind == "property")
        );
        // tree-sitter-c-sharp 0.23 的 variable_declarator AST 结构与当前遍历方式不兼容，
        // 字段析取器名称提取依赖 tree-sitter 版本。当前已验证主要类型（类/方法/属性）均可正确识别。
    }
}

/// t07：Unity 形态覆盖——MonoBehaviour 类/SerializeField 字段/生命周期方法
/// 全部应解析（此前字段提取是已知弱项，见 test_parse_csharp_basics 注释）
#[test]
fn test_parse_csharp_unity_morphology() {
    let source = r#"using UnityEngine;

namespace Test.Unity
{
    public class PlayerController : MonoBehaviour
    {
        [SerializeField]
        private float moveSpeed = 5f;

        [SerializeField]
        private string playerName;

        public int Score { get; private set; }

        void Awake() { this.moveSpeed = 1f; }

        void Start() { this.playerName = "hero"; }

        void Update() { this.Score++; }

        public void Move(Vector3 dir) { }
    }
}
"#;
    let proc = CSharpProcessor::new().unwrap();
    let result = proc
        .parse(source, Path::new("PlayerController.cs"))
        .unwrap();
    // 类（MonoBehaviour 子类作普通 class）
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "PlayerController" && e.kind == "class")
    );
    // SerializeField 字段（私有但序列化——文档应含）
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "moveSpeed" && e.kind == "variable"),
        "SerializeField 字段应解析: {:?}",
        result.entities
    );
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "playerName" && e.kind == "variable")
    );
    // 属性
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "Score" && e.kind == "property")
    );
    // 生命周期方法
    for lifecycle in ["Awake", "Start", "Update", "Move"] {
        assert!(
            result
                .entities
                .iter()
                .any(|e| e.name == lifecycle && e.kind == "function"),
            "方法 {lifecycle} 应解析"
        );
    }
}

/// P3-3：C# delegate/event 此前未映射，补齐映射后应解析为 delegate/event
#[test]
fn test_csharp_delegate_and_event_parsed() {
    let source = r#"public delegate void Handler(int x);
public class C {
    public event EventHandler Changed;
}
"#;
    let proc = CSharpProcessor::new().unwrap();
    let result = proc.parse(source, Path::new("test.cs")).unwrap();
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "Handler" && e.kind == "delegate"),
        "委托类型应解析: {:?}",
        result.entities
    );
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "Changed" && e.kind == "event"),
        "事件应解析: {:?}",
        result.entities
    );
}

/// F3 回归锚：多声明事件 `public event EventHandler A, B;` 的每个
/// 声明名都必须提取（旧文本末词提取只取 B、A 静默丢失）
#[test]
fn test_csharp_multi_declarator_event() {
    let source = r#"public class C {
    public event EventHandler A, B;
}
"#;
    let proc = CSharpProcessor::new().unwrap();
    let result = proc.parse(source, Path::new("test.cs")).unwrap();
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "A" && e.kind == "event"),
        "A 应解析为 event: {:?}",
        result.entities
    );
    assert!(
        result
            .entities
            .iter()
            .any(|e| e.name == "B" && e.kind == "event"),
        "B 应解析为 event: {:?}",
        result.entities
    );
}
