use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use crate::config::plan::ResolvedPlan;
use crate::config::schema::WikiConfig;
use crate::generate::chunk::Chunk;
use crate::generate::llm::{LlmProvider, Message};
use crate::generate::prompt;
use crate::generate::GenerationOutput;
use crate::model::{DocumentKind, EdgeKind, KnowledgeGraph, NodeId, Reference, WikiDocument};

/// Wiki 页面生成器
///
/// 通过 LLM 为每个模块生成叙述性的 Wiki 页面，
/// 供人类开发者阅读和理解代码。
pub struct WikiGenerator<'a, P: LlmProvider> {
    provider: &'a P,
    call_count: AtomicUsize,
    /// 生效计划（用于 notes 注入与模板选择，None 表示未启用）
    plan: Option<ResolvedPlan>,
    /// 生成失败的模块名列表（演进计划 T3.2 失败隔离：失败只记录不中断）
    failed: std::sync::Mutex<Vec<String>>,
}

impl<'a, P: LlmProvider> WikiGenerator<'a, P> {
    /// 使用指定的 LLM Provider 创建 WikiGenerator
    ///
    /// plan 为解析后的生效计划（无计划时传 None）。
    pub fn new(provider: &'a P, plan: Option<ResolvedPlan>) -> Self {
        Self {
            provider,
            call_count: AtomicUsize::new(0),
            plan,
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

    /// 记录一个模块的生成失败（内部使用，并发安全）
    pub(crate) fn record_failure(&self, module: String) {
        if let Ok(mut failed) = self.failed.lock() {
            failed.push(module);
        }
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
        let messages = prompt::wiki_page_prompt(chunk, card_summary, language, self.plan.as_ref());
        let content = self.provider.complete(&messages).await?;
        let now = chrono::Utc::now().to_rfc3339();

        Ok(WikiDocument {
            // 标题 = 完整模块路径（"src::generate"）：crossref 校验与概览/架构
            // 页引用的 target_title（模块名）一致，且链接文本比末段更可辨识；
            // 页面文件名由 module_path 派生，与标题解耦
            title: chunk.module_path.join("::"),
            kind: DocumentKind::WikiPage,
            content,
            language: config.wiki.language.clone(),
            module_path: chunk.module_path.clone(),
            references: build_references(chunk, &config.wiki.language),
            last_updated: now,
            fingerprint: None,
        })
    }

    /// 生成架构概览页面
    ///
    /// 基于所有模块的生成输出和知识图谱，生成项目级的架构概览文档。
    /// 生成前先为每个模块生成一行职责描述（describe_modules），
    /// 填充到模块快照的 description 字段——架构 prompt 依此输出模块职责，
    /// 取代原本恒为 None 的空描述（此前的 overview 只有模块名+节点数+边计数，
    /// 无法表达"模块负责什么"）。
    pub async fn generate_architecture(
        &self,
        output: &GenerationOutput,
        graph: &KnowledgeGraph,
        config: &WikiConfig,
    ) -> Result<WikiDocument> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        let language = &config.wiki.language;
        let modules = self.describe_modules(graph, language).await;
        let messages =
            prompt::architecture_overview_prompt(&modules, graph, language, self.plan.as_ref());
        let content = self.provider.complete(&messages).await?;
        let now = chrono::Utc::now().to_rfc3339();

        Ok(WikiDocument {
            title: "架构概览".into(),
            kind: DocumentKind::ArchitectureOverview,
            content,
            language: config.wiki.language.clone(),
            module_path: vec![],
            references: output
                .cards
                .iter()
                .map(|c| Reference {
                    target_title: c.module_name.clone(),
                    target_path: format!(
                        "wiki/{}/{}.md",
                        config.wiki.language,
                        // 模块页写盘文件名 = module_path.join("_")（见 output::wiki_file_name），
                        // 链接必须与之一致，否则 TOC/概览出现断链
                        c.module_name.replace("::", "_")
                    ),
                    relation: "module".into(),
                })
                .collect(),
            last_updated: now,
            fingerprint: None,
        })
    }

    /// 为每个模块生成一行职责描述（LLM），返回带 description 的模块快照
    ///
    /// 逐模块一条 user 消息：输入 = 模块名 + 实体名列表 + 依赖模块列表，
    /// 输出 = 一句话职责（≤30 字）。跳过 src 兜底模块（它吸收未聚类文件，
    /// 无明确职责边界，描述会失真）；LLM 失败时保留空描述（降级不影响主流程）。
    /// 各模块描述**并行**生成（join_all 保留顺序）——串行在 10+ 模块仓库会
    /// 拖长架构概览/项目概览的生成（真实 LLM 实测超时 10min 的根因之一）。
    async fn describe_modules(
        &self,
        graph: &KnowledgeGraph,
        language: &str,
    ) -> Vec<crate::model::ModuleCluster> {
        // 并行生成所有需描述的模块描述（保留输入顺序）
        let futures: Vec<_> = graph
            .modules
            .iter()
            .map(|module| async {
                // 兜底模块(src)与空模块跳过：无职责边界可描述
                if module.name == "src" || module.node_ids.is_empty() {
                    return module.clone();
                }
                let mut enriched = module.clone();
                if let Ok(text) = self.describe_module(module, graph, language).await
                    && !text.trim().is_empty()
                {
                    enriched.description = Some(text.trim().to_string());
                }
                enriched
            })
            .collect();
        futures::future::join_all(futures).await
    }

    /// 生成单个模块的一句话职责描述（LLM）
    async fn describe_module(
        &self,
        module: &crate::model::ModuleCluster,
        graph: &KnowledgeGraph,
        language: &str,
    ) -> Result<String> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        // 收集模块内实体名（跳过容器节点），作为 LLM 判断职责的输入
        let entity_names: Vec<String> = module
            .node_ids
            .iter()
            .filter_map(|nid| graph.graph.node_weight(*nid))
            .filter(|n| {
                !matches!(
                    n.kind,
                    crate::model::NodeKind::Project
                        | crate::model::NodeKind::Module
                        | crate::model::NodeKind::File
                )
            })
            .map(|n| n.name.clone())
            .take(30)
            .collect();

        let messages = prompt::module_description_prompt(
            &module.name,
            &entity_names,
            language,
        );
        self.provider.complete(&messages).await
    }

    /// 生成项目概览页面
    ///
    /// 与 generate_architecture 同签名同风格：基于完整 KnowledgeGraph 的
    /// 模块列表与模块间依赖摘要，生成全仓库概览（技术栈/目录结构/核心模块）。
    pub async fn generate_overview(
        &self,
        output: &GenerationOutput,
        graph: &KnowledgeGraph,
        config: &WikiConfig,
    ) -> Result<WikiDocument> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        // 与 generate_architecture 一致：先补模块职责描述，概览内容才能
        // 表达"模块负责什么"；再叠加卡片摘要（自底向上合成：父概览基于
        // 子模块的职责描述 + 卡片摘要生成，而非仅模块名/节点数/边计数）
        let modules = self.describe_modules(graph, &config.wiki.language).await;
        let messages = vec![Message::user(overview_prompt(&modules, &output.cards, graph, config))];
        let content = self.provider.complete(&messages).await?;
        let now = chrono::Utc::now().to_rfc3339();

        Ok(WikiDocument {
            title: "项目概览".into(),
            kind: DocumentKind::ProjectOverview,
            content,
            language: config.wiki.language.clone(),
            module_path: vec![],
            references: output
                .cards
                .iter()
                .map(|c| Reference {
                    target_title: c.module_name.clone(),
                    target_path: format!(
                        "wiki/{}/{}.md",
                        config.wiki.language,
                        // 模块页写盘文件名 = module_path.join("_")（见 output::wiki_file_name），
                        // 链接必须与之一致，否则 TOC/概览出现断链
                        c.module_name.replace("::", "_")
                    ),
                    relation: "module".into(),
                })
                .collect(),
            last_updated: now,
            fingerprint: None,
        })
    }
}

/// 生成项目概览的 prompt（单条 user 消息，模板风格与 architecture_overview_prompt 一致）
///
/// 输入 = 模块列表（含职责描述）+ 卡片摘要（自底向上合成的一层：概览基于
/// 子模块的卡片摘要生成）+ 模块间依赖摘要，输出 = 技术栈 / 目录结构 / 核心模块。
fn overview_prompt(
    modules: &[crate::model::ModuleCluster],
    cards: &[crate::model::KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
) -> String {
    let mut parts = Vec::new();

    parts.push(format!(
        "你是一个资深软件架构师，负责为整个项目生成人类可读的项目概览文档。\n\n\
         请基于下面的模块聚类信息、各模块卡片摘要和模块间依赖摘要，输出以下结构：\n\n\
         # 项目概览\n\n\
         ## 技术栈\n根据模块名称与依赖关系推断项目使用的技术栈。\n\n\
         ## 目录结构\n根据模块划分描述仓库的目录结构。\n\n\
         ## 核心模块\n列出核心模块及其职责。\n\n\
         请用 {} 语言输出。保留 Markdown 格式。",
        config.wiki.language
    ));

    parts.push("## 模块列表".to_string());
    for module in modules {
        let desc = module.description.as_deref().unwrap_or("");
        parts.push(format!(
            "- {} (节点数: {}{})",
            module.name,
            module.node_ids.len(),
            if desc.is_empty() {
                String::new()
            } else {
                format!(", 职责: {}", desc)
            }
        ));
    }

    // 卡片摘要：模块级详情的浓缩（自底向上合成的输入）。
    // 每卡片一行：模块名 + 摘要 + 关键实体（实体名列表，数量多于描述输入）
    if !cards.is_empty() {
        parts.push("## 模块卡片摘要".to_string());
        for card in cards {
            let entities: Vec<&str> = card
                .key_entities
                .iter()
                .map(|e| e.name.as_str())
                .take(8)
                .collect();
            parts.push(format!(
                "- {}: {}（关键实体: {}）",
                card.module_name,
                card.summary,
                if entities.is_empty() {
                    "无".to_string()
                } else {
                    entities.join(", ")
                }
            ));
        }
    }

    // 模块间依赖摘要：建立 节点→模块 映射后，按模块对聚合非 Contains 边
    let mut module_of: std::collections::HashMap<NodeId, &str> = Default::default();
    for module in modules {
        for nid in &module.node_ids {
            module_of.insert(*nid, module.name.as_str());
        }
    }
    let mut deps: std::collections::BTreeMap<(String, String), usize> = Default::default();
    for edge in graph.graph.edge_weights() {
        if edge.kind == EdgeKind::Contains {
            continue;
        }
        let (Some(src), Some(dst)) = (module_of.get(&edge.source), module_of.get(&edge.target))
        else {
            continue;
        };
        *deps.entry((src.to_string(), dst.to_string())).or_default() += 1;
    }
    if deps.is_empty() {
        parts.push("\n## 模块间依赖\n（图中未检测到模块间依赖边）".to_string());
    } else {
        parts.push("\n## 模块间依赖".to_string());
        for ((src, dst), count) in deps {
            parts.push(format!("- {} → {} ({} 条边)", src, dst, count));
        }
    }

    parts.join("\n")
}

/// 从 Chunk 构建交叉引用
///
/// * `language` — 目标语言目录名，链接指向 `wiki/{language}/` 下的页面
fn build_references(chunk: &Chunk, language: &str) -> Vec<Reference> {
    chunk
        .dependencies
        .iter()
        .map(|dep| Reference {
            target_title: dep.clone(),
            target_path: format!(
                "wiki/{language}/{}.md",
                // 依赖模块页文件名与输出层 wiki_file_name 一致（"::" → "_"）
                dep.replace("::", "_")
            ),
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

    fn make_test_chunk() -> Chunk {
        let entity = Entity {
            name: "Server".into(),
            kind: "struct".into(),
            line_start: 1,
            line_end: 50,
            doc_comment: Some("HTTP 服务".into()),
            signature: None,
            summary: None,
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
            source: String::new(),
        };
        chunk_by_file(&insight)
    }

    #[tokio::test]
    async fn test_skip_empty_chunk() {
        let provider = MockProvider::new();
        let generator = WikiGenerator::new(&provider, None);
        let config = WikiConfig::default();
        let empty_chunk = Chunk {
            module_path: vec![],
            entities: vec![],
            imports: vec![],
            dependencies: vec![],
            file_paths: vec![],
            entity_sources: vec![],
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
            entity_sources: vec![],
        };

        let refs = build_references(&chunk, "zh");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].target_title, "tokio");
        assert_eq!(refs[0].target_path, "wiki/zh/tokio.md");
    }

    /// 链接路径与写盘文件名规则必须一致（"::" → "_"，与 output::wiki_file_name 相同），
    /// 否则 TOC/概览/模块互链全部断链（历史 bug：曾用 "::" → "/" 生成 wiki/zh/src/analysis.md
    /// 而实际写盘 src_analysis.md）
    #[test]
    fn test_build_references_uses_underscore_like_write_path() {
        let chunk = Chunk {
            module_path: vec!["src".into(), "generate".into()],
            entities: vec![],
            imports: vec![],
            dependencies: vec!["src::analysis".into(), "src::output".into()],
            file_paths: vec![],
            entity_sources: vec![],
        };

        let refs = build_references(&chunk, "zh");
        assert_eq!(refs[0].target_path, "wiki/zh/src_analysis.md");
        assert_eq!(refs[1].target_path, "wiki/zh/src_output.md");
    }

    fn make_hints_plan(title: &str) -> ResolvedPlan {
        use crate::config::plan::PlanDocument;
        ResolvedPlan {
            whitelist: Some(vec![PlanDocument {
                title: title.into(),
                goal: String::new(),
                parent: None,
                include_patterns: vec![],
                exclude_patterns: vec![],
                hints: Some("重点写服务启动流程".into()),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn test_whitelist_hints_injected_on_title_match() {
        // make_test_chunk（src/server.rs）的模块路径为 ["src"]，模块名为 "src"
        let chunk = make_test_chunk();
        let plan = make_hints_plan("src");
        let messages = prompt::wiki_page_prompt(&chunk, "摘要", "zh", Some(&plan));
        let user = &messages[1].content;
        assert!(user.contains("写作提示（用户指定）: 重点写服务启动流程"));
    }

    #[test]
    fn test_whitelist_hints_not_injected_on_title_mismatch() {
        let chunk = make_test_chunk();
        let plan = make_hints_plan("other");
        let messages = prompt::wiki_page_prompt(&chunk, "摘要", "zh", Some(&plan));
        let user = &messages[1].content;
        assert!(!user.contains("写作提示"));
    }

    /// 模块职责描述 prompt:含模块名与实体列表,并约束一行输出
    #[test]
    fn test_module_description_prompt_shape() {
        let messages = prompt::module_description_prompt(
            "src::net",
            &["connect".into(), "listen".into()],
            "zh",
        );
        let user = &messages[1].content;
        assert!(user.contains("src::net"), "应含模块名");
        assert!(user.contains("connect"), "应含实体名");
        assert!(user.contains("listen"), "应含实体名");
        assert!(messages[0].content.contains("30"), "zh 应约束 30 字内");
    }

    /// describe_modules:src 兜底模块跳过,带实体模块获得描述快照
    #[tokio::test]
    async fn test_describe_modules_enriches_description() {
        use crate::model::{CodeEdge, CodeNode, ModuleCluster, NodeId, NodeKind};
        use petgraph::stable_graph::StableDiGraph;

        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let f = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::File,
            name: "net.rs".into(),
            file_path: Some("src/net.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["src".into(), "net".into()],
        });
        let e = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "connect".into(),
            file_path: Some("src/net.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["src".into(), "net".into()],
        });
        let kg = KnowledgeGraph {
            graph: g,
            modules: vec![
                ModuleCluster {
                    name: "src::net".into(),
                    node_ids: vec![f, e],
                    cohesion: 0.5,
                    coupling: 0.5,
                    description: None,
                },
                ModuleCluster {
                    name: "src".into(),
                    node_ids: vec![],
                    cohesion: 1.0,
                    coupling: 0.0,
                    description: None,
                },
            ],
            features: Vec::new(),
        };
        let provider = MockProvider::new();
        let generator = WikiGenerator::new(&provider, None);
        let enriched = generator.describe_modules(&kg, "zh").await;
        assert_eq!(enriched.len(), 2);
        assert!(enriched[0].description.is_some(), "带实体的模块应获得描述");
        assert_eq!(enriched[1].name, "src");
        assert!(enriched[1].description.is_none(), "src 兜底模块不描述");
    }

    /// 概览自底向上合成:overview_prompt 注入卡片摘要段(模块名+摘要+关键实体)
    #[test]
    fn test_overview_prompt_includes_card_summaries() {
        use crate::model::KnowledgeCard;

        let graph = KnowledgeGraph::default();
        let config = WikiConfig::default();
        let card = KnowledgeCard {
            module_name: "src::net".into(),
            module_type: "module".into(),
            summary: "网络模块:连接管理与监听".into(),
            key_entities: vec![crate::model::EntitySummary {
                name: "connect".into(),
                kind: "function".into(),
                visibility: "public".into(),
                doc: None,
                source: None,
            }],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        };
        let prompt = overview_prompt(&[], &[card], &graph, &config);
        assert!(prompt.contains("## 模块卡片摘要"), "应含卡片摘要节");
        assert!(prompt.contains("src::net"), "应含模块名");
        assert!(prompt.contains("网络模块"), "应含卡片摘要");
        assert!(prompt.contains("connect"), "应含关键实体");
    }
}
