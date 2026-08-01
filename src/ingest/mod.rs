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
    // 扫描产出绝对路径；转换为相对扫描根（== 进程 cwd）的路径——
    // 模块名派生（graph/chunk 的 Normal 组件提取）、搜索索引、指纹记录
    // 全部以相对路径为基准，杜绝绝对路径污染模块名（此前产出
    // RustProjects_repo-wiki_src 这类含机器路径的模块名）。
    // 相对路径下的 IO 相对 cwd 解析，与绝对路径等价。
    let files = scanner
        .scan()?
        .into_iter()
        .map(|f| f.strip_prefix(&root).map(|p| p.to_path_buf()).unwrap_or(f))
        .collect::<Vec<_>>();

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
