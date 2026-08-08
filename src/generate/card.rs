use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use futures::future::join_all;

use crate::config::schema::WikiConfig;
use crate::generate::chunk::Chunk;
use crate::generate::llm::{LlmProvider, Provider};
use crate::generate::prompt;
use crate::model::{EntitySummary, KnowledgeCard};

/// 卡片操作动作（CLI card 子命令与 Qoder /knowledge 对等）
pub enum CardAction {
    /// 为单个模块生成卡片（重新生成）
    Generate { module: String },
    /// 按指令修改已有卡片
    Modify { module: String, instruction: String, references: Vec<PathBuf> },
    /// 在已有卡片上追加内容
    Supplement { module: String, instruction: String, references: Vec<PathBuf> },
    /// 忽略现有内容全量重写
    Rewrite { module: String, instruction: String, references: Vec<PathBuf> },
}

/// 卡片编辑模式（决定 prompt 是否携带现有内容）
pub enum CardEditMode {
    /// 修改：现有内容 + 指令
    Modify,
    /// 追加：现有保留，新内容附后
    Supplement,
    /// 重写：仅指令 + 模块来源信息
    Rewrite,
}

impl CardEditMode {
    fn as_str(&self) -> &'static str {
        match self {
            CardEditMode::Modify => "modify",
            CardEditMode::Supplement => "supplement",
            CardEditMode::Rewrite => "rewrite",
        }
    }
}

/// Knowledge Card 生成器
///
/// 通过 LLM 为每个代码模块生成结构化的 Knowledge Card，
/// 供 AI Agent 快速理解模块职责和关键实体。
pub struct CardGenerator<'a, P: LlmProvider> {
    provider: &'a P,
    call_count: AtomicUsize,
    semaphore: tokio::sync::Semaphore,
    /// 输出语言（用于 prompt 模板）
    language: String,
    /// 项目配置（卡片定位：生成前从旧卡片恢复人工修改记录，与单卡路径同构）
    config: WikiConfig,
    /// 生成失败的模块名列表（演进计划 T3.2 失败隔离：失败只记录不中断）
    failed: std::sync::Mutex<Vec<String>>,
}

impl<'a, P: LlmProvider> CardGenerator<'a, P> {
    /// 使用指定的 LLM Provider 创建 CardGenerator
    ///
    /// max_concurrent 控制并行 LLM 调用的最大并发数（0 表示不限制）。
    /// language 指定生成内容的语言。
    /// config 提供产物路径定位（卡片文件读/写规则），供人工修改记录恢复。
    pub fn new(
        provider: &'a P,
        config: WikiConfig,
        max_concurrent: usize,
        language: String,
    ) -> Self {
        // tokio Semaphore 许可数有 MAX_PERMITS 上限（约 2^61），usize::MAX 会 panic；
        // "0=不限制" 用足够大的许可数表达（对真实并发规模永不构成瓶颈）
        let max = if max_concurrent == 0 { 1_000_000_000 } else { max_concurrent };
        Self {
            provider,
            call_count: AtomicUsize::new(0),
            semaphore: tokio::sync::Semaphore::new(max),
            language,
            config,
            failed: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 返回已完成的 LLM 调用次数
    pub fn llm_call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }

    /// 返回生成失败的模块名列表（演进计划 T3.2：失败隔离的可见性出口）
    pub fn failed_modules(&self) -> Vec<String> {
        self.failed.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 为单个模块生成 Knowledge Card
    ///
    /// 跳过空块。LLM 调用失败时返回错误。
    /// 通过 Semaphore 控制并发（acquire → complete → release）。
    /// pending_manual_edits 为人工修改记录：注入 LLM 输入，
    /// 且解析后回填新卡片（该字段由管道维护，LLM 不会输出，回填保证不丢）。
    pub async fn generate_card(
        &self,
        chunk: &Chunk,
        pending_manual_edits: &[String],
    ) -> Result<KnowledgeCard> {
        if chunk.is_empty() {
            anyhow::bail!("空块，跳过生成");
        }

        // 获取并发许可（无限并发时 Semaphore::new(usize::MAX) 永不会阻塞）
        let _permit = self.semaphore.acquire().await.map_err(|_| {
            anyhow::anyhow!("信号量已关闭")
        })?;

        self.call_count.fetch_add(1, Ordering::Relaxed);

        let messages = prompt::knowledge_card_prompt(
            chunk,
            &self.language,
            pending_manual_edits,
        );
        let response = self.provider.complete(&messages).await?;

        let mut card = parse_card_response(&response, chunk)?;
        if !pending_manual_edits.is_empty() {
            card.pending_manual_edits = pending_manual_edits.to_vec();
        }
        // 反向链接回填（演进计划 T3.3）：key_entities 按名匹配 chunk 实体，
        // 填充 "文件路径:起始行-结束行" 的源码定位，不经过 LLM（防幻觉）
        backfill_entity_sources(&mut card, chunk);
        Ok(card)
    }

    /// 并行生成所有模块的 Knowledge Card
    ///
    /// 使用 join_all + Semaphore 实现可控并发。失败的卡片会被跳过（不中断整体流程）。
    ///
    /// 人工修改记录的两路来源在**生成前**合并为 LLM 输入（与单卡重生成
    /// generate_module_card 同构，管道不再"生成后补记"）：
    /// 1. 旧卡片磁盘上的"人工修改待同步"节（recover_pending_manual_edits，
    ///    记录随生成回填到新卡片，保证不丢）；
    /// 2. extra_edits：本次运行新检测到的人工修改（模块名 → 记录文本），
    ///    由上层（lib.rs 从状态指纹比对结果）组装传入。
    pub async fn generate_all_cards(
        &self,
        chunks: &[Chunk],
        extra_edits: &std::collections::HashMap<String, Vec<String>>,
    ) -> Result<Vec<KnowledgeCard>> {
        let mut handles = Vec::with_capacity(chunks.len());

        // task_modules 与 handles 同源收集：循环内仅对非空 chunk push，
        // 下方 zip 时二者长度/顺序 1:1，空 chunk 跳过不会造成失败归因错位
        let mut task_modules: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            // v31 修复（C-03）：空 chunk 是确定性「无内容」而非生成失败——
            // 跳过不产生 Err、不记 failed_modules（污染会使 should_skip_noop
            // 永久失效并引发无关模块补偿重试）。真实 LLM 失败仍由 Err 分支记录。
            // module 名与 handles 同源收集（下方 zip 与 task_modules 1:1 对齐，
            // 空 chunk 跳过不会造成失败归因错位或结果截断）。
            if chunk.is_empty() {
                continue;
            }
            let module = chunk.module_path.join("::");
            task_modules.push(module.clone());
            // 旧卡片读取失败按失败隔离语义降级（单卡不中断整体生成），
            // 但显式告警——静默丢人工修改记录会让人工修改保护失效
            let mut pending = match self.recover_pending_manual_edits(&module) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "读取旧卡片人工修改记录失败，本次生成不携带旧记录 {}: {}",
                        module,
                        e
                    );
                    Vec::new()
                }
            };
            if let Some(extra) = extra_edits.get(&module) {
                for note in extra {
                    if !pending.contains(note) {
                        pending.push(note.clone());
                    }
                }
            }
            // async move 拥有 pending（避免 future 借用循环内临时值）；
            // chunk 为 &Chunk，self 为 &self，均为引用移动
            let generator = self;
            handles.push(async move { generator.generate_card(chunk, &pending).await });
        }

        let results = join_all(handles).await;
        let cards: Vec<KnowledgeCard> = task_modules
            .into_iter()
            .zip(results)
            .filter_map(|(module, r)| {
                match r {
                    Ok(card) => Some(card),
                    Err(e) => {
                        // 失败隔离：记录失败的模块名（T3.2），不中断其他模块生成
                        tracing::warn!("Knowledge Card 生成失败，跳过 {}: {}", module, e);
                        if let Ok(mut failed) = self.failed.lock() {
                            failed.push(module);
                        }
                        None
                    }
                }
            })
            .collect();

        Ok(cards)
    }

    /// 从旧卡片磁盘文件恢复"人工修改待同步"记录（无该节时返回空）
    ///
    /// 按模块名精确定位卡片文件（read_card 路径规则与渲染层一致），
    /// 解析"## 人工修改待同步"节内容——与单卡重生成路径共用同一解析逻辑，
    /// 保证两条路径对记录的恢复行为一致。
    ///
    /// 返回 Result：磁盘读取错误（损坏/权限）向上传播，由调用方按
    /// 失败隔离语义显式告警并降级（生成时携带空记录），不静默吞掉。
    fn recover_pending_manual_edits(&self, module: &str) -> Result<Vec<String>> {
        Ok(read_card(&self.config, module)?
            .map(|content| extract_pending_manual_edits(&content))
            .unwrap_or_default())
    }
}

/// 卡片文件路径：cards/{主语言}/{module.replace("::","_")}.md（与 render_all 写盘规则一致）
fn card_path(config: &WikiConfig, module: &str) -> PathBuf {
    let primary_lang = &crate::output::wiki_languages(config)[0];
    crate::output::card_page_path(config.output_dir(), primary_lang, module)
}

/// 读取现有卡片 markdown（按模块名定位 cards/{lang}/{module}.md）
///
/// 卡片不存在时返回 Ok(None)。
pub fn read_card(config: &WikiConfig, module: &str) -> Result<Option<String>> {
    let path = card_path(config, module);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 读取参考文件并拼接为参考材料段落（空列表跳过拼接）
fn read_references(references: &[PathBuf]) -> Result<String> {
    let mut block = String::new();
    for path in references {
        let content = std::fs::read_to_string(path)?;
        block.push_str(&format!("\n\n### {}\n{}", path.display(), content));
    }
    Ok(block)
}

/// 去除 LLM 返回中的 Markdown 代码块包裹（如有），否则原样返回
fn extract_markdown(text: &str) -> &str {
    let text = text.trim();
    match text.strip_prefix("```") {
        Some(rest) => rest
            .split_once('\n')
            .map(|(_, body)| body.trim().trim_end_matches("```").trim())
            .unwrap_or(text),
        None => text,
    }
}

/// 原子写回卡片文件：统一走 fs::write_file_atomic（同目录临时文件 +
/// rename 原子覆盖）。rustc 1.84+ 的 rename 在 Windows 已是 POSIX 语义
/// 原子替换，无需先删目标（删掉旧实现的 remove_file 前置，消除
/// "目标已删、临时文件未就位"的中间窗口）。
fn write_card_atomic(config: &WikiConfig, module: &str, content: &str) -> Result<()> {
    let path = card_path(config, module);
    crate::fs::write_file_atomic(&path, content)
}

/// 从卡片 markdown 中提取"人工修改待同步"节内容（无该节时返回空）
///
/// 节内格式为 "- 记录" 列表行；遇到其他 "## " 标题即结束。
/// 用于单卡重生成（generate_module_card）时恢复旧卡片上的记录作为 LLM 输入。
fn extract_pending_manual_edits(content: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_section = false;
    for line in content.lines() {
        if line.starts_with("## ") {
            in_section = line == "## 人工修改待同步";
            continue;
        }
        if in_section && let Some(item) = line.strip_prefix("- ") {
            items.push(item.to_string());
        }
    }
    items
}

/// 为单个模块重新生成卡片：全量扫描分块 → 定位模块 chunk → LLM 生成 → 写回
///
/// 仅写主语言目录的卡片文件（与其他语言目录由全量 generate 统一刷新）。
/// 旧卡片上的"人工修改待同步"记录会被恢复为本次 LLM 输入，并保留在新卡片中。
pub async fn generate_module_card(
    provider: &Provider,
    config: &WikiConfig,
    root: &crate::project::ProjectRoot,
    module: &str,
) -> Result<()> {
    let insights = crate::ingest::scan_and_parse_at(root)?.insights;
    let graph = crate::analysis::build_graph(&insights)?;
    let chunks = crate::generate::chunk::chunk_by_module(&insights, &graph.modules, &graph);
    let chunk = chunks
        .into_iter()
        .find(|c| c.module_path.join("::") == module)
        .ok_or_else(|| anyhow::anyhow!(
            "未找到模块 {module} 对应的代码分块，请检查模块名或先运行 `code-repo-wiki generate` 全量生成"
        ))?;

    // 从旧卡片恢复人工修改记录（卡片不存在或尚无该节时为空）
    let pending = read_card(config, module)?
        .map(|content| extract_pending_manual_edits(&content))
        .unwrap_or_default();

    let generator = CardGenerator::new(provider, config.clone(), 1, config.wiki.language.clone());
    let card = generator.generate_card(&chunk, &pending).await?;
    let content = crate::output::markdown::render_knowledge_card(&card);

    write_card_atomic(config, module, &content)?;
    tracing::info!("卡片已生成: {} → {}", module, card_path(config, module).display());
    Ok(())
}

/// 修改/补充/重写：现有内容 + 指令 + 参考文件 → LLM → 原子写回
pub async fn edit_card(
    provider: &Provider,
    config: &WikiConfig,
    module: &str,
    instruction: &str,
    references: &[PathBuf],
    mode: CardEditMode,
) -> Result<()> {
    let existing = read_card(config, module)?.ok_or_else(|| anyhow::anyhow!(
        "模块 {module} 的卡片不存在（{}），请先运行 `code-repo-wiki generate` 或 `code-repo-wiki card generate <module>` 生成",
        card_path(config, module).display()
    ))?;
    let reference_block = read_references(references)?;

    let messages = prompt::edit_card_prompt(
        mode.as_str(),
        module,
        &existing,
        instruction,
        &reference_block,
        &config.wiki.language,
    );
    let response = provider.complete(&messages).await?;
    let content = extract_markdown(&response);

    write_card_atomic(config, module, content)?;
    tracing::info!("卡片已更新: {} → {}", module, card_path(config, module).display());
    Ok(())
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
                    source: None,
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

    // LLM 描述性字段：缺失时置空，不因单个字段缺失而丢弃整张卡片
    let coding_spec = parsed["coding_spec"].as_str().map(|s| s.to_string());
    let tech_stack: Vec<String> = parsed["tech_stack"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let architecture = parsed["architecture"].as_str().map(|s| s.to_string());

    Ok(KnowledgeCard {
        module_name: chunk.module_path.join("::"),
        module_type: "module".to_string(),
        summary,
        key_entities,
        dependencies: chunk.dependencies.clone(),
        dependents: Vec::new(),
        design_patterns,
        todo_notes,
        // 关联文件不经过 LLM，直接由 chunk 的源文件列表填充
        related_files: chunk
            .file_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        coding_spec,
        tech_stack,
        architecture,
        pending_manual_edits: Vec::new(),
        features: Vec::new(),
    })
}

/// 回填实体反向链接（演进计划 T3.3）
///
/// key_entities 按实体名匹配 chunk 实体，填充 "文件路径:起始行-结束行"。
/// 文件归属来自 chunk.entity_sources（T2.3 引入的实体→文件平行向量）；
/// 源码定位不经过 LLM，杜绝幻觉。未匹配到实体的条目保持 None。
fn backfill_entity_sources(card: &mut KnowledgeCard, chunk: &Chunk) {
    for es in &mut card.key_entities {
        if es.source.is_some() {
            continue;
        }
        if let Some((idx, entity)) = chunk
            .entities
            .iter()
            .enumerate()
            .find(|(_, e)| e.name == es.name)
            && let Some(file) = chunk.entity_sources.get(idx)
        {
            es.source = Some(format!(
                "{}:{}-{}",
                file.display(),
                entity.line_start,
                entity.line_end
            ));
        }
    }
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
    use crate::generate::llm::MockProvider;
    
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    use std::path::PathBuf;

    fn make_test_chunk() -> Chunk {
        let entity = Entity {
            name: "Config".into(),
            kind: "struct".into(),
            line_start: 1,
            line_end: 30,
            doc_comment: Some("配置管理".into()),
            signature: None, visibility: None,
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

    #[test]
    fn test_extract_json() {
        let input = "```json\n{\"summary\": \"test\"}\n```";
        assert_eq!(extract_json(input), "{\"summary\": \"test\"}");

        let input = "{\"summary\": \"test\"}";
        assert_eq!(extract_json(input), "{\"summary\": \"test\"}");
    }

    #[test]
    fn test_parse_card_response() {
        let response = r#"{"summary": "配置模块", "key_entities": [{"name": "Config", "kind": "struct", "visibility": "public", "doc": "配置结构"}], "design_patterns": ["Builder"], "todo_notes": [], "coding_spec": "遵循 rustfmt", "tech_stack": ["serde"], "architecture": "分层"}"#;
        let chunk = make_test_chunk();
        let card = parse_card_response(response, &chunk).unwrap();

        assert_eq!(card.summary, "配置模块");
        assert_eq!(card.key_entities.len(), 1);
        assert_eq!(card.key_entities[0].name, "Config");
        // 关联文件直接来自 chunk，不经过 LLM
        assert_eq!(card.related_files, vec!["src/config.rs".to_string()]);
        // LLM 描述性字段
        assert_eq!(card.coding_spec.as_deref(), Some("遵循 rustfmt"));
        assert_eq!(card.tech_stack, vec!["serde".to_string()]);
        assert_eq!(card.architecture.as_deref(), Some("分层"));
    }

    #[test]
    fn test_parse_card_empty_response() {
        let response = r#"{"summary": "", "key_entities": [], "design_patterns": [], "todo_notes": []}"#;
        let chunk = make_test_chunk();
        let card = parse_card_response(response, &chunk).unwrap();

        assert!(card.summary.is_empty());
        assert!(card.key_entities.is_empty());
        // LLM 未输出描述性字段时保持空值
        assert!(card.coding_spec.is_none());
        assert!(card.tech_stack.is_empty());
        assert!(card.architecture.is_none());
    }

    /// 预置卡片并返回临时输出目录的配置（tag 用于区分同进程内的多个测试目录）
    fn card_fixture(tag: &str, module: &str, content: &str) -> (WikiConfig, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_card_{tag}_{}_{}", module.replace("::", "_"), std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = WikiConfig { output_dir: Some(dir.to_path_buf()), ..Default::default() };
        std::fs::create_dir_all(config.output_dir().join("cards").join("zh")).unwrap();
        std::fs::write(card_path(&config, module), content).unwrap();
        (config, dir)
    }

    #[tokio::test]
    async fn test_edit_card_supplement_roundtrip() {
        let (config, dir) = card_fixture("supplement", "crate::test", "# crate::test\n\n## 摘要\n旧内容");
        let provider = Provider::Mock(MockProvider::new());
        edit_card(
            &provider,
            &config,
            "crate::test",
            "追加新内容",
            &[],
            CardEditMode::Supplement,
        )
        .await
        .unwrap();

        // 写回路径与 render_all 一致：cards/zh/crate_test.md
        let written = std::fs::read_to_string(config.output_dir().join("cards").join("zh").join("crate_test.md")).unwrap();
        assert!(written.contains("模拟摘要"), "应写入 Mock Provider 的响应内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_card_requires_existing() {
        let (config, dir) = card_fixture("missing", "crate::test", "内容");
        let provider = Provider::Mock(MockProvider::new());
        let err = edit_card(
            &provider,
            &config,
            "crate::missing",
            "指令",
            &[],
            CardEditMode::Rewrite,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("卡片不存在"), "应报卡片不存在: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_markdown_strips_codeblock() {
        assert_eq!(extract_markdown("```markdown\n# 标题\n```"), "# 标题");
        assert_eq!(extract_markdown("# 标题"), "# 标题");
    }

    #[test]
    fn test_extract_pending_manual_edits() {
        let content = "# src::test\n\n## 摘要\n旧内容\n\n## 人工修改待同步\n\n- 人工修改待同步: a.md 内容摘要: 1\n- 人工修改待同步: b.md 内容摘要: 2\n\n## 待办事项\n\n- [ ] x\n";
        let items = extract_pending_manual_edits(content);
        assert_eq!(items.len(), 2, "应提取节内两条记录");
        assert!(items[0].contains("a.md"));
        assert!(items[1].contains("b.md"));
        // 无该节 → 空
        assert!(extract_pending_manual_edits("# t\n\n## 摘要\nx").is_empty());
    }

    #[tokio::test]
    async fn test_generate_card_keeps_pending_manual_edits() {
        let chunk = make_test_chunk();
        let provider = Provider::Mock(MockProvider::new());
        let (config, dir) = card_fixture("pending", "src", "旧卡片内容");
        let generator = CardGenerator::new(&provider, config, 1, "zh".into());
        // 带记录：LLM 输入注入且生成后回填（渲染不丢）
        let pending = vec!["人工修改待同步: wiki/zh/src_config.md 内容摘要: 用户改的".into()];
        let card = generator.generate_card(&chunk, &pending).await.unwrap();
        assert_eq!(card.pending_manual_edits, pending);
        // 无记录：不回填
        let card = generator.generate_card(&chunk, &[]).await.unwrap();
        assert!(card.pending_manual_edits.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 管道路径闭环：generate_all_cards 生成前合并两路人工修改记录——
    /// 旧卡片磁盘恢复（recover_pending_manual_edits）+ 本次新增（extra_edits），
    /// 去重后注入 LLM 输入并回填新卡片
    #[tokio::test]
    async fn test_generate_all_cards_merges_recovered_and_extra_edits() {
        let chunk = make_test_chunk();
        let provider = Provider::Mock(MockProvider::new());
        // 预置旧卡片：含一条"人工修改待同步"记录（磁盘恢复源）
        let (config, dir) = card_fixture(
            "merge",
            "src",
            "# src\n\n## 摘要\n旧内容\n\n## 人工修改待同步\n\n- 人工修改待同步: wiki/zh/src.md 内容摘要: 旧记录\n",
        );

        let generator = CardGenerator::new(&provider, config, 1, "zh".into());
        // 本次新增记录（模块名 → 记录文本，lib.rs 组装）
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "src".to_string(),
            vec!["人工修改待同步: wiki/zh/src.md 内容摘要: 新修改".to_string()],
        );

        let cards = generator.generate_all_cards(&[chunk], &extra).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].pending_manual_edits.len(), 2, "旧记录 + 新记录应合并为 2 条");
        assert!(cards[0].pending_manual_edits.iter().any(|n| n.contains("旧记录")));
        assert!(cards[0].pending_manual_edits.iter().any(|n| n.contains("新修改")));

        // 去重：同一条记录同时存在于磁盘与 extra 时只保留一份
        let cards2 = generator.generate_all_cards(
            &[make_test_chunk()],
            &extra,
        ).await.unwrap();
        assert!(
            cards2[0].pending_manual_edits.len() <= 2,
            "重复记录应被去重: {:?}",
            cards2[0].pending_manual_edits
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3-4（A8 防回归）：旧卡片读取失败（卡片路径被替换为目录 →
    /// read_to_string 返回 IsADirectory）时，显式告警 + 降级为空记录，
    /// 不中断整批卡片生成；本次 extra 记录照常注入
    #[tokio::test]
    async fn test_generate_all_cards_survives_card_read_failure() {
        let chunk = make_test_chunk();
        let provider = Provider::Mock(MockProvider::new());
        let (config, dir) = card_fixture("readfail", "src", "# src\n\n## 摘要\n旧内容");

        // 卡片文件替换为同名目录：读取必然失败（非 NotFound 的 IO 错误路径）
        let card_file = card_path(&config, "src");
        std::fs::remove_file(&card_file).unwrap();
        std::fs::create_dir_all(&card_file).unwrap();

        let generator = CardGenerator::new(&provider, config, 1, "zh".into());
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "src".to_string(),
            vec!["人工修改待同步: wiki/zh/src.md 内容摘要: 新修改".to_string()],
        );

        let cards = generator.generate_all_cards(&[chunk], &extra).await.unwrap();
        assert_eq!(cards.len(), 1, "旧卡片读失败不应中断整批生成");
        assert_eq!(
            cards[0].pending_manual_edits,
            vec!["人工修改待同步: wiki/zh/src.md 内容摘要: 新修改".to_string()],
            "旧记录读取失败降级为空，只携带本次 extra 记录"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v31 C-03 防回归：管线入口已剔除空 chunk（chunks/cards/wiki/backfill
    /// 全链路 1:1 对齐）——generate_all_cards 收到非空 chunk 数组；
    /// 空 chunk 交错在中间时（[空, 失败] 与 [空, 成功]）失败归因必须
    /// 正确落在真实失败模块名下、成功卡片不得静默丢失、不得记入空模块。
    #[tokio::test]
    async fn test_generate_all_cards_skips_empty_chunks_records_real_failures() {
        // 构造空 chunk（管线入口过滤后不应到达生成循环；此处验证交错对齐）
        let mut empty = make_test_chunk();
        empty.module_path = vec!["zzz".into()];
        empty.entities = Vec::new();
        empty.imports = Vec::new();
        let provider = Provider::Mock(MockProvider::new());

        // ① [空, 真实失败]：失败必须记在真实模块 src 名下，而非空模块 zzz
        let failing = FailingProvider;
        let (config, dir) = card_fixture("interleave-fail", "src", "# src\n\n## 摘要\n旧内容");
        let fail_gen = CardGenerator::new(&failing, config, 1, "zh".into());
        let cards = fail_gen
            .generate_all_cards(&[empty.clone(), make_test_chunk()], &std::collections::HashMap::new())
            .await
            .unwrap();
        assert!(cards.is_empty(), "失败模块不产出卡片");
        assert_eq!(
            fail_gen.failed_modules(),
            vec!["src"],
            "失败必须归因到真实失败模块（空 chunk 已被入口剔除，不参与对齐）: {:?}",
            fail_gen.failed_modules()
        );
        let _ = std::fs::remove_dir_all(&dir);

        // ② [空, 成功]：成功卡片保留且摘要正确（无错位截断）
        let (config2, dir2) = card_fixture("interleave-ok", "src", "# src\n\n## 摘要\n旧内容");
        let gen2 = CardGenerator::new(&provider, config2, 1, "zh".into());
        let cards2 = gen2
            .generate_all_cards(&[empty, make_test_chunk()], &std::collections::HashMap::new())
            .await
            .unwrap();
        assert_eq!(cards2.len(), 1, "成功卡片不得静默丢失");
        assert_eq!(cards2[0].module_name, "src");
        assert!(gen2.failed_modules().is_empty(), "无真实失败时不记 failed_modules");
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// 测试专用：总是失败的 LLM provider（验证真实失败仍入 failed_modules）
    struct FailingProvider;

    impl LlmProvider for FailingProvider {
        async fn complete(
            &self,
            _messages: &[crate::generate::llm::Message],
        ) -> anyhow::Result<String> {
            anyhow::bail!("模拟 LLM 调用失败")
        }

        async fn complete_stream(
            &self,
            _messages: &[crate::generate::llm::Message],
        ) -> anyhow::Result<Vec<String>> {
            anyhow::bail!("模拟 LLM 调用失败")
        }

        fn call_count(&self) -> usize {
            0
        }
    }
}
