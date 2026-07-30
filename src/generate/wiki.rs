use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::generate::chunk::Chunk;
use crate::generate::llm::LlmProvider;
use crate::generate::prompt;
use crate::generate::GenerationOutput;
use crate::model::{DocumentKind, KnowledgeGraph, Reference, WikiDocument};

/// Wiki 页面生成器
///
/// 通过 LLM 为每个模块生成叙述性的 Wiki 页面，
/// 供人类开发者阅读和理解代码。
pub struct WikiGenerator<'a, P: LlmProvider> {
    provider: &'a P,
    call_count: AtomicUsize,
}

impl<'a, P: LlmProvider> WikiGenerator<'a, P> {
    /// 使用指定的 LLM Provider 创建 WikiGenerator
    pub fn new(provider: &'a P) -> Self {
        Self {
            provider,
            call_count: AtomicUsize::new(0),
        }
    }

    /// 返回已完成的 LLM 调用次数
    pub fn llm_call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }

    /// 生成模块 Wiki 页面
    ///
    /// * `chunk` — 模块的代码数据块
    /// * `card_summary` — 之前生成的 Knowledge Card 摘要，作为上下文参考
    /// * `config` — Wiki 配置（用于获取语言设置等）
    pub async fn generate_wiki_page(
        &self,
        chunk: &Chunk,
        card_summary: &str,
        config: &WikiConfig,
    ) -> Result<WikiDocument> {
        if chunk.is_empty() {
            anyhow::bail!("空块，跳过 Wiki 页面生成");
        }

        self.call_count.fetch_add(1, Ordering::Relaxed);

        let language = &config.wiki.language;
        let messages = prompt::wiki_page_prompt(chunk, card_summary, language);
        let content = self.provider.complete(&messages).await?;
        let now = chrono::Utc::now().to_rfc3339();

        Ok(WikiDocument {
            title: chunk.module_path.last().cloned().unwrap_or_default(),
            kind: DocumentKind::WikiPage,
            content,
            module_path: chunk.module_path.clone(),
            references: build_references(chunk),
            last_updated: now,
            fingerprint: None,
        })
    }

    /// 生成架构概览页面
    ///
    /// 基于所有模块的生成输出和知识图谱，生成项目级的架构概览文档。
    pub async fn generate_architecture(
        &self,
        output: &GenerationOutput,
        graph: &KnowledgeGraph,
        config: &WikiConfig,
    ) -> Result<WikiDocument> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        let language = &config.wiki.language;
        let messages = prompt::architecture_overview_prompt(&graph.modules, graph, language);
        let content = self.provider.complete(&messages).await?;
        let now = chrono::Utc::now().to_rfc3339();

        Ok(WikiDocument {
            title: "架构概览".into(),
            kind: DocumentKind::ArchitectureOverview,
            content,
            module_path: vec![],
            references: output
                .cards
                .iter()
                .map(|c| Reference {
                    target_title: c.module_name.clone(),
                    target_path: format!("wiki/{}.md", c.module_name.replace("::", "/")),
                    relation: "module".into(),
                })
                .collect(),
            last_updated: now,
            fingerprint: None,
        })
    }
}

/// 从 Chunk 构建交叉引用
fn build_references(chunk: &Chunk) -> Vec<Reference> {
    chunk
        .dependencies
        .iter()
        .map(|dep| Reference {
            target_title: dep.clone(),
            target_path: format!("wiki/{}.md", dep.replace("::", "/")),
            relation: "depends_on".into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::chunk::chunk_by_file;
    use crate::generate::llm::MockProvider;
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    
    use std::path::PathBuf;

    #[allow(dead_code)]
    fn make_test_chunk() -> Chunk {
        let entity = Entity {
            name: "Server".into(),
            kind: "struct".into(),
            line_start: 1,
            line_end: 50,
            doc_comment: Some("HTTP 服务".into()),
            signature: None,
        };
        let insight = FileInsight {
            path: PathBuf::from("src/server.rs"),
            language: "rust".into(),
            entities: vec![entity],
            imports: vec![ImportStmt {
                source: "tokio".into(),
                alias: None,
                line: 1,
            }],
            doc_comments: vec![],
        };
        chunk_by_file(&insight)
    }

    #[tokio::test]
    async fn test_skip_empty_chunk() {
        let provider = MockProvider::new();
        let generator = WikiGenerator::new(&provider);
        let config = WikiConfig::default();

        let empty_chunk = Chunk {
            module_path: vec![],
            entities: vec![],
            imports: vec![],
            dependencies: vec![],
            file_paths: vec![],
        };

        let result = generator.generate_wiki_page(&empty_chunk, "", &config).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_build_references() {
        let chunk = Chunk {
            module_path: vec!["crate".into(), "net".into()],
            entities: vec![],
            imports: vec![],
            dependencies: vec!["tokio".into(), "serde".into()],
            file_paths: vec![],
        };

        let refs = build_references(&chunk);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].target_title, "tokio");
    }
}
