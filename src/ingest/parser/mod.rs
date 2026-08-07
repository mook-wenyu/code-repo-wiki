mod rust;
mod typescript;
mod python;
mod go;
mod javascript;
mod csharp;
mod java;

use std::path::{Path, PathBuf};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

/// 全部注册语言处理器的扩展名集合（含前导点，与各处理器 extensions() 逐项一致）。
/// 扫描器与文件监听共用此集合：只收可解析语言，非支持语言一律不进入管线。
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    ".rs", ".ts", ".tsx", ".py", ".go", ".js", ".jsx", ".mjs", ".cjs", ".cs", ".java",
];

/// 解析后的文件洞察，包含所有提取的实体和导入信息
///
/// Serialize/Deserialize 供增量管线的解析缓存使用（.state/insights_cache.json）：
/// 未变更文件直接反序列化复用，避免重复 tree-sitter 解析。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInsight {
    pub path: PathBuf,
    pub language: String,
    pub entities: Vec<Entity>,
    pub imports: Vec<ImportStmt>,
    pub doc_comments: Vec<String>,
    /// 文件的源代码文本，用于避免搜索索引构建时重复读盘
    pub source: String,
}

/// 代码实体（struct / fn / trait / impl / enum / type / const / 等）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    /// 实体类型："struct", "fn", "trait", "impl", "enum", "type", "const", "mod", "class", "interface", "function"
    pub kind: String,
    /// 起始行号（1-based）
    pub line_start: usize,
    /// 结束行号（1-based）
    pub line_end: usize,
    pub doc_comment: Option<String>,
    pub signature: Option<String>,
    // 实体摘要字段已删除（v31）：原 generate_entity_summaries 对每实体一次
    // LLM 调用但字段零消费者——纯 token 浪费；未来如需实体级语义索引，
    // 应在生成时预索引重建，而非逐个惰性调用。
    /// 可见性修饰符（"pub"/"pub(crate)"/"private"/"internal"/"export" 等）；
    /// 由解析出口的 fill_visibilities 按行级文本统一提取，缺失（默认可见性）
    /// 为 None。serde(default) 兼容旧版 insights_cache 反序列化。
    #[serde(default)]
    pub visibility: Option<String>,
}

/// 导入语句
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportStmt {
    /// 导入的源模块（如 "std::collections::HashMap"）
    pub source: String,
    /// 别名（如 use Foo as Bar 中的 "Bar"）
    pub alias: Option<String>,
    /// 导入语句所在行号（1-based）
    pub line: usize,
}

/// 语言处理器 trait — 每个语言实现此 trait
///
/// 必须 Send + Sync 以支持并行解析
pub trait LanguageProcessor: Send + Sync {
    fn name(&self) -> &'static str;
    fn extensions(&self) -> &[&str];
    fn parse(&self, source: &str, path: &Path) -> Result<FileInsight>;
}

/// kind 映射规则：tree-sitter 节点 kind → 实体 kind（语言差异点数据化）
///
/// 覆盖 7 语言 walk 中"纯 kind→kind 映射 + 可选签名提取"的分支，
/// 由 `SharedProcessor::record_by_rule` 统一提取，输出与各语言原 match 分支逐字节一致。
#[derive(Debug, Clone, Copy)]
pub struct KindRule {
    /// tree-sitter 节点 kind（如 "function_declaration"）
    pub node_kind: &'static str,
    /// 映射后的实体 kind（如 "function"）
    pub entity_kind: &'static str,
    /// 是否提取签名：取节点文本中首个分隔符前的部分
    pub with_signature: bool,
    /// 签名截断分隔符（Rust/Go/Java/C#/JS/TS 用 '{'，Python 用 ':'）
    pub sig_delim: char,
}

impl KindRule {
    /// 无签名提取的映射规则
    pub const fn plain(node_kind: &'static str, entity_kind: &'static str) -> Self {
        Self { node_kind, entity_kind, with_signature: false, sig_delim: '{' }
    }
    /// 带签名提取的映射规则（sig_delim 为签名截断分隔符）
    pub const fn with_sig(node_kind: &'static str, entity_kind: &'static str, sig_delim: char) -> Self {
        Self { node_kind, entity_kind, with_signature: true, sig_delim }
    }
}

/// 共享的 tree-sitter 解析骨架 — 7 语言 walk/fallback 公共部分去重
///
/// 背景：原 7 个 parser 的 walk 头部（bytes/entities/imports 初始化、
/// Parser 构造、set_language 失败→fallback、parse 返回 None→fallback、cursor 初始化）、
/// walk 尾部（DFS 遍历）与 parse 方法体（FileInsight 组装）逐字重复约 20 行 × 7，
/// 全部收敛到本 trait 的默认实现，每语言只保留差异点。
///
/// 差异点分三类承载：
/// 1. grammar()：tree-sitter 语法常量（原 LANGUAGE 一行差异）
/// 2. kinds()：纯 kind→kind 映射分支数据化（含签名提取规则），
///    公共 walk 命中后走统一的 record_by_rule；命中规则与 handle_special
///    的分支互斥（等价于原 match 每个 kind 恰好一个分支）
/// 3. handle_special()：无法数据化的分支钩子 — 动态 kind 判断
///    （Go type_spec、JS variable_declarator 箭头函数）、子节点遍历
///    （field_declaration）、导入语句文本解析（use_declaration / import_* / using_directive）
///
/// fallback() 保持每语言钩子：各语言正则降级差异极大（C# 是启发式代码、
/// 导入解析规则各异），强行表化会引入比原代码更复杂的规则引擎，故不数据化；
/// 统一的是触发契约 — set_language 失败或 parse 返回 None 时由 extract()
/// 统一调用 fallback()，保证降级路径行为一致。
///
/// post_process()：walk 结束后的文档注释关联钩子
/// （Rust 的 /// 注释、Python 的 docstring），其余语言无操作。
pub trait SharedProcessor: Sized {
    /// 语言名，用于 name() 与 FileInsight.language（与现状一致，如 "Go"）
    fn language() -> &'static str;
    /// tree-sitter 语法常量（语言差异点）
    fn grammar() -> Language;
    /// kind 映射表（差异点数据化）：命中的节点由 record_by_rule 统一提取
    fn kinds() -> &'static [KindRule];
    /// 无法数据化的节点处理钩子（动态 kind / 子节点遍历 / 导入解析）
    fn handle_special(node: Node, bytes: &[u8], entities: &mut Vec<Entity>, imports: &mut Vec<ImportStmt>);
    /// tree-sitter 失败时的正则降级（差异点钩子，触发契约由 extract 统一）
    fn fallback(source: &str) -> (Vec<Entity>, Vec<ImportStmt>);
    /// walk 完成后的文档关联钩子（默认无操作）
    fn post_process(_source: &str, _entities: &mut Vec<Entity>) {}

    /// 统一 walk 入口：构造 parser → tree-sitter 失败触发 fallback → DFS 遍历
    ///
    /// 骨架与原各语言 walk 逐字一致：命中 kinds() 表走 record_by_rule，
    /// 未命中走 handle_special（等价于原 match 分支的聚合）。
    fn extract(source: &str) -> (Vec<Entity>, Vec<ImportStmt>) {
        let bytes = source.as_bytes();
        let mut entities = Vec::new();
        let mut imports = Vec::new();

        let mut parser = Parser::new();
        if parser.set_language(&Self::grammar()).is_err() {
            let (mut e, i) = Self::fallback(source);
            fill_visibilities(source, &mut e);
            return (e, i);
        }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Self::fallback(source),
        };

        let mut cursor = tree.walk();
        if !cursor.goto_first_child() { return (entities, imports); }

        'walk: loop {
            let node = cursor.node();
            match Self::kinds().iter().find(|r| r.node_kind == node.kind()) {
                Some(rule) => Self::record_by_rule(node, bytes, rule, &mut entities),
                None => Self::handle_special(node, bytes, &mut entities, &mut imports),
            }
            if cursor.goto_first_child() { continue; }
            loop {
                if cursor.goto_next_sibling() { continue 'walk; }
                if !cursor.goto_parent() { break 'walk; }
            }
        }

        Self::post_process(source, &mut entities);
        fill_visibilities(source, &mut entities);
        (entities, imports)
    }

    /// 按 kinds() 规则统一提取实体（与各语言原 match 分支输出一致）
    fn record_by_rule(node: Node, bytes: &[u8], rule: &KindRule, entities: &mut Vec<Entity>) {
        if let Some(name) = node.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok()) {
            let sig = if rule.with_signature {
                node.utf8_text(bytes).ok()
                    .and_then(|t| t.split(rule.sig_delim).next().map(|s| s.trim().to_string()))
            } else { None };
            entities.push(Entity {
                name: name.to_string(), kind: rule.entity_kind.to_string(),
                line_start: node.start_position().row + 1,                line_end: node.end_position().row + 1,
                doc_comment: None, signature: sig, visibility: None,
            });
        }
    }

    /// 统一 FileInsight 组装（empty 早退 + extract），语言侧 parse 一行调用
    fn parse_file(source: &str, path: &Path) -> Result<FileInsight> {        let language = Self::language();
        if source.is_empty() {
            return Ok(FileInsight { path: path.to_path_buf(), language: language.into(), entities: vec![], imports: vec![], doc_comments: vec![], source: source.to_string() });
        }
        let (entities, imports) = Self::extract(source);
        Ok(FileInsight { path: path.to_path_buf(), language: language.into(), entities, imports, doc_comments: vec![], source: source.to_string() })
    }
}

/// 从源码文本统一提取实体可见性（解析出口调用，覆盖 record_by_rule /
/// handle_special / fallback 三条产出路径）
///
/// 可见性不在 tree-sitter 节点字段中（各语言语法差异），统一按行级文本
/// 提取：从实体起始行向上回溯，跳过属性宏行（Rust `#[...]`、C# `[...]`）
/// 与空行，取首个「修饰符 token」——命中显式可见性声明（Rust pub 系 /
/// C# Java private·protected·internal / TS JS export）即返回原文；
/// 未命中（默认可见性，如 Python 无修饰符、Go 大写导出语义）返回 None，
/// api.md 标注省略。实体起始行即声明行（多行签名首行含可见性），
/// 回溯只在属性宏场景发生（如 `#[derive]` 前置的 pub struct），
/// 遇到任何非属性行即停止，不会扫到文件头。
fn fill_visibilities(source: &str, entities: &mut Vec<Entity>) {
    let lines: Vec<&str> = source.lines().collect();
    for e in entities {
        if e.visibility.is_some() {
            continue;
        }
        let mut i = e.line_start.saturating_sub(1);
        while let Some(line) = lines.get(i) {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
                if i == 0 {
                    break;
                }
                i -= 1;
                continue;
            }
            let token = t.split_whitespace().next().unwrap_or("");
            e.visibility = match token {
                "pub" | "pub(crate)" | "pub(super)" | "private" | "protected" | "internal" | "export" => {
                    Some(token.to_string())
                }
                _ => None,
            };
            break;
        }
    }
}

/// 解析器注册表 — 管理所有内置语言处理器
pub struct ParserRegistry {
    parsers: Vec<Box<dyn LanguageProcessor>>,
}

impl ParserRegistry {
    /// 创建注册表并注册所有内置处理器
    pub fn new() -> Self {
        let mut reg = Self { parsers: Vec::new() };
        reg.register(Box::new(rust::RustProcessor::new().unwrap()));
        reg.register(Box::new(typescript::TypeScriptProcessor::new().unwrap()));
        reg.register(Box::new(python::PythonProcessor::new().unwrap()));
        reg.register(Box::new(go::GoProcessor::new().unwrap()));
        reg.register(Box::new(javascript::JavaScriptProcessor::new().unwrap()));
        reg.register(Box::new(csharp::CSharpProcessor::new().unwrap()));
        reg.register(Box::new(java::JavaProcessor::new().unwrap()));
        reg
    }

    /// 注册自定义处理器
    pub fn register(&mut self, parser: Box<dyn LanguageProcessor>) {
        self.parsers.push(parser);
    }

    /// 根据文件路径查找对应的处理器
    pub fn get_for_file(&self, path: &Path) -> Option<&dyn LanguageProcessor> {
        let ext = path.extension()?.to_str()?;
        let ext_str = format!(".{}", ext);
        self.parsers.iter().find(|p| p.extensions().contains(&ext_str.as_str())).map(|b| b.as_ref())
    }
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(name: &str, start: usize) -> Entity {
        Entity {
            name: name.into(),
            kind: "function".into(),
            line_start: start,
            line_end: start,
            doc_comment: None,
            signature: None,
            visibility: None,
        }
    }

    #[test]
    fn test_fill_visibilities_extracts_modifiers() {
        // 显式修饰符：Rust pub / C# private 按行首 token 提取
        let src = "pub fn a() {}\n\nprivate int x;\n";
        let mut es = vec![entity("a", 1), entity("x", 3)];
        fill_visibilities(src, &mut es);
        assert_eq!(es[0].visibility.as_deref(), Some("pub"));
        assert_eq!(es[1].visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_fill_visibilities_skips_attribute_lines() {
        // 属性宏行（Rust #[derive] / C# [SerializeField]）不携带可见性，
        // 回溯到其上方的 pub / private 声明行
        let src = "#[derive(Debug)]\npub struct Foo;\n\n[SerializeField]\nprivate float speed;\n";
        let mut es = vec![entity("Foo", 2), entity("speed", 5)];
        fill_visibilities(src, &mut es);
        assert_eq!(es[0].visibility.as_deref(), Some("pub"));
        assert_eq!(es[1].visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_fill_visibilities_none_without_modifier() {
        // 无修饰符语言（Python 默认可见性 / Go 大写导出语义）→ None，
        // api.md 渲染省略可见性标注
        let src = "def run():\n    pass\n\nfunc Run() {}\n";
        let mut es = vec![entity("run", 1), entity("Run", 4)];
        fill_visibilities(src, &mut es);
        assert!(es[0].visibility.is_none());
        assert!(es[1].visibility.is_none());
    }

    #[test]
    fn test_fill_visibilities_keeps_pub_crate_variant() {
        // pub(crate)/pub(super) 为完整 token，原样保留
        let src = "pub(crate) fn internal() {}\npub(super) fn child() {}\n";
        let mut es = vec![entity("internal", 1), entity("child", 2)];
        fill_visibilities(src, &mut es);
        assert_eq!(es[0].visibility.as_deref(), Some("pub(crate)"));
        assert_eq!(es[1].visibility.as_deref(), Some("pub(super)"));
    }
}
