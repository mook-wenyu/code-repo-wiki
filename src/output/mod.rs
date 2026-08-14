//! 渲染与导出层（单进程契约：export_snapshot.json 等状态文件无锁，
//! 同一输出目录并发运行不被支持，见 README 限制项）
pub mod citation;
pub mod crossref;
pub mod dependency_check;
pub mod html;
pub mod lint;
pub mod llms_txt;
pub mod markdown;
pub mod mermaid;
pub mod mermaid_check;
pub mod residue_check;
pub mod semantic_lint;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::markdown::write_document;

/// 主语言（v51 命名卫生：v30 起 expand_languages 已删除，恒单语言，
/// 旧名 wiki_languages 残留多语言语义误导调用方）。
///
/// 返回 `config.wiki.language` 单值；所有调用点按单值消费（生成/渲染/
/// 指纹记录/索引均只处理主语言）。由 generate::collect_languages 移入
/// （消除 output→generate 反向依赖；generate 侧调用点改向本函数——
/// generate→output 依赖本就存在，card.rs 已用）。
pub fn primary_language(config: &WikiConfig) -> String {
    config.wiki.language.clone()
}

/// API 参考页写盘路径：`{}/wiki/{lang}/api.md`（每种语言独立一份）
///
/// render_all 写盘与状态层指纹记录共用本函数产出路径，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致（同一规则，防止两处漂移）。
pub fn api_doc_path(output_dir: &Path, lang: &str) -> PathBuf {
    output_dir.join("wiki").join(lang).join("api.md")
}

/// 概览页写盘路径：`{}/wiki/{lang}/overview.md`（仅主语言一份）
pub fn overview_doc_path(output_dir: &Path, lang: &str) -> PathBuf {
    output_dir.join("wiki").join(lang).join("overview.md")
}

/// 目录页写盘路径：`{}/_toc.md`（输出目录根一份）
pub fn toc_doc_path(output_dir: &Path) -> PathBuf {
    output_dir.join("_toc.md")
}

/// 卡片文件名主体（不含 .md 后缀）：`module.replace("::", "_")`
///
/// 卡片写盘、卡片索引与删除清理共用本函数，保证卡片命名单一来源。
pub(crate) fn card_file_stem(module: &str) -> String {
    module.replace("::", "_")
}

/// 卡片写盘路径：`{}/cards/{lang}/{module.replace("::","_")}.md`
///
/// render_all 写盘、卡片指纹记录与删除清理共用本函数产出路径，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致（防止两处漂移）。
pub(crate) fn card_page_path(output_dir: &Path, lang: &str, module: &str) -> PathBuf {
    output_dir
        .join("cards")
        .join(lang)
        .join(format!("{}.md", card_file_stem(module)))
}

/// Wiki 页面写盘路径：`{}/wiki/{lang}/{file}.md`
///
/// 文件名复用 markdown::wiki_file_name（ArchitectureOverview 特判写 architecture.md）。
/// render_all 写盘、write_document 落盘与状态层指纹记录共用本函数，
/// 保证人工修改保护的判定路径与指纹记录路径完全一致。
pub(crate) fn wiki_page_path(output_dir: &Path, lang: &str, doc: &WikiDocument) -> PathBuf {
    if doc.kind == crate::model::DocumentKind::ArchitectureOverview {
        output_dir.join("wiki").join(lang).join("architecture.md")
    } else if doc.kind == crate::model::DocumentKind::ProjectOverview {
        output_dir.join("wiki").join(lang).join("overview.md")
    } else {
        output_dir
            .join("wiki")
            .join(lang)
            .join(markdown::wiki_file_name(doc))
    }
}

/// Wiki 页面 HTML 写盘路径：`{}/wiki/{lang}/{file}.html`
///
/// 与 wiki_page_path 同构（命名规则完全一致，仅扩展名 .md → .html），
/// 保证 HTML 导出与 markdown 产物一一对应（多语言同名标题不冲突）。
pub fn wiki_page_html_path(output_dir: &Path, doc: &WikiDocument) -> PathBuf {
    wiki_page_path(output_dir, &doc.language, doc).with_extension("html")
}

/// 导出快照中的模块摘要（快照 JSON 的 modules 项）
///
/// name/cohesion/coupling 直接来自 graph.modules；files 由模块节点反查
/// file_path 派生（去重排序）；features 取同模块卡片的特征追溯列表
/// （生成层 backfill_features 已按模块回填，无需重算交集）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExportModuleSnapshot {
    pub name: String,
    pub files: Vec<String>,
    pub cohesion: f64,
    pub coupling: f64,
    pub features: Vec<String>,
    /// 依赖的模块名列表（U05/D9：module-deps.html 此前只有节点零边——
    /// 快照无依赖字段，HTML 侧画不出依赖边；serde default 兼容旧快照）
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// 导出快照（`{output_dir}/.state/export_snapshot.json`）
///
/// 对外契约：main.rs 的 export --skip-generate 直接消费本文件，
/// 不再重跑生成流水线。documents/cards 为本次完整生成集（含受保护
/// 跳过写盘的文档——磁盘保留人工版，快照记录的是生成意图集）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExportSnapshot {
    pub version: u32,
    pub documents: Vec<WikiDocument>,
    pub cards: Vec<KnowledgeCard>,
    pub modules: Vec<ExportModuleSnapshot>,
}

/// 导出快照写盘路径：`{output_dir}/.state/export_snapshot.json`
/// （与 generation_state.json 同目录，沿用既有状态目录约定）
pub fn export_snapshot_path(output_dir: &Path) -> PathBuf {
    output_dir.join(".state").join("export_snapshot.json")
}

/// 全部语言目录下 wiki 页的最新修改时间（票 04 陈旧检测用）
///
/// 遍历 `wiki/{lang}/*.md`（主语言 + 扩展语言），返回最新 mtime；
/// 无任何页面时返回 None（此时快照也必然不存在，陈旧检测不适用）。
pub fn latest_wiki_page_mtime(output_dir: &Path) -> Option<std::time::SystemTime> {
    let wiki_root = output_dir.join("wiki");
    let mut latest: Option<std::time::SystemTime> = None;
    let Ok(entries) = std::fs::read_dir(&wiki_root) else {
        return None;
    };
    for lang in entries.flatten() {
        if !lang.path().is_dir() {
            continue;
        }
        let Ok(pages) = std::fs::read_dir(lang.path()) else {
            continue;
        };
        for page in pages.flatten() {
            let path = page.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(meta) = std::fs::metadata(&path)
                && let Ok(mtime) = meta.modified()
            {
                latest = Some(match latest {
                    Some(prev) => prev.max(mtime),
                    None => mtime,
                });
            }
        }
    }
    latest
}

/// 从图与卡片提取快照模块列表（按模块名排序保证确定性）
pub fn export_modules(
    graph: &KnowledgeGraph,
    cards: &[KnowledgeCard],
) -> Vec<ExportModuleSnapshot> {
    // U05/D9：实体节点 → 所属模块映射（先到先得，与 index.rs/community
    // 的同规则），用于聚合跨模块依赖边（Calls + Imports，排除 Contains）
    use petgraph::visit::{EdgeRef, IntoEdgeReferences};
    use std::collections::{BTreeMap, BTreeSet};

    let mut node_module: std::collections::HashMap<crate::model::NodeId, String> =
        std::collections::HashMap::new();
    for module in &graph.modules {
        for nid in &module.node_ids {
            node_module
                .entry(*nid)
                .or_insert_with(|| module.name.clone());
        }
    }
    let mut deps: BTreeMap<String, BTreeSet<String>> = Default::default();
    for edge in graph.graph.edge_references() {
        if matches!(
            graph.graph[edge.id()].kind,
            crate::model::EdgeKind::Calls | crate::model::EdgeKind::Imports
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

    let mut modules: Vec<ExportModuleSnapshot> = graph
        .modules
        .iter()
        .map(|m| {
            let mut files: Vec<String> = m
                .node_ids
                .iter()
                .filter_map(|nid| {
                    graph
                        .graph
                        .node_weight(*nid)
                        .and_then(|n| n.file_path.clone())
                })
                .collect();
            files.sort();
            files.dedup();
            let features = cards
                .iter()
                .find(|c| c.module_name == m.name)
                .map(|c| c.features.clone())
                .unwrap_or_default();
            let mut dependencies: Vec<String> = deps
                .get(&m.name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            dependencies.sort();
            ExportModuleSnapshot {
                name: m.name.clone(),
                files,
                cohesion: m.cohesion,
                coupling: m.coupling,
                features,
                dependencies,
            }
        })
        .collect();
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    modules
}

/// 写导出快照到 `.state/export_snapshot.json`
fn write_export_snapshot(
    output_dir: &Path,
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
) -> Result<()> {
    let snapshot = ExportSnapshot {
        version: 1,
        documents: documents.to_vec(),
        cards: cards.to_vec(),
        modules: export_modules(graph, cards),
    };
    let path = export_snapshot_path(output_dir);
    // 原子写（fs::write_file_atomic）：写入失败不残留半截快照；
    // 陈旧检测（mtime 比对）在 export --skip-generate 侧，见票 04
    crate::fs::write_file_atomic(&path, &serde_json::to_string_pretty(&snapshot)?)?;
    Ok(())
}

/// 本次生成的产物路径集合（供增量清理 diff 使用）
///
/// 语义：render_all 本次生成意图写入的全部文件路径，**含受保护跳过写盘
/// 的文档**（保护文档属于生成集，磁盘上是人工版；清理 diff 以本集合
/// 为准时，保护路径天然被排除在待删集合外，不会误删人工编辑内容）。
/// 路径与 render_all 写盘规则逐一对应：wiki 页按 doc.language 落盘、
/// 关联卡片同语言落盘（精确关联）、api.md 只写主语言、_toc.md 写产物根。
pub fn rendered_paths(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    config: &WikiConfig,
) -> Vec<PathBuf> {
    let output_dir = config.output_dir();
    let mut paths: std::collections::BTreeSet<PathBuf> = Default::default();
    for doc in documents {
        paths.insert(wiki_page_path(output_dir, &doc.language, doc));
        let doc_module = doc.module_path.join("::");
        for card in cards {
            if card.module_name == doc_module {
                paths.insert(card_page_path(output_dir, &doc.language, &card.module_name));
            }
        }
    }
    paths.insert(api_doc_path(output_dir, &config.wiki.language));
    paths.insert(toc_doc_path(output_dir));
    paths.into_iter().collect()
}

/// 渲染所有文档到输出目录
///
/// 1. 创建输出目录结构（主语言 + 扩展语言）
/// 2. 按文档自身语言渲染并写入 Wiki 页面（多语言独立生成，不再按语言循环复制；
///    项目概览与架构概览由生成层产出，经 wiki_page_path 特判写 overview.md / architecture.md）
/// 3. 渲染并写入 Knowledge Card
/// 4. 生成 API 参考页（只写主语言）与目录页
/// 5. 生成 Mermaid 关系图
///
/// `protected` 为人工修改保护集（路径字符串），命中路径跳过写盘，
/// 覆盖 Wiki 页面与三个全局文档（api.md / overview.md / _toc.md）。
/// v17 t06：mock provider 占位页脚标注（产物可辨识，防误读为真实文档）。
/// 单一来源：lib.rs（LLM 文档）与 render_all（合成页 api.md）共用。
pub const MOCK_FOOTER_MARK: &str = "\n\n<!-- 本页由 mock provider 生成，非真实内容 -->\n";

/// 当前配置是否为 mock provider（占位页脚注入判定）
fn is_mock_provider(config: &WikiConfig) -> bool {
    matches!(
        config.llm.provider,
        crate::config::schema::LlmProviderType::Mock
    )
}

pub fn render_all(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    protected: &std::collections::HashSet<String>,
) -> Result<()> {
    let output_dir = config.output_dir();
    let assets_dir = output_dir.join("assets");
    let primary_lang = primary_language(config);

    // 创建主语言目录（v30 起多语言已删除，恒单语言；目录结构保持稳定）
    std::fs::create_dir_all(output_dir.join("wiki").join(&primary_lang))?;
    std::fs::create_dir_all(output_dir.join("cards").join(&primary_lang))?;
    std::fs::create_dir_all(&assets_dir)?;

    // 1. 写入 Wiki 页面（按文档自身语言分组写入对应目录）
    for doc in documents {
        // 路径计算与 write_document 落盘共用 wiki_page_path（人工修改保护判定依据）
        let wiki_path = wiki_page_path(output_dir, &doc.language, doc);
        if protected.contains(&wiki_path.to_string_lossy().to_string()) {
            // 页面受人工修改保护：跳过页面写盘（保留人工版）。卡片写盘
            // 已移至下方独立循环（v22 修复），不在此处处理。
            continue;
        }
        write_document(doc, output_dir, &doc.language)?;
    }

    // 1.3 独立写入 Knowledge Card（v22 修复：原卡片写盘绑定在 write_document
    // 内，模块页面 LLM 生成失败时卡片一并丢失，产出「快照/_index 有、磁盘
    // 无」的不一致（Unity 实测：52 卡仅 42 页对应卡落盘）。卡片与页面解耦：
    // 无论页面是否生成成功，卡片都按语言目录全量落盘；受人工修改保护的
    // 卡片跳过（保留人工版）。卡片仅主语言生成一次（generate_all_cards 以
    // 主语言调用），各语言目录写同一份内容——与旧实现语义一致。
    for card in cards {
        let card_path = card_page_path(output_dir, &primary_lang, &card.module_name);
        if protected.contains(&card_path.to_string_lossy().to_string()) {
            continue;
        }
        crate::fs::write_file_atomic(&card_path, &markdown::render_knowledge_card(card))?;
    }

    // 1.5 写入 API 参考页（按模块分组的实体清单；内容与语言无关，只写主语言一份；
    // 命中保护集跳过写盘。指纹记录按同一规则：state.rs 对未落盘的 en/api.md 不记指纹）
    let api_path = api_doc_path(output_dir, &primary_lang);
    if !protected.contains(&api_path.to_string_lossy().to_string()) {
        let api_doc = markdown::render_api_reference(graph);
        // v17 t06：mock 模式下合成页（api.md 非 LLM 文档，不走 lib.rs 的
        // documents 注入路径）同样追加占位页脚，标注点保持单一来源
        // MOCK_FOOTER_MARK，与 lib.rs 注入一致
        let content = if is_mock_provider(config) {
            format!("{}{}", api_doc.content, MOCK_FOOTER_MARK)
        } else {
            api_doc.content
        };
        crate::fs::write_file_atomic(&api_path, &content)?;
    }

    // 3. 写入 Knowledge Card 索引（JSON 格式，写入主语言目录）
    let cards_index_json = serde_json::json!({
        "version": "1.0",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "cards": cards.iter().map(|c| {
            serde_json::json!({
                "name": card_file_stem(&c.module_name),
                "title": c.module_name,
                "path": format!("cards/{}/{}.md", primary_lang, card_file_stem(&c.module_name)),
            })
        }).collect::<Vec<_>>(),
    });
    let cards_index = output_dir
        .join("cards")
        .join(&primary_lang)
        .join("_index.json");
    crate::fs::write_file_atomic(
        &cards_index,
        &serde_json::to_string_pretty(&cards_index_json)?,
    )?;

    // 4. 生成目录页（命中保护集跳过写盘）
    let toc_path = toc_doc_path(output_dir);
    if !protected.contains(&toc_path.to_string_lossy().to_string()) {
        let toc = markdown::render_table_of_contents(documents);
        crate::fs::write_file_atomic(&toc_path, &toc)?;
    }

    // 4.1 llms.txt（v14 E 组，t07 拍板）：Agent 站点地图（llmstxt.org
    // 规范），列出全部模块页/全局文档/卡片路径。确定性重生成产物，
    // 不参与人工修改保护（与 _toc.md 的人工编辑语义不同）；写失败
    // 仅告警——机器消费索引是辅助产物，缺了不破坏 Wiki 主体。
    if let Err(e) = llms_txt::write_llms_txt(output_dir, documents, cards, config) {
        // t05（v21）：llms.txt 是外部 Agent 的入口文件（站点地图），缺失
        // 会静默削弱 Agent 的发现路径——失败必须显式说明影响面。
        tracing::warn!(
            "llms.txt 写入失败（Agent 入口文件缺失，搜索类 Agent 将无法发现本 Wiki）: {}",
            e
        );
    }

    // 4.2 llms-full.txt（v19 t05）：模块职责 + 实体清单内联索引
    // （llms.txt 的超集，单次读取即获完整骨架；32K token 预算裁剪）。
    // 与 llms.txt 同生命周期语义：确定性重生成、不参与人工修改保护、
    // 写失败仅告警。
    if let Err(e) = llms_txt::write_llms_full_txt(output_dir, cards, config) {
        tracing::warn!("llms-full.txt 写入失败: {}", e);
    }

    // 4.3 architecture-map.md（v0.9 W2）：预构建架构知识——确定性合成
    // 的常驻小地图（模块职责 + 模块级依赖），供 Agent 少调工具快速答。
    // 路径精确为 `wiki/{主语言}/architecture-map.md`（install 注入块与
    // AGENTS.md 已按此路径引用，路径写错 agent 会读空文件）。职责复用
    // module_descriptions.json 现有 LLM 缓存，依赖来自知识图谱静态聚合，
    // 均不新增 LLM 调用。确定性重生成产物，不参与人工修改保护；写盘
    // 失败向上传播（Agent 导航入口缺失是实质缺陷，非辅助产物）。
    let arch_map_path = output_dir
        .join("wiki")
        .join(&primary_lang)
        .join("architecture-map.md");
    let descriptions =
        crate::analysis::architecture_map::load_module_descriptions(output_dir, &primary_lang);
    let arch_map = crate::analysis::architecture_map::render_architecture_map(graph, &descriptions);
    crate::fs::write_file_atomic(&arch_map_path, &arch_map)?;

    // 5. 生成 Mermaid 依赖图
    let diagrams_dir = assets_dir.join("diagrams");
    std::fs::create_dir_all(&diagrams_dir)?;
    let mermaid_content = mermaid::render_module_dependency_graph(graph);
    crate::fs::write_file_atomic(&diagrams_dir.join("module-deps.mermaid"), &mermaid_content)?;

    // 5.1 模块级调用关系图（Calls 边按模块聚合）
    let call_graph_content = mermaid::render_module_call_graph(graph);
    crate::fs::write_file_atomic(
        &diagrams_dir.join("call-graph.mermaid"),
        &call_graph_content,
    )?;

    tracing::info!(
        "输出完成: {} 个页面, {} 个卡片, {} 个模块, 目录: {}",
        documents.len(),
        cards.len(),
        graph.modules.len(),
        config.output_dir().display()
    );

    // AGENTS.md 引导文件：不存在才生成（幂等），人工已有 AGENTS.md 时跳过。
    // A2（v14）：写失败显式告警——此前 `let _` 静默吞掉（与下方导出快照
    // 写失败的 warn 语义对齐；引导文件是辅助产物，失败不中断渲染，但
    // 必须可观测，否则用户以为生成了导航入口实际没有）。
    if let Err(e) = generate_agents_md(output_dir) {
        tracing::warn!("AGENTS.md 引导文件生成失败: {}", e);
    }

    // 6. 写盘完成后同步导出快照（export --skip-generate 消费的对外契约）。
    //    辅助产物：写入失败仅告警不中断——快照缺失时 export --skip-generate
    //    会明确报错，属可观测性契约内，非兜底。
    if let Err(e) = write_export_snapshot(output_dir, documents, cards, graph) {
        tracing::warn!("导出快照写入失败: {}", e);
    }
    Ok(())
}

/// 生成 AGENTS.md 引导文件（AI 代理导航入口，repositories-wiki 最佳实践）
///
/// 写入当前工作目录（与 scan_and_parse 的扫描根一致）：若已存在则跳过
/// （尊重人工维护的 AGENTS.md，不覆盖）；内容指向 wiki 产物目录并说明
/// AI 代理如何消费（搜索命令、卡片格式、更新流程、lint 门禁）。
/// 返回是否生成了文件（false = 已存在跳过）。
///
/// v28 t08 模板对齐（t12 生态核证）：
/// - 结构用 agents.md 官网推荐节（产物布局/常用命令/开发规范），纯
///   Markdown 不发明 schema、无必填字段（官网 FAQ 明示 "Are there
///   required fields? No. AGENTS.md is just standard Markdown"）；
/// - 指令可证伪：每节写明「做什么/何时做/何时不做」（KyenAI 2026-07
///   实测 53% 样本缺验证/完成标准）；
/// - 保持精简（<200 行；TomeVault 实测 AGENTS.md 中位数 29 行，>200 行
///   进入指令过载区，占 2.2%）；
/// - 单一基线不双发：只生成 AGENTS.md，不生成 CLAUDE.md（TomeVault 实测
///   双发仓 88.9% 两文件互不连接，"第二份文件几乎从未被读"）。
pub fn generate_agents_md(output_dir: &Path) -> Result<bool> {
    // AGENTS.md 写到产物目录的上级（项目根）：output_dir 通常是 .code-repo-wiki/ 或 wiki/，
    // 其上级即仓库根。不用 cwd——测试的 cwd 是项目根而 output_dir 是临时目录，
    // 用 cwd 会把 AGENTS.md 写进被测仓库，污染工作树。
    let Some(root) = output_dir.parent() else {
        return Ok(false);
    };
    let agents_path = root.join("AGENTS.md");
    if agents_path.exists() {
        // t04a（v21）：已存在时跳过注入是保护行为，但必须让用户/外部 Agent
        // 知道产物没被指引（静默跳过会让 AI 代理找不到 wiki 入口）——
        // 提示补救路径（v33：install 命令默认注入 wiki 引用块，可把
        // 当前工具的指引合并进既有文件）。
        tracing::warn!(
            "仓库已存在 AGENTS.md（{}），跳过注入以保护人工维护内容；如需 code-repo-wiki 指引可运行 `code-repo-wiki install`",
            agents_path.display()
        );
        return Ok(false);
    }
    let content = format!(
        r#"# AGENTS.md — AI 代理导航（由 code-repo-wiki 生成，可人工编辑）

本仓库使用 code-repo-wiki 维护可持续进化的项目 Wiki，产物位于 `{output_dir}/`。

## 产物布局

- `{output_dir}/llms.txt` — Agent 站点地图（llmstxt.org 规范，首选入口；头部含生成时间戳与 git 源码基线，可据此核对新鲜度）
- `{output_dir}/llms-full.txt` — 完整内容索引（32K token 预算内联实体清单，超预算模块页内注明省略）
- `{output_dir}/wiki/{{lang}}/` — 模块页（每模块一份，含职责/实体/依赖/使用示例）
- `{output_dir}/wiki/{{lang}}/api.md` — API 参考（按模块分组）
- `{output_dir}/wiki/{{lang}}/architecture.md` — 架构概览
- `{output_dir}/wiki/{{lang}}/overview.md` — 项目概览（自底向上合成）
- `{output_dir}/cards/{{lang}}/` — Knowledge Card（AI 代理的结构化摘要，JSON 元数据+Markdown）
- `{output_dir}/assets/diagrams/` — Mermaid 调用图/依赖图

## 常用命令

- 查找实体（函数/结构体/类）：`code-repo-wiki search -q "<关键词>"`（text/semantic/hybrid 三引擎，hybrid 含调用链补全）。何时做：需要某实体的签名/定位/说明时；何时不做：不知道关键词时先读 llms.txt 定位页面，不要盲目搜索。
- 更新产物：代码修改后运行 `code-repo-wiki update` 增量更新；`code-repo-wiki sync` 以 Git 内容合入；`code-repo-wiki lint` 检查产物健康（孤儿页/断链/过时）。何时做：每次代码变更后、以及发现产物与代码不一致时；何时不做：未改代码时不运行（no-op 无收益）。
- 知识沉淀：`code-repo-wiki note "<记录>"` 追加到 `{output_dir}/wiki/{{lang}}/_log.md`。何时做：需要给后续会话留下可检索的决策或教训时。

## 开发规范

- 开始任务时：先读 `{output_dir}/wiki/{{lang}}/overview.md` 与 `{output_dir}/wiki/{{lang}}/architecture.md` 建立全局认知，再按需深入模块页；上下文预算充足时用 `llms-full.txt` 一次获得完整实体骨架。
- 判断新鲜度：核对 `{output_dir}/llms.txt` 头部的生成时间戳与 git 基线——基线落后当前 HEAD 或时间戳距今超过 7 天时，先运行 `code-repo-wiki update` 再消费（过期产物会降低检索质量）。
- 人工修改保护：产物页面被人工编辑后不会被自动覆盖（保护机制），修改会反向同步到卡片（pending_manual_edits 节）。
- 何时不做：不直接编辑 `llms.txt` / `llms-full.txt`（确定性重生成会覆盖）；不在产物目录手工放置页面（`code-repo-wiki lint` 会判为孤儿页）。
"#,
        output_dir = output_dir.display(),
    );
    crate::fs::write_file_atomic(&agents_path, &content)?;
    tracing::info!("AGENTS.md 已生成: {}", agents_path.display());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DocumentKind, KnowledgeCard, WikiDocument};

    fn make_doc(language: &str) -> WikiDocument {
        WikiDocument {
            title: "TestModule".into(),
            kind: DocumentKind::WikiPage,
            content: "## 概述\n\n内容".into(),
            language: language.into(),
            module_path: vec!["src".into(), "testmodule".into()],
            references: vec![],
            parent: String::new(),
            last_updated: "2025-01-01T00:00:00Z".into(),
            based_on_commit: None,
            fingerprint: None,
        }
    }

    fn make_card() -> KnowledgeCard {
        KnowledgeCard {
            module_name: "src::testmodule".into(),
            module_type: "module".into(),
            summary: "摘要".into(),
            key_entities: vec![],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            design_rationale: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        }
    }

    #[test]
    fn test_primary_language_default_single() {
        let config = WikiConfig::default();
        assert_eq!(primary_language(&config), "zh");
    }
    #[test]
    fn test_primary_language_single_main() {
        let config = WikiConfig {
            wiki: crate::config::schema::WikiSection {
                language: "zh".into(),
            },
            ..Default::default()
        };
        // v30：expand_languages 已删除，恒只生成主语言
        assert_eq!(primary_language(&config), "zh");
    }

    /// A4：wiki 页与卡片的路径规则收敛后，路径计算必须与
    /// render_all/write_document 的落盘命名完全一致（单测锁死规则，防止漂移）
    #[test]
    fn test_wiki_and_card_path_rules() {
        let doc = make_doc("zh");
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &doc),
            Path::new("out")
                .join("wiki")
                .join("zh")
                .join("src_testmodule.md")
        );
        // ArchitectureOverview 特判写 architecture.md
        let arch = WikiDocument {
            kind: DocumentKind::ArchitectureOverview,
            ..make_doc("zh")
        };
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &arch),
            Path::new("out")
                .join("wiki")
                .join("zh")
                .join("architecture.md")
        );
        // ProjectOverview 特判写 overview.md
        let overview = WikiDocument {
            kind: DocumentKind::ProjectOverview,
            ..make_doc("zh")
        };
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &overview),
            Path::new("out").join("wiki").join("zh").join("overview.md")
        );
        // 卡片命名：module.replace("::","_")，与 card.rs 的 card_path 一致
        assert_eq!(
            card_page_path(Path::new("out"), "zh", "src::testmodule"),
            Path::new("out")
                .join("cards")
                .join("zh")
                .join("src_testmodule.md")
        );
    }

    /// A3：人工编辑过的卡片进入保护集后，全量 generate 不覆盖（保留人工编辑版）
    #[test]
    fn test_render_all_skips_protected_card() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_protected_card_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let config = WikiConfig {
            output_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };

        let card = make_card();
        let doc = make_doc("zh");
        let graph = KnowledgeGraph::default();

        // 预写"人工编辑版"卡片（与 render_all 落盘路径一致）
        let card_file = dir.join("cards").join("zh").join("src_testmodule.md");
        std::fs::create_dir_all(card_file.parent().unwrap()).unwrap();
        std::fs::write(&card_file, "人工编辑的内容").unwrap();

        // 保护集命中卡片路径 → 写盘跳过，人工编辑版保留
        let protected: std::collections::HashSet<String> =
            [card_file.to_string_lossy().to_string()]
                .into_iter()
                .collect();
        render_all(
            std::slice::from_ref(&doc),
            std::slice::from_ref(&card),
            &graph,
            &config,
            &protected,
        )
        .unwrap();
        let kept = std::fs::read_to_string(&card_file).unwrap();
        assert_eq!(
            kept, "人工编辑的内容",
            "被保护的卡片不应被全量 generate 覆盖"
        );

        // 无保护时卡片正常写盘（保护语义开关验证）
        let _ = std::fs::remove_file(&card_file);
        let empty = std::collections::HashSet::new();
        render_all(&[doc], &[card], &graph, &config, &empty).unwrap();
        assert!(card_file.exists(), "未保护的卡片应正常写盘");
        assert!(
            dir.join("wiki")
                .join("zh")
                .join("src_testmodule.md")
                .exists(),
            "wiki 页应正常写盘"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v22 修复：卡片写盘与页面生成解耦——页面全部失败（documents 为空）
    /// 时卡片仍全量落盘，杜绝「快照/_index 有、磁盘无」的不一致
    /// （Unity 实测：52 卡仅 42 页对应卡落盘的根因是卡片写盘绑定
    /// write_document，页面 LLM 失败即连带丢卡）。
    #[test]
    fn test_render_all_writes_cards_without_documents() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_cards_no_docs_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let config = WikiConfig {
            output_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };

        let card = make_card();
        let graph = KnowledgeGraph::default();
        let empty_docs: [WikiDocument; 0] = [];
        let empty_protected = std::collections::HashSet::new();

        render_all(
            &empty_docs,
            std::slice::from_ref(&card),
            &graph,
            &config,
            &empty_protected,
        )
        .unwrap();

        assert!(
            dir.join("cards")
                .join("zh")
                .join("src_testmodule.md")
                .exists(),
            "页面全部失败时卡片必须独立落盘"
        );
        assert!(
            !dir.join("wiki")
                .join("zh")
                .join("src_testmodule.md")
                .exists(),
            "无文档时不应产出页面"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 t08：AGENTS.md 模板对齐——精简（<200 行）、可证伪（每节含
    /// 「何时」= 做什么/何时做/何时不做）、保留 llms.txt/llms-full.txt
    /// 指引与搜索建议；只生成 AGENTS.md 不生成 CLAUDE.md（单一基线不双发）
    #[test]
    fn test_generate_agents_md_template_aligned() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_agents_md_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let output_dir = dir.join("out");
        std::fs::create_dir_all(&output_dir).unwrap();

        let generated = generate_agents_md(&output_dir).unwrap();
        assert!(generated, "首次生成应返回 true");

        let content = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(
            content.lines().count() < 200,
            "模板须精简（<200 行）: {} 行",
            content.lines().count()
        );
        // 官网推荐节（项目概述/常用命令/开发规范）
        assert!(content.contains("## 产物布局"), "应含产物布局节: {content}");
        assert!(content.contains("## 常用命令"), "应含常用命令节: {content}");
        assert!(content.contains("## 开发规范"), "应含开发规范节: {content}");
        // 保留既有功能：llms.txt / llms-full.txt 指引 + 搜索建议
        assert!(
            content.contains("llms.txt"),
            "应保留 llms.txt 指引: {content}"
        );
        assert!(
            content.contains("llms-full.txt"),
            "应保留 llms-full.txt 指引: {content}"
        );
        assert!(
            content.contains("code-repo-wiki search"),
            "应保留搜索建议: {content}"
        );
        // 可证伪措辞：每节明确「何时做/何时不做」
        assert!(
            content.contains("何时"),
            "指令须可证伪（含何时）: {content}"
        );
        // 单一基线不双发：不生成 CLAUDE.md
        assert!(
            !dir.join("CLAUDE.md").exists(),
            "只生成 AGENTS.md 不生成 CLAUDE.md"
        );

        // 幂等：已存在时跳过（返回 false，人工内容不被覆盖）
        let second = generate_agents_md(&output_dir).unwrap();
        assert!(!second, "已存在时应跳过注入");
        let kept = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert_eq!(kept, content, "已存在时内容不得被改动");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
