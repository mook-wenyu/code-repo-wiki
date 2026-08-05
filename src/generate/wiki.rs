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
    /// describe_modules 并发信号量（演进计划 T5.1：模块职责描述并行
    /// 生成时限制并发，避免 10+ 模块仓库一次性打爆 LLM API 限流）
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

/// 引用契约重试上限（P0-1）：生成后校验源码引用，无效时重试注入
/// 反馈，最多重试 CITATION_RETRY_MAX 次（共 CITATION_RETRY_MAX + 1 次
/// 调用）。"总是重试"但必须有上限防死循环（每次重试消耗 LLM 调用）。
pub const CITATION_RETRY_MAX: usize = 2;

/// Mermaid 语法校验重试上限（G2）：与引用契约对齐——首次调用 + 每次
/// 坏块反馈后重试，共 `MERMAID_RETRY_MAX + 1` 次调用；耗尽后降级
/// （坏块转 text + 标记注释，见 output::mermaid_check::degrade_mermaid_blocks）。
/// 与 CITATION_RETRY_MAX 合并进同一重试循环（上限取两者最大值）。
pub const MERMAID_RETRY_MAX: usize = crate::output::mermaid_check::MERMAID_RETRY_MAX;

impl<'a, P: LlmProvider> WikiGenerator<'a, P> {
    /// 使用指定的 LLM Provider 创建 WikiGenerator
    ///
    /// plan 为解析后的生效计划（无计划时传 None）。
    /// max_concurrent 控制 describe_modules 的并行上限（0 表示不限制）。
    pub fn new(provider: &'a P, plan: Option<ResolvedPlan>, max_concurrent: usize) -> Self {
        // tokio Semaphore 许可数有 MAX_PERMITS 上限（约 2^61），usize::MAX 会 panic；
        // "0=不限制" 用足够大的许可数表达（对真实并发规模永不构成瓶颈）
        let max = if max_concurrent == 0 { 1_000_000_000 } else { max_concurrent };
        Self {
            provider,
            call_count: AtomicUsize::new(0),
            plan,
            failed: std::sync::Mutex::new(Vec::new()),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(max)),
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
    ///
    /// P0-1 引用契约：生成后校验源码引用（文件存在 + 行号有效），
    /// 引用无效时重试（最多 `CITATION_RETRY_MAX` 次，用户决策"总是重试"）。
    /// 重试注入错误反馈（无效引用清单），最后一次仍失败时返回错误——
    /// 由调用方失败隔离（record_failure）记录，不产出无引用的页面。
    /// G2 Mermaid 契约：正文中的 Mermaid 代码块同样校验（merman 权威解析），
    /// 坏块错误消息注入重试反馈；重试耗尽后**降级**而非失败——坏块替换为
    /// text fence + 标记注释（OpenWiki degrade-and-repair），页面照常产出。
    pub async fn generate_wiki_page(
        &self,
        chunk: &Chunk,
        card_summary: &str,
        config: &WikiConfig,
        root: &crate::project::ProjectRoot,
        entity_ranges: Option<&crate::output::citation::EntityRanges>,
    ) -> Result<WikiDocument> {
        if chunk.is_empty() {
            anyhow::bail!("空块，跳过 Wiki 页面生成");
        }

        let language = &config.wiki.language;
        let mut messages =
            prompt::wiki_page_prompt(chunk, card_summary, language, self.plan.as_ref());
        let mut content = String::new();
        let mut last_invalid = Vec::new();
        let mut last_mermaid = Vec::new();

        // 重试循环：首次调用 + 每次校验失败后追加反馈重试（共 RETRY_MAX + 1 次调用）。
        // 引用与 Mermaid 校验共享同一循环（上限取两者最大值——当前均为 2），
        // 每次调用后两类校验都执行，任一失败都注入对应反馈。
        let retry_max = CITATION_RETRY_MAX.max(MERMAID_RETRY_MAX);
        for attempt in 0..=retry_max {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            content = self.provider.complete(&messages).await?;
            // 空/纯空白内容同样视为校验失败（重试语义：不产出空白页面）
            last_invalid = if content.trim().is_empty() {
                vec![crate::output::citation::InvalidCitation {
                    citation: crate::output::citation::Citation {
                        path: String::new(),
                        start: 0,
                        end: 0,
                    },
                    reason: "输出为空（未生成任何内容）".into(),
                }]
            } else {
                // v14 B 组（t03 拍板）：生产路径传实体行区间表 → 两级校验
                // （文件级 + 区间重叠）；None（测试/无表场景）退化为文件级。
                match entity_ranges {
                    Some(ranges) => {
                        crate::output::citation::validate_citations_against_entities(
                            root.path(),
                            &content,
                            ranges,
                        )
                    }
                    None => crate::output::citation::validate_citations(root.path(), &content),
                }
            };
            last_mermaid = crate::output::mermaid_check::validate_mermaid_blocks(&content);
            if last_invalid.is_empty() && last_mermaid.is_empty() {
                break;
            }
            if !last_invalid.is_empty() {
                tracing::warn!(
                    "Wiki 页面引用校验失败（第 {} 次，无效 {} 条）: {}",
                    attempt + 1,
                    last_invalid.len(),
                    chunk.module_path.join("::")
                );
            }
            if !last_mermaid.is_empty() {
                tracing::warn!(
                    "Wiki 页面 Mermaid 校验失败（第 {} 次，坏块 {} 个）: {}",
                    attempt + 1,
                    last_mermaid.len(),
                    chunk.module_path.join("::")
                );
            }
            // 重试机会已用完：跳出循环走收尾路径
            if attempt == retry_max {
                break;
            }
            if !last_invalid.is_empty() {
                messages.push(Message::user(
                    crate::output::citation::retry_feedback(&last_invalid),
                ));
            }
            if !last_mermaid.is_empty() {
                messages.push(Message::user(
                    crate::output::mermaid_check::mermaid_retry_feedback(&last_mermaid),
                ));
            }
        }

        // 引用校验：重试耗尽仍无效 → 失败（不产出无引用的页面）
        if !last_invalid.is_empty() {
            anyhow::bail!(
                "Wiki 页面引用校验失败（重试 {} 次仍无效，共 {} 条无效引用）: {}",
                retry_max,
                last_invalid.len(),
                chunk.module_path.join("::")
            );
        }
        // Mermaid 校验：重试耗尽仍坏 → 降级（坏块转 text + 注释），页面保留。
        // 降级注释含错误消息，供人工与下次 LLM 生成时参考修复（repair 语义）。
        if !last_mermaid.is_empty() {
            tracing::warn!(
                "Wiki 页面 Mermaid 重试耗尽（{} 个坏块），降级为 text 块: {}",
                last_mermaid.len(),
                chunk.module_path.join("::")
            );
            content = crate::output::mermaid_check::degrade_mermaid_blocks(&content, &last_mermaid);
        }

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

    /// 带 Mermaid 校验的 LLM 调用（G2）：架构/概览等全局文档的专用路径
    ///
    /// 这些页面由 LLM 自由生成，可能包含 Mermaid 图（架构图等）。调用后校验
    /// 正文 Mermaid 块，坏块错误消息注入重试反馈；重试耗尽后降级（坏块转
    /// text + 注释）而非失败——页面照常产出，坏图不出现在产物中。
    /// 与 generate_wiki_page 的区别：这里只校验 Mermaid（无引用契约——
    /// 全局文档不强制源码引用，与既有行为一致）。
    /// 实现委托自由函数 `complete_with_mermaid_guard_free`（U03/D1：
    /// Schema 文档复用同一校验-重试-降级路径），此处只负责调用计数。
    async fn complete_with_mermaid_guard(
        &self,
        messages: Vec<Message>,
        label: &str,
    ) -> Result<String> {
        complete_with_mermaid_guard_free(self.provider, messages, label, Some(&self.call_count))
            .await
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
        let language = &config.wiki.language;
        let modules = self.describe_modules(graph, language).await;
        let messages =
            prompt::architecture_overview_prompt(&modules, graph, language, self.plan.as_ref());
        // LLM 调用计数在 complete_with_mermaid_guard 内部（含重试）
        let content = self.complete_with_mermaid_guard(messages, "架构概览").await?;
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
        // 并行生成所有需描述的模块描述（保留输入顺序）；Semaphore 限制
        // 并发（演进计划 T5.1）：0=不限时许可数巨大永不会阻塞。
        let semaphore = self.semaphore.clone();
        let futures: Vec<_> = graph
            .modules
            .iter()
            .map(|module| {
                let semaphore = semaphore.clone();
                async move {
                    // 兜底模块(src)与空模块跳过：无职责边界可描述
                    if module.name == "src" || module.node_ids.is_empty() {
                        return module.clone();
                    }
                    let _permit = match semaphore.acquire().await {
                        Ok(p) => p,
                        Err(_) => return module.clone(),
                    };
                    let mut enriched = module.clone();
                    if let Ok(text) = self.describe_module(module, graph, language).await
                        && !text.trim().is_empty()
                    {
                        enriched.description = Some(text.trim().to_string());
                    }
                    enriched
                }
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
        // 与 generate_architecture 一致：先补模块职责描述，概览内容才能
        // 表达"模块负责什么"；再叠加卡片摘要（自底向上合成：父概览基于
        // 子模块的职责描述 + 卡片摘要生成，而非仅模块名/节点数/边计数）
        // LLM 调用计数在 complete_with_mermaid_guard 内部（含重试）
        let modules = self.describe_modules(graph, &config.wiki.language).await;
        let messages = vec![Message::user(overview_prompt(&modules, &output.cards, graph, config))];
        let content = self.complete_with_mermaid_guard(messages, "项目概览").await?;
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

/// 带 Mermaid 校验的 LLM 调用（自由函数版，U03/D1 提取）
///
/// 与 WikiGenerator::complete_with_mermaid_guard 等价，但直接接受
/// provider 与可选调用计数指针——Schema 文档生成（generate/schema.rs）
/// 只有 Provider 没有 WikiGenerator，此前直接 complete 绕过校验（唯一
/// 强制 erDiagram 输出的文档类型反而无校验-重试-降级，D1）。
pub async fn complete_with_mermaid_guard_free<P: LlmProvider>(
    provider: &P,
    mut messages: Vec<Message>,
    label: &str,
    call_count: Option<&std::sync::atomic::AtomicUsize>,
) -> Result<String> {
    use std::sync::atomic::Ordering;
    let mut content = String::new();
    let mut last_mermaid = Vec::new();

    for attempt in 0..=MERMAID_RETRY_MAX {
        if let Some(c) = call_count {
            c.fetch_add(1, Ordering::Relaxed);
        }
        content = provider.complete(&messages).await?;
        last_mermaid = crate::output::mermaid_check::validate_mermaid_blocks(&content);
        if last_mermaid.is_empty() {
            return Ok(content);
        }
        tracing::warn!(
            "{label} Mermaid 校验失败（第 {} 次，坏块 {} 个）",
            attempt + 1,
            last_mermaid.len()
        );
        if attempt == MERMAID_RETRY_MAX {
            break;
        }
        messages.push(Message::user(
            crate::output::mermaid_check::mermaid_retry_feedback(&last_mermaid),
        ));
    }

    tracing::warn!("{label} Mermaid 重试耗尽（{} 个坏块），降级为 text 块", last_mermaid.len());
    Ok(crate::output::mermaid_check::degrade_mermaid_blocks(
        &content,
        &last_mermaid,
    ))
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

/// 确定性架构/概览骨架（U06/D12）：provider 失败时降级而非丢页
///
/// 内容 = 模块列表（名 + 实体数 + 依赖清单），全部来自 knowledge graph
/// （零 LLM 调用），与 index.md 的确定性骨架同一思路：页面存在性优先，
/// LLM 摘要可在下次成功生成时补齐。kind/title 由调用方传入（架构概览与
/// 项目概览共用，避免两份骨架代码漂移）。
pub fn fallback_architecture_doc(
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    kind: DocumentKind,
    title: &str,
) -> WikiDocument {
    use petgraph::visit::{EdgeRef, IntoEdgeReferences};
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    // 实体节点 → 所属模块（先到先得，与 index.rs/export_modules 同规则）
    let mut node_module: HashMap<NodeId, String> = HashMap::new();
    for module in &graph.modules {
        for nid in &module.node_ids {
            node_module
                .entry(*nid)
                .or_insert_with(|| module.name.clone());
        }
    }
    // 模块 → 依赖模块名集合（Calls + Imports 跨模块边，BTreeSet 字典序）
    let mut deps: BTreeMap<String, BTreeSet<String>> = Default::default();
    for edge in graph.graph.edge_references() {
        if matches!(
            graph.graph[edge.id()].kind,
            EdgeKind::Calls | EdgeKind::Imports
        ) {
            let (Some(src), Some(tgt)) = (
                node_module.get(&edge.source()),
                node_module.get(&edge.target()),
            ) else {
                continue;
            };
            if src != tgt {
                deps.entry(src.clone()).or_default().insert(tgt.clone());
            }
        }
    }

    let mut body = format!(
        "# {title}\n\n> LLM 生成不可用，本页为确定性骨架：模块与依赖关系由知识图谱自动生成（无 LLM 摘要）。\n\n## 模块\n\n"
    );
    for module in &graph.modules {
        body.push_str(&format!("- `{}`（{} 个实体）", module.name, module.node_ids.len()));
        if let Some(dl) = deps.get(&module.name)
            && !dl.is_empty()
        {
            body.push_str(&format!(" — 依赖 {}", dl.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        body.push('\n');
    }

    let mut refs: Vec<Reference> = graph
        .modules
        .iter()
        .map(|m| Reference {
            target_title: m.name.clone(),
            target_path: format!(
                "wiki/{}/{}.md",
                config.wiki.language,
                m.name.replace("::", "_")
            ),
            relation: "module".into(),
        })
        .collect();
    // 按目标标题字典序，保证输出确定性
    refs.sort_by(|a, b| a.target_title.cmp(&b.target_title));

    WikiDocument {
        title: title.to_string(),
        kind,
        content: body,
        language: config.wiki.language.clone(),
        module_path: vec![],
        references: refs,
        last_updated: chrono::Utc::now().to_rfc3339(),
        fingerprint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::chunk::chunk_by_file;
    use crate::generate::llm::MockProvider;
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    
    use std::path::PathBuf;

    /// 可编程 mock：按调用次数依次返回预设响应（引用重试测试用）
    struct ScriptedProvider {
        responses: std::sync::Mutex<std::vec::IntoIter<String>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into_iter()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl LlmProvider for ScriptedProvider {
        async fn complete(&self, _messages: &[Message]) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.responses
                .lock()
                .unwrap()
                .next()
                .ok_or_else(|| anyhow::anyhow!("预设响应耗尽"))
        }
    }

    fn make_test_chunk() -> Chunk {
        let entity = Entity {
            name: "Server".into(),
            kind: "struct".into(),
            line_start: 1,
            line_end: 50,
            doc_comment: Some("HTTP 服务".into()),
            signature: None, visibility: None,
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
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let root = crate::project::ProjectRoot::new(std::env::temp_dir());
        let empty_chunk = Chunk {
            module_path: vec![],
            entities: vec![],
            imports: vec![],
            dependencies: vec![],
            file_paths: vec![],
            entity_sources: vec![],
        };

        let result = generator.generate_wiki_page(&empty_chunk, "", &config, &root, None).await;
        assert!(result.is_err());
    }

    /// 引用契约重试：无效引用 → 重试注入反馈 → 第二次输出有效引用则成功
    #[tokio::test]
    async fn test_wiki_page_retries_on_invalid_citation() {
        // 临时目录放一个真实文件 src/server.rs（3 行），使有效引用通过校验
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_retry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("server.rs"), "pub struct Server;\n// comment\n").unwrap();
        let root = crate::project::ProjectRoot::new(dir.clone());

        // 第一次输出引用不存在的文件，第二次输出有效引用
        let provider = ScriptedProvider::new(vec![
            "模块职责是管理连接。核心实体 `Server` 定义见 nonexistent.rs:99。".to_string(),
            "模块职责是管理连接。核心实体 `Server` 定义见 src/server.rs:1。".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator.generate_wiki_page(&chunk, "摘要", &config, &root, None).await.unwrap();
        assert!(doc.content.contains("src/server.rs:1"), "重试后应使用有效引用");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2, "应调用 2 次（1 次失败 + 1 次重试）");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 引用契约重试耗尽：超过 CITATION_RETRY_MAX 仍无效 → 报错
    #[tokio::test]
    async fn test_wiki_page_bails_when_citations_never_valid() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_fail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = crate::project::ProjectRoot::new(dir.clone());

        // 全部输出无效引用（文件不存在）
        let provider = ScriptedProvider::new(vec![
            "引用 nonexistent.rs:99".to_string(),
            "引用 nonexistent.rs:99".to_string(),
            "引用 nonexistent.rs:99".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let result = generator.generate_wiki_page(&chunk, "摘要", &config, &root, None).await;
        assert!(result.is_err(), "重试耗尽后应报错");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("引用校验失败"), "错误信息应说明引用校验失败: {err}");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::Relaxed),
            CITATION_RETRY_MAX + 1,
            "应调用 CITATION_RETRY_MAX+1 次后放弃"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 引用契约放行：无引用的输出直接通过（契约只惩罚编造引用，不强制必须有）
    #[tokio::test]
    async fn test_wiki_page_without_citations_passes() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_ok_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = crate::project::ProjectRoot::new(dir.clone());

        let provider = ScriptedProvider::new(vec!["模块职责是管理连接。".to_string()]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator.generate_wiki_page(&chunk, "摘要", &config, &root, None).await.unwrap();
        assert_eq!(doc.content, "模块职责是管理连接。");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1, "无引用无需重试");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// G2 Mermaid 契约重试：坏图 → 重试注入错误反馈 → 第二次输出好图则成功
    #[tokio::test]
    async fn test_wiki_page_retries_on_bad_mermaid() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_mermaid_retry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = crate::project::ProjectRoot::new(dir.clone());

        // 第一次输出坏图（标签未闭合），第二次输出好图
        let provider = ScriptedProvider::new(vec![
            "```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n".to_string(),
            "```mermaid\nflowchart LR\nA[Start] --> B[End]\n```\n".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator.generate_wiki_page(&chunk, "摘要", &config, &root, None).await.unwrap();
        assert!(doc.content.contains("A[Start] --> B[End]"), "重试后应保留好图");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2, "应调用 2 次（1 次坏图 + 1 次重试）");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 B 组：区间重叠校验——文件存在且行号有效但引用区间不覆盖任何实体
    /// （行号对但内容错）→ 校验失败 → 重试反馈注入 → 修正后成功
    #[tokio::test]
    async fn test_wiki_page_retries_on_overlap_citation() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_overlap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 10 行文件：实体区间 (2,4)（fn Server 定义）
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(src.join("server.rs"), content).unwrap();
        let root = crate::project::ProjectRoot::new(dir.clone());

        let mut ranges: crate::output::citation::EntityRanges =
            crate::output::citation::EntityRanges::new();
        ranges.insert("src/server.rs".to_string(), vec![(2, 4)]);

        // 第一次输出引用 8 行（文件内但不在实体区间），第二次输出 2 行（覆盖实体）
        let provider = ScriptedProvider::new(vec![
            "模块职责是管理连接。核心实体 `Server` 定义见 src/server.rs:8。".to_string(),
            "模块职责是管理连接。核心实体 `Server` 定义见 src/server.rs:2。".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator
            .generate_wiki_page(&chunk, "摘要", &config, &root, Some(&ranges))
            .await
            .unwrap();
        assert!(doc.content.contains("src/server.rs:2"), "重试后应使用覆盖实体的引用");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "区间外引用应触发一次重试"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 B 组：区间重叠校验重试耗尽 → 页面失败（t03 拍板：维持 bail，
    /// 与文件级引用校验同一失败语义，Mermaid 才是唯一降级路径）
    #[tokio::test]
    async fn test_wiki_page_bails_when_overlap_never_valid() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_overlap_fail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(src.join("server.rs"), content).unwrap();
        let root = crate::project::ProjectRoot::new(dir.clone());

        let mut ranges: crate::output::citation::EntityRanges =
            crate::output::citation::EntityRanges::new();
        ranges.insert("src/server.rs".to_string(), vec![(2, 4)]);

        // 全部输出区间外引用（8 行）
        let provider = ScriptedProvider::new(vec![
            "核心实体 `Server` 见 src/server.rs:8。".to_string(),
            "核心实体 `Server` 见 src/server.rs:8。".to_string(),
            "核心实体 `Server` 见 src/server.rs:8。".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let result = generator
            .generate_wiki_page(&chunk, "摘要", &config, &root, Some(&ranges))
            .await;
        assert!(result.is_err(), "区间重叠校验重试耗尽应报错");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("引用校验失败"), "错误信息应说明引用校验失败: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 B 组：无实体文件（README.md 等非代码文件）的引用放行——区间
    /// 校验只对有实体的文件生效（引用配置/说明文件是合法行为）
    #[tokio::test]
    async fn test_wiki_page_passes_non_code_file_citation() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_cite_noncode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "docs\n").unwrap();
        let root = crate::project::ProjectRoot::new(dir.clone());

        // 实体表不含 README.md（无实体）
        let ranges: crate::output::citation::EntityRanges =
            crate::output::citation::EntityRanges::new();
        let provider = ScriptedProvider::new(vec![
            "模块说明见 README.md:1。".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator
            .generate_wiki_page(&chunk, "摘要", &config, &root, Some(&ranges))
            .await
            .unwrap();
        assert!(doc.content.contains("README.md:1"), "无实体文件引用应放行");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1, "无需重试");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// G2 Mermaid 契约重试耗尽：超过 MERMAID_RETRY_MAX 仍坏图 → 降级而非失败
    /// （坏块替换为 text fence + 标记注释，页面照常产出）
    #[tokio::test]
    async fn test_wiki_page_degrades_when_mermaid_never_valid() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_test_mermaid_degrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = crate::project::ProjectRoot::new(dir.clone());

        // 连续 3 次（MERMAID_RETRY_MAX + 1）都输出坏图
        let provider = ScriptedProvider::new(vec![
            "```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n".to_string(),
            "```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n".to_string(),
            "```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let chunk = make_test_chunk();

        let doc = generator.generate_wiki_page(&chunk, "摘要", &config, &root, None).await.unwrap();
        assert!(!doc.content.contains("```mermaid"), "坏图不应再以 mermaid 块出现");
        assert!(doc.content.contains("```text"), "坏块应降级为 text fence");
        assert!(doc.content.contains("repo-wiki: mermaid parse failed"), "应含降级标记注释");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::Relaxed),
            MERMAID_RETRY_MAX + 1,
            "应调用 MERMAID_RETRY_MAX+1 次后降级"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// G2 架构概览：坏图重试耗尽同样降级，页面正常产出
    #[tokio::test]
    async fn test_architecture_degrades_on_bad_mermaid() {
        let provider = ScriptedProvider::new(vec![
            "```mermaid\nflowchart LR\nA[hello world\n```\n".to_string(),
            "```mermaid\nflowchart LR\nA[hello world\n```\n".to_string(),
            "```mermaid\nflowchart LR\nA[hello world\n```\n".to_string(),
        ]);
        let generator = WikiGenerator::new(&provider, None, 0);
        let config = WikiConfig::default();
        let graph = crate::model::KnowledgeGraph::default();
        let output = crate::generate::GenerationOutput {
            cards: vec![],
            documents: vec![],
            generation_stats: crate::generate::GenerationStats::default(),
        };

        let doc = generator.generate_architecture(&output, &graph, &config).await.unwrap();
        assert!(!doc.content.contains("```mermaid"), "坏图不应再以 mermaid 块出现");
        assert!(doc.content.contains("repo-wiki: mermaid parse failed"), "应含降级标记注释");
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
            signature: None, visibility: None,
            module_path: vec!["src".into(), "net".into()],
        });
        let e = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "connect".into(),
            file_path: Some("src/net.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
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
        let generator = WikiGenerator::new(&provider, None, 0);
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
