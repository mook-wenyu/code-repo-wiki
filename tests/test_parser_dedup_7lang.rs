//! 航图票 14 验收测试：7 语言 parser 去重后行为保持
//!
//! 覆盖三组断言：
//! 1. 各语言"实体 + 导入"双断言（与各语言内嵌测试同形状，验证重构后公共骨架行为不变）
//! 2. 各语言损坏源码（未闭合括号等）→ 不 panic、产出实体、同一实体不出现两次
//! 3. 混合仓库：临时目录含 7 种语言文件，直接调 ParserRegistry.parse，各语言实体 > 0
//!
//! 说明：tree-sitter 是容错解析器，语法损坏时通常仍产出含 ERROR 节点的树
//! （parse 返回 None / set_language 失败才会走正则 fallback，二者无法经公共 API 人为构造）。
//! 因此第 2 组断言守护的是统一 fallback 触发契约的底线：无论解析走 tree-sitter
//! 容错路径还是正则 fallback 路径，结果必须不 panic、有实体、无重复。

use std::path::Path;

use repo_wiki::ingest::parser::{Entity, ParserRegistry};

/// 断言实体列表无重复（name + kind + 起始行 三元组唯一）
fn assert_no_duplicates(entities: &[Entity]) {
    let mut seen = std::collections::HashSet::new();
    for e in entities {
        let key = (e.name.as_str(), e.kind.as_str(), e.line_start);
        assert!(
            seen.insert(key),
            "同一实体出现两次: {} (kind={}, line={})",
            e.name, e.kind, e.line_start
        );
    }
}

/// 通过注册表解析源码并返回 FileInsight
fn parse_src(language: &str, source: &str, path: &str) -> repo_wiki::ingest::parser::FileInsight {
    let registry = ParserRegistry::new();
    let processor = registry
        .get_for_file(Path::new(path))
        .unwrap_or_else(|| panic!("{language} 处理器未注册"));
    processor.parse(source, Path::new(path)).unwrap()
}

// ==================== 1. 实体 + 导入 双断言（7 语言） ====================

#[test]
fn test_rust_entity_and_import() {
    let insight = parse_src("Rust", "use std::collections::HashMap;\nstruct Point { x: i32 }\nfn add(a: i32, b: i32) -> i32 { a + b }\n", "main.rs");
    assert!(insight.entities.iter().any(|e| e.name == "Point" && e.kind == "struct"));
    assert!(insight.entities.iter().any(|e| e.name == "add" && e.kind == "function"));
    assert!(insight.imports.iter().any(|i| i.source == "std::collections::HashMap"));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_python_entity_and_import() {
    let insight = parse_src("Python", "import os\nfrom typing import List\nclass Person:\n    pass\n\ndef greet(name: str) -> str:\n    return name\n", "app.py");
    assert!(insight.entities.iter().any(|e| e.name == "Person" && e.kind == "class"));
    assert!(insight.entities.iter().any(|e| e.name == "greet" && e.kind == "function"));
    assert!(insight.imports.iter().any(|i| i.source == "os"));
    assert!(insight.imports.iter().any(|i| i.source.starts_with("from typing")));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_go_entity_and_import() {
    let insight = parse_src("Go", "package main\nimport \"fmt\"\ntype Point struct { X int }\nfunc add(a int, b int) int { return a + b }\n", "main.go");
    assert!(insight.entities.iter().any(|e| e.name == "Point" && e.kind == "struct"));
    assert!(insight.entities.iter().any(|e| e.name == "add" && e.kind == "function"));
    assert!(insight.imports.iter().any(|i| i.source == "fmt"));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_java_entity_and_import() {
    let insight = parse_src("Java", "import java.util.List;\nclass Calculator {\n    public int add(int a, int b) { return a + b; }\n}\n", "Main.java");
    assert!(insight.entities.iter().any(|e| e.name == "Calculator" && e.kind == "class"));
    assert!(insight.entities.iter().any(|e| e.name == "add" && e.kind == "function"));
    assert!(insight.imports.iter().any(|i| i.source == "java.util.List"));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_csharp_entity_and_import() {
    let insight = parse_src("C#", "using System;\nnamespace App {\n    class Player { }\n}\n", "Program.cs");
    assert!(insight.entities.iter().any(|e| e.name == "App" && e.kind == "mod"));
    assert!(insight.entities.iter().any(|e| e.name == "Player" && e.kind == "class"));
    assert!(insight.imports.iter().any(|i| i.source == "System"));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_javascript_entity_and_import() {
    let insight = parse_src("JavaScript", "import { useState } from \"react\";\nfunction greet(name) { return name; }\nclass Foo {}\n", "app.js");
    assert!(insight.entities.iter().any(|e| e.name == "greet" && e.kind == "function"));
    assert!(insight.entities.iter().any(|e| e.name == "Foo" && e.kind == "class"));
    assert!(insight.imports.iter().any(|i| i.source == "react"));
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_typescript_entity_and_import() {
    let insight = parse_src("TypeScript", "import { Component } from \"react\";\ninterface Props { name: string; }\nfunction greet(name: string): string { return name; }\n", "app.ts");
    assert!(insight.entities.iter().any(|e| e.name == "Props" && e.kind == "interface"));
    assert!(insight.entities.iter().any(|e| e.name == "greet" && e.kind == "function"));
    assert!(insight.imports.iter().any(|i| i.source == "react"));
    assert_no_duplicates(&insight.entities);
}

// ==================== 2. 损坏源码：不 panic、产出实体、无重复（7 语言） ====================

#[test]
fn test_rust_broken_source_no_panic_no_dup() {
    let insight = parse_src("Rust", "struct Point { x: i32 }\nfn broken(a: i32, {", "broken.rs");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_python_broken_source_no_panic_no_dup() {
    let insight = parse_src("Python", "class Person:\n    pass\n\ndef broken(a, b:\n", "broken.py");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_go_broken_source_no_panic_no_dup() {
    let insight = parse_src("Go", "type Point struct { X int }\nfunc broken(a int, {", "broken.go");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_java_broken_source_no_panic_no_dup() {
    let insight = parse_src("Java", "class Broken {\n    public void m(int a, { }\n", "Broken.java");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_csharp_broken_source_no_panic_no_dup() {
    let insight = parse_src("C#", "class Broken {\n    public void M(int a, { }\n", "Broken.cs");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_javascript_broken_source_no_panic_no_dup() {
    let insight = parse_src("JavaScript", "class Foo {}\nfunction broken(a, { }", "broken.js");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

#[test]
fn test_typescript_broken_source_no_panic_no_dup() {
    let insight = parse_src("TypeScript", "interface Props { name: string; }\nfunction broken(a: string, { }", "broken.ts");
    assert!(!insight.entities.is_empty(), "损坏源码应产出至少一个实体");
    assert_no_duplicates(&insight.entities);
}

// ==================== 3. 混合仓库：7 语言各实体 > 0 ====================

#[test]
fn test_mixed_repo_all_languages_have_entities() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_dedup_7lang_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let files: [(&str, &str); 7] = [
        ("main.rs", "struct Point { x: i32 }\nfn add(a: i32, b: i32) -> i32 { a + b }\nuse std::collections::HashMap;\n"),
        ("app.py", "import os\nclass Person:\n    pass\n\ndef greet(name):\n    return name\n"),
        ("main.go", "package main\nimport \"fmt\"\ntype Point struct { X int }\nfunc add(a int, b int) int { return a + b }\n"),
        ("Main.java", "import java.util.List;\nclass Calc {\n    public int add(int a, int b) { return a + b; }\n}\n"),
        ("Program.cs", "using System;\nnamespace App {\n    class Program {\n        public void Run() { }\n    }\n}\n"),
        ("app.js", "import { x } from \"mod\";\nfunction greet(name) { return name; }\nclass Foo {}\n"),
        ("app.ts", "import { Component } from \"react\";\ninterface Props { name: string; }\nfunction greet(name: string): string { return name; }\n"),
    ];
    for (name, content) in &files {
        std::fs::write(dir.join(name), content).unwrap();
    }

    let registry = ParserRegistry::new();
    let mut parsed = 0;
    for (name, _) in &files {
        let file = dir.join(name);
        let processor = registry.get_for_file(&file).expect("注册表应含全部语言处理器");
        let source = std::fs::read_to_string(&file).unwrap();
        let insight = processor.parse(&source, &file).unwrap();
        assert!(!insight.entities.is_empty(), "{name} 应产出至少一个实体");
        assert_no_duplicates(&insight.entities);
        parsed += 1;
    }
    assert_eq!(parsed, 7, "应成功解析全部 7 种语言文件");

    let _ = std::fs::remove_dir_all(&dir);
}
