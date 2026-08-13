#![cfg(test)]

//! v31 C-03 边界补充验证（集成层，仅用公共 API）
//!
//! 修复语义：空 chunk 是确定性「无内容」而非生成失败——
//! 空 chunk 不产生 Err、不记 failed_modules；成功卡片不截断；
//! 失败归因准确（空 chunk 不得错位）。
//!
//! 单测已覆盖 [空, 失败] 与 [空, 成功]（空在首位）；本文件补齐
//! 未覆盖边界：
//! ① 全空 chunks → Ok(空卡片列表)、零 LLM 调用
//! ② 混合交错 [成功, 空, 失败]（空在中间）→ 成功卡片保留 + 失败归因真实模块
//!    （旧实现 module 名取全量 chunks 与仅非空的 results zip，会把失败记到空模块名下）
//! ③ 交错 [空, 失败] / [空, 成功] 的公共 API 视角（与单测同构）

use std::collections::HashMap;
use std::path::PathBuf;

use code_repo_wiki::config::schema::WikiConfig;
use code_repo_wiki::generate::card::CardGenerator;
use code_repo_wiki::generate::chunk::{Chunk, chunk_by_file};
use code_repo_wiki::generate::llm::{LlmProvider, Message, MockProvider};
use code_repo_wiki::ingest::parser::{Entity, FileInsight, ImportStmt};

/// 构造非空 chunk（module_path = ["src"]，与单测 make_test_chunk 同构）
fn make_test_chunk() -> Chunk {
    let entity = Entity {
        name: "Config".into(),
        kind: "struct".into(),
        line_start: 1,
        line_end: 30,
        doc_comment: Some("配置管理".into()),
        signature: None,
        visibility: None,
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
        source: String::new(),
    };
    chunk_by_file(&insight)
}

/// 构造指定模块路径的空 chunk（无实体、无导入 → is_empty() == true）
fn make_empty_chunk(module: &str) -> Chunk {
    let mut chunk = make_test_chunk();
    chunk.module_path = vec![module.to_string()];
    chunk.entities = Vec::new();
    chunk.imports = Vec::new();
    chunk
}

fn temp_config(tag: &str) -> (WikiConfig, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_it_v31_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (
        WikiConfig {
            output_dir: Some(dir.to_path_buf()),
            ..Default::default()
        },
        dir,
    )
}

/// 边界①：全空 chunks → Ok(与 chunks 对齐的全 None 占位)、零失败、零 LLM 调用
#[tokio::test]
async fn all_empty_chunks_yield_empty_cards_and_no_failures() {
    let (config, dir) = temp_config("all_empty");
    let provider = MockProvider::new();
    let generator = CardGenerator::new(
        &provider,
        config,
        1,
        "zh".into(),
        HashMap::new(),
        HashMap::new(),
    );

    let cards = generator
        .generate_all_cards(
            &[make_empty_chunk("zzz_a"), make_empty_chunk("zzz_b")],
            &HashMap::new(),
            &|_| {},
        )
        .await
        .unwrap();
    // P1-1 对齐语义：全空 chunk 返回长度 = chunks 长度、全 None 位
    assert_eq!(cards.len(), 2, "对齐语义：长度恒 = chunks 长度");
    assert!(
        cards.iter().all(|c| c.is_none()),
        "全空 chunk 占 None 位，不产出卡片"
    );
    assert!(
        generator.failed_modules().is_empty(),
        "全空 chunk 不得记入 failed_modules: {:?}",
        generator.failed_modules()
    );
    assert_eq!(generator.llm_call_count(), 0, "空 chunk 不得触发 LLM 调用");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 边界②：混合交错 [成功(src), 空(zzz), 失败(other)]——空 chunk 在中间。
/// 旧实现（module 名取全量 chunks 与仅非空的 results zip）会把失败记到
/// 空模块 zzz 名下；修复后必须归因到真实失败模块 other。
#[tokio::test]
async fn mixed_interleave_success_empty_failure_attributes_correctly() {
    let mut other = make_test_chunk();
    other.module_path = vec!["other".into()];

    let selective = SelectiveFailingProvider {
        fail_on: "模块路径: other",
    };
    let (config, dir) = temp_config("mixed");
    let generator = CardGenerator::new(
        &selective,
        config,
        1,
        "zh".into(),
        HashMap::new(),
        HashMap::new(),
    );

    let cards = generator
        .generate_all_cards(
            &[make_test_chunk(), make_empty_chunk("zzz"), other],
            &HashMap::new(),
            &|_| {},
        )
        .await
        .unwrap();
    // P1-1 对齐语义：长度恒 = chunks 长度（空 chunk/失败占 None 位），成功卡在索引 0
    assert_eq!(cards.len(), 3, "对齐语义：长度恒 = chunks 长度");
    assert!(cards[0].as_ref().is_some(), "成功卡片保留在正确索引位");
    assert_eq!(
        cards[0].as_ref().unwrap().module_name,
        "src",
        "成功卡片归因正确，不得被空 chunk 错位截断"
    );
    assert!(
        cards[1].is_none() && cards[2].is_none(),
        "空 chunk 与失败模块均占 None 位"
    );
    assert_eq!(
        generator.failed_modules(),
        vec!["other"],
        "失败必须归因到真实失败模块（空 chunk 在中间不得错位）: {:?}",
        generator.failed_modules()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 边界③：[空, 失败] 与 [空, 成功]（公共 API 视角，与单测同构）
#[tokio::test]
async fn empty_chunk_interleaved_with_failure_and_success() {
    // [空, 失败]：失败归因到真实模块 src，空模块不得记入
    let failing = FailingProvider;
    let (config1, dir1) = temp_config("empty_fail");
    let gen1 = CardGenerator::new(
        &failing,
        config1,
        1,
        "zh".into(),
        HashMap::new(),
        HashMap::new(),
    );
    let cards1 = gen1
        .generate_all_cards(
            &[make_empty_chunk("zzz"), make_test_chunk()],
            &HashMap::new(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(cards1.len(), 2, "对齐语义：长度恒 = chunks 长度");
    assert!(
        cards1.iter().all(|c| c.is_none()),
        "空 chunk 与失败模块均不产出卡片"
    );
    assert_eq!(
        gen1.failed_modules(),
        vec!["src"],
        "失败必须归因到真实失败模块: {:?}",
        gen1.failed_modules()
    );

    // [空, 成功]：成功卡片保留且归因正确（无错位截断）
    let provider = MockProvider::new();
    let (config2, dir2) = temp_config("empty_ok");
    let gen2 = CardGenerator::new(
        &provider,
        config2,
        1,
        "zh".into(),
        HashMap::new(),
        HashMap::new(),
    );
    let cards2 = gen2
        .generate_all_cards(
            &[make_empty_chunk("zzz"), make_test_chunk()],
            &HashMap::new(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(cards2.len(), 2, "对齐语义：长度恒 = chunks 长度");
    assert!(cards2[0].is_none(), "空 chunk 占 None 位");
    assert_eq!(
        cards2[1].as_ref().unwrap().module_name,
        "src",
        "成功卡片在正确索引位，不得错位"
    );
    assert!(
        gen2.failed_modules().is_empty(),
        "无真实失败时不记 failed_modules: {:?}",
        gen2.failed_modules()
    );

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// 总是失败的 LLM provider（验证真实失败仍入 failed_modules）
struct FailingProvider;

impl LlmProvider for FailingProvider {
    async fn complete(&self, _messages: &[Message]) -> anyhow::Result<String> {
        anyhow::bail!("模拟 LLM 调用失败")
    }

    async fn complete_stream(&self, _messages: &[Message]) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("模拟 LLM 调用失败")
    }

    fn call_count(&self) -> usize {
        0
    }
}

/// 按消息内容选择性失败：命中 fail_on 子串的调用失败，其余成功
/// （prompt 内含 "模块路径: <path>"，可据此区分模块）
struct SelectiveFailingProvider {
    fail_on: &'static str,
}

impl LlmProvider for SelectiveFailingProvider {
    async fn complete(&self, messages: &[Message]) -> anyhow::Result<String> {
        if messages.iter().any(|m| m.content.contains(self.fail_on)) {
            anyhow::bail!("模拟 LLM 调用失败")
        }
        Ok(r#"{"summary": "这是 Mock Provider 生成的模拟摘要", "key_entities": []}"#.to_string())
    }

    async fn complete_stream(&self, messages: &[Message]) -> anyhow::Result<Vec<String>> {
        if messages.iter().any(|m| m.content.contains(self.fail_on)) {
            anyhow::bail!("模拟 LLM 调用失败")
        }
        Ok(vec!["模拟流式响应 chunk".to_string()])
    }

    fn call_count(&self) -> usize {
        0
    }
}
