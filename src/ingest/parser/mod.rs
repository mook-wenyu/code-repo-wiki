mod rust;
mod typescript;
mod python;
mod go;
mod javascript;
mod csharp;
mod java;

use std::path::{Path, PathBuf};
use anyhow::Result;

/// 解析后的文件洞察，包含所有提取的实体和导入信息
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
    pub summary: Option<String>,
}

/// 导入语句
#[derive(Debug, Clone)]
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

    /// 返回所有支持的扩展名列表（含点号前缀）
    pub fn supported_extensions(&self) -> Vec<String> {
        let mut exts = Vec::new();
        for p in &self.parsers {
            for e in p.extensions() {
                exts.push(e.to_string());
            }
        }
        exts
    }
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}
