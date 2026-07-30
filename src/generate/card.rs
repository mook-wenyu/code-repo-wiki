use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use futures::future::join_all;

use crate::generate::chunk::Chunk;
use crate::generate::llm::LlmProvider;
use crate::generate::prompt;
use crate::model::{EntitySummary, KnowledgeCard};

/// Knowledge Card 生成器
///
/// 通过 LLM 为每个代码模块生成结构化的 Knowledge Card，
/// 供 AI Agent 快速理解模块职责和关键实体。
pub struct CardGenerator<'a, P: LlmProvider> {
    provider: &'a P,
    call_count: AtomicUsize,
    semaphore: tokio::sync::Semaphore,
}

impl<'a, P: LlmProvider> CardGenerator<'a, P> {
    /// 使用指定的 LLM Provider 创建 CardGenerator
    ///
    /// max_concurrent 控制并行 LLM 调用的最大并发数（0 表示不限制）。
    pub fn new(provider: &'a P, max_concurrent: usize) -> Self {
        let max = if max_concurrent == 0 { usize::MAX } else { max_concurrent };
        Self {
            provider,
            call_count: AtomicUsize::new(0),
            semaphore: tokio::sync::Semaphore::new(max),
        }
    }

    /// 返回已完成的 LLM 调用次数
    pub fn llm_call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }

    /// 为单个模块生成 Knowledge Card
    ///
    /// 跳过空块。LLM 调用失败时返回错误。
    /// 通过 Semaphore 控制并发（acquire → complete → release）。
    pub async fn generate_card(&self, chunk: &Chunk) -> Result<KnowledgeCard> {
        if chunk.is_empty() {
            anyhow::bail!("空块，跳过生成");
        }

        // 获取并发许可（无限并发时 Semaphore::new(usize::MAX) 永不会阻塞）
        let _permit = self.semaphore.acquire().await.map_err(|_| {
            anyhow::anyhow!("信号量已关闭")
        })?;

        self.call_count.fetch_add(1, Ordering::Relaxed);

        let messages = prompt::knowledge_card_prompt(chunk, "zh");
        let response = self.provider.complete(&messages).await?;

        parse_card_response(&response, chunk)
    }

    /// 并行生成所有模块的 Knowledge Card
    ///
    /// 使用 join_all + Semaphore 实现可控并发。失败的卡片会被跳过（不中断整体流程）。
    pub async fn generate_all_cards(
        &self,
        chunks: &[Chunk],
    ) -> Result<Vec<KnowledgeCard>> {
        let mut handles = Vec::with_capacity(chunks.len());

        for chunk in chunks {
            handles.push(self.generate_card(chunk));
        }

        let results = join_all(handles).await;
        let cards: Vec<KnowledgeCard> = results
            .into_iter()
            .filter_map(|r| {
                if let Err(e) = &r {
                    tracing::warn!("Knowledge Card 生成失败，跳过: {}", e);
                    None
                } else {
                    r.ok()
                }
            })
            .collect();

        Ok(cards)
    }
}

/// 解析 LLM 返回的 JSON 响应为 KnowledgeCard
fn parse_card_response(response: &str, chunk: &Chunk) -> Result<KnowledgeCard> {
    let json_str = extract_json(response);

    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("解析卡片 JSON 失败: {}", e))?;

    let summary = parsed["summary"].as_str().unwrap_or("").to_string();

    let key_entities: Vec<EntitySummary> = parsed["key_entities"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|v| EntitySummary {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    kind: v["kind"].as_str().unwrap_or("").to_string(),
                    visibility: v["visibility"].as_str().unwrap_or("public").to_string(),
                    doc: v["doc"].as_str().map(|s| s.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();

    let design_patterns: Vec<String> = parsed["design_patterns"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let todo_notes: Vec<String> = parsed["todo_notes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(KnowledgeCard {
        module_name: chunk.module_path.join("::"),
        module_type: "module".to_string(),
        summary,
        key_entities,
        dependencies: chunk.dependencies.clone(),
        dependents: Vec::new(),
        design_patterns,
        todo_notes,
    })
}

/// 从 LLM 响应中提取 JSON 字符串（去除 Markdown 代码块标记）
fn extract_json(text: &str) -> &str {
    let text = text.trim();
    if let Some(start) = text.find('{') {
        let end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
        &text[start..end]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::chunk::chunk_by_file;
    
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    use std::path::PathBuf;

    fn make_test_chunk() -> Chunk {
        let entity = Entity {
            name: "Config".into(),
            kind: "struct".into(),
            line_start: 1,
            line_end: 30,
            doc_comment: Some("配置管理".into()),
            signature: None,
        };
        let insight = FileInsight {
            path: PathBuf::from("src/config.rs"),
            language: "rust".into(),
            entities: vec![entity],
            imports: vec![ImportStmt {
                source: "serde".into(),
                alias: None,
                line: 1,
            }],
            doc_comments: vec![],
        };
        chunk_by_file(&insight)
    }

    #[test]
    fn test_extract_json() {
        let input = "```json\n{\"summary\": \"test\"}\n```";
        assert_eq!(extract_json(input), "{\"summary\": \"test\"}");

        let input = "{\"summary\": \"test\"}";
        assert_eq!(extract_json(input), "{\"summary\": \"test\"}");
    }

    #[test]
    fn test_parse_card_response() {
        let response = r#"{"summary": "配置模块", "key_entities": [{"name": "Config", "kind": "struct", "visibility": "public", "doc": "配置结构"}], "design_patterns": ["Builder"], "todo_notes": []}"#;
        let chunk = make_test_chunk();
        let card = parse_card_response(response, &chunk).unwrap();

        assert_eq!(card.summary, "配置模块");
        assert_eq!(card.key_entities.len(), 1);
        assert_eq!(card.key_entities[0].name, "Config");
    }

    #[test]
    fn test_parse_card_empty_response() {
        let response = r#"{"summary": "", "key_entities": [], "design_patterns": [], "todo_notes": []}"#;
        let chunk = make_test_chunk();
        let card = parse_card_response(response, &chunk).unwrap();

        assert!(card.summary.is_empty());
        assert!(card.key_entities.is_empty());
    }
}
