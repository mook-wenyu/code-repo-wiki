//! 数据库 Schema 文档生成
//!
//! 收集项目内的 .sql 文件，切分 CREATE TABLE 语句块，
//! 交给 LLM 生成表结构文档（含 Mermaid erDiagram）。

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::generate::llm::{LlmProvider, Provider};
use crate::generate::prompt;
use crate::model::{DocumentKind, WikiDocument};
use crate::project::ProjectRoot;

/// 切分 SQL 中的 CREATE TABLE 语句块（仅定位，不解析）
///
/// 从行首匹配 "CREATE TABLE"（大小写不敏感，天然支持 IF NOT EXISTS 与带引号表名），
/// 收集到第一个分号所在行结束。分号出现在字符串字面量内时不做识别（接受误切）。
pub fn extract_create_table_blocks(sql: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0usize;

    for line in sql.split('\n') {
        if start.is_none()
            && line
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE TABLE")
        {
            start = Some(offset);
        }
        if let Some(s) = start
            && line.contains(';')
        {
            blocks.push(&sql[s..offset + line.len()]);
            start = None;
        }
        offset += line.len() + 1;
    }

    blocks
}

/// 在指定项目根下收集 .sql 文件（独立全量遍历：SQL 建模是附加能力，
/// 不经 scanner 的支持语言过滤——scanner 只产出可解析语言文件）
pub fn collect_sql_files_at(root: &ProjectRoot) -> Result<Vec<PathBuf>> {
    use crate::ingest::scanner::{NOISE_DIRS, ROOT_ONLY_NOISE_DIRS};
    let mut out = Vec::new();
    // 栈元素 (目录, Normal 段深度)：深度 0 = 仓库根自身，1 = 直接子目录
    let mut stack = vec![(root.path().to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // 噪音目录整棵跳过（与 scanner 同一边界，F5）：
                // 任意深度清单直接命中；根级清单（dist/build/out/bin/obj）
                // 仅深度 1（根的直接子目录）命中——src/bin 合法源码目录不误杀
                let name = entry.file_name().to_string_lossy().into_owned();
                if NOISE_DIRS.contains(&name.as_str())
                    || (depth == 0 && ROOT_ONLY_NOISE_DIRS.contains(&name.as_str()))
                    || name.starts_with('.')
                {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 在指定项目根下并行生成所有 .sql 文件的 Schema 文档
///
/// root 注入链路：generate_schema_documents_at → collect_sql_files_at，
/// SQL 扫描基准全程显式传递，与进程 cwd 解耦。
pub async fn generate_schema_documents_at(
    root: &ProjectRoot,
    provider: &Provider,
    config: &WikiConfig,
) -> Result<Vec<WikiDocument>> {
    let files = collect_sql_files_at(root)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let semaphore = tokio::sync::Semaphore::new(crate::config::schema::llm_effective_concurrency(
        &config.llm,
    ));
    let mut handles = Vec::with_capacity(files.len());
    for file in files {
        // 每次迭代重新绑定引用，async move 只移入引用与文件路径
        let semaphore = &semaphore;
        handles.push(async move {
            let _permit = semaphore
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("信号量已关闭"))?;
            generate_schema_document(provider, &file, config).await
        });
    }

    let results = futures::future::join_all(handles).await;
    let mut documents = Vec::new();
    for result in results {
        match result {
            Ok(Some(doc)) => documents.push(doc),
            Ok(None) => {}
            Err(e) => tracing::warn!("Schema 文档生成跳过: {}", e),
        }
    }
    Ok(documents)
}

/// 为单个 SQL 文件生成 Schema 文档
///
/// 文件不含 CREATE TABLE 语句时返回 None（不调用 LLM）。
///
/// U03/D1：Schema 文档是 prompt 中唯一强制输出 Mermaid erDiagram 的文档
/// 类型（prompt.rs schema_doc_prompt），此前直接 complete 绕过校验——
/// 坏图直接落盘。现接入 `complete_with_mermaid_guard_free`：坏块重试
/// （注入错误反馈），耗尽后降级为 text 块（与架构/概览一致）。
async fn generate_schema_document<P: LlmProvider>(
    provider: &P,
    path: &Path,
    config: &WikiConfig,
) -> Result<Option<WikiDocument>> {
    let sql = tokio::fs::read_to_string(path).await?;
    let blocks = extract_create_table_blocks(&sql);
    if blocks.is_empty() {
        return Ok(None);
    }

    let messages = prompt::schema_doc_prompt(path, &blocks, &config.wiki.language);
    let content = crate::generate::wiki::complete_with_mermaid_guard_free(
        provider,
        messages,
        "Schema 文档",
        None,
    )
    .await?;
    Ok(Some(WikiDocument {
        title: format!("Database Schema: {}", path.display()),
        kind: DocumentKind::DatabaseSchema,
        content,
        language: config.wiki.language.clone(),
        module_path: vec![],
        references: vec![],
        parent: String::new(),
        last_updated: chrono::Utc::now().to_rfc3339(),
        // Schema 文档由 SQL 渲染（非 LLM 页），不带 git 基线行
        based_on_commit: None,
        fingerprint: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::llm::MockProvider;

    #[test]
    fn test_extract_create_table_blocks() {
        let sql = r#"
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY
);

INSERT INTO seed (id) VALUES (1);

CREATE TABLE "quoted table" (
    id INTEGER
);
"#;
        let blocks = extract_create_table_blocks(sql);
        assert_eq!(blocks.len(), 3);
        // 语句块从 CREATE TABLE 起到分号止
        assert!(blocks[0].starts_with("CREATE TABLE users"));
        assert!(blocks[0].ends_with(';'));
        assert!(blocks[0].contains("id INTEGER PRIMARY KEY"));
        // IF NOT EXISTS 变体
        assert!(blocks[1].starts_with("CREATE TABLE IF NOT EXISTS orders"));
        // 带引号表名
        assert!(blocks[2].starts_with("CREATE TABLE \"quoted table\""));
    }

    #[test]
    fn test_extract_create_table_blocks_case_insensitive() {
        let sql = "create table foo (\n  id int\n);\n";
        let blocks = extract_create_table_blocks(sql);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].starts_with("create table foo"));
    }

    #[test]
    fn test_extract_create_table_blocks_no_match() {
        assert!(extract_create_table_blocks("SELECT 1;").is_empty());
        assert!(extract_create_table_blocks("").is_empty());
        // 无分号结尾的块不返回
        assert!(extract_create_table_blocks("CREATE TABLE foo (\n  id int\n").is_empty());
    }

    #[tokio::test]
    async fn test_generate_schema_document_with_mock() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_test_schema_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sql_path = dir.join("001_init.sql");
        std::fs::write(&sql_path, "CREATE TABLE users (id INTEGER PRIMARY KEY);\n").unwrap();

        let config = WikiConfig::default();
        let provider = Provider::Mock(MockProvider::new());
        let doc = generate_schema_document(&provider, &sql_path, &config)
            .await
            .unwrap()
            .expect("应生成 Schema 文档");

        assert_eq!(
            doc.title,
            format!("Database Schema: {}", sql_path.display())
        );
        assert_eq!(doc.kind, DocumentKind::DatabaseSchema);
        assert_eq!(doc.language, "zh");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_generate_schema_document_skips_without_create_table() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_schema_empty_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sql_path = dir.join("seed.sql");
        std::fs::write(&sql_path, "INSERT INTO seed (id) VALUES (1);\n").unwrap();

        let config = WikiConfig::default();
        let provider = Provider::Mock(MockProvider::new());
        // 无建表语句的文件不生成文档，也不调用 LLM
        let doc = generate_schema_document(&provider, &sql_path, &config)
            .await
            .unwrap();
        assert!(doc.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// D1 防回归：Schema 文档（唯一强制 erDiagram 的文档类型）接入 mermaid
/// 校验-重试-降级——坏图重试耗尽后降级为 text 块，页面照常产出
#[tokio::test]
async fn test_schema_document_degrades_bad_mermaid() {
    use crate::generate::llm::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 恒输出坏 mermaid 的 provider（未闭合标签，merman 报 Unterminated）
    struct BadMermaidProvider {
        calls: AtomicUsize,
    }
    impl LlmProvider for BadMermaidProvider {
        async fn complete(&self, _messages: &[Message]) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("```mermaid\nerDiagram\nA[hello world\n```\n".to_string())
        }
    }

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_schema_d1_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sql_path = dir.join("001_init.sql");
    std::fs::write(&sql_path, "CREATE TABLE users (id INTEGER PRIMARY KEY);\n").unwrap();

    let config = WikiConfig::default();
    let provider = BadMermaidProvider {
        calls: AtomicUsize::new(0),
    };
    let doc = generate_schema_document(&provider, &sql_path, &config)
        .await
        .unwrap()
        .expect("坏图重试耗尽应降级而非失败");
    assert!(
        !doc.content.contains("```mermaid"),
        "坏图不应再以 mermaid 块出现"
    );
    assert!(doc.content.contains("```text"), "坏块应降级为 text fence");
    assert!(
        doc.content.contains("code-repo-wiki: mermaid parse failed"),
        "应含降级标记注释"
    );
    assert_eq!(
        provider.calls.load(Ordering::Relaxed),
        crate::output::mermaid_check::MERMAID_RETRY_MAX + 1,
        "应调用 MERMAID_RETRY_MAX+1 次后降级"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// D1：好 mermaid（erDiagram 合法）直通，不触发重试
#[tokio::test]
async fn test_schema_document_passes_good_mermaid() {
    use crate::generate::llm::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct GoodMermaidProvider {
        calls: AtomicUsize,
    }
    impl LlmProvider for GoodMermaidProvider {
        async fn complete(&self, _messages: &[Message]) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("```mermaid\nerDiagram\nUSERS ||--o{ ORDERS : has\n```\n".to_string())
        }
    }

    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_schema_good_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sql_path = dir.join("001_init.sql");
    std::fs::write(&sql_path, "CREATE TABLE users (id INTEGER PRIMARY KEY);\n").unwrap();

    let config = WikiConfig::default();
    let provider = GoodMermaidProvider {
        calls: AtomicUsize::new(0),
    };
    let doc = generate_schema_document(&provider, &sql_path, &config)
        .await
        .unwrap()
        .expect("好图应直通");
    assert!(doc.content.contains("```mermaid"), "好图应保留");
    assert_eq!(provider.calls.load(Ordering::Relaxed), 1, "好图不应重试");

    let _ = std::fs::remove_dir_all(&dir);
}
