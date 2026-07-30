pub mod scanner;
pub mod parser;

use anyhow::Result;
use crate::config::schema::WikiConfig;
use parser::{FileInsight, ParserRegistry};

/// 执行扫描和解析，返回 FileInsight 列表
///
/// 1. 使用 Scanner 遍历文件系统（应用 .gitignore 和 scope 过滤）
/// 2. 使用 ParserRegistry 根据扩展名分发到对应的 LanguageProcessor
pub fn scan_and_parse(config: &WikiConfig) -> Result<Vec<FileInsight>> {
    let root = std::env::current_dir()?;
    let scanner = scanner::Scanner::new(&root, &config.scope);
    let files = scanner.scan()?;

    let registry = ParserRegistry::new();

    let mut insights = Vec::new();
    for file in &files {
        let processor = match registry.get_for_file(file) {
            Some(p) => p,
            None => continue,
        };
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("跳过非 UTF-8 文件 {}: {}", file.display(), e);
                continue;
            }
        };
        match processor.parse(&source, file) {
            Ok(insight) => insights.push(insight),
            Err(e) => {
                tracing::error!("解析失败 {}: {}", file.display(), e);
            }
        }
    }

    tracing::info!("扫描完成: 共 {} 个文件, 成功解析 {} 个", files.len(), insights.len());
    Ok(insights)
}
