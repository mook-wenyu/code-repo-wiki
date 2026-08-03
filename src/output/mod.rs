//! 渲染与导出层（单进程契约：export_snapshot.json 等状态文件无锁，
//! 同一输出目录并发运行不被支持，见 README 限制项）
pub mod crossref;
pub mod citation;
pub mod lint;
pub mod markdown;
pub mod mermaid;
pub mod mermaid_check;
pub mod html;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::schema::WikiConfig;
use crate::model::{KnowledgeCard, KnowledgeGraph, WikiDocument};

use self::markdown::write_document;

/// 生效的 wiki 语言列表（主语言 + 扩展语言）
///
/// 由 generate::collect_languages 移入（消除 output→generate 反向依赖；
/// generate 侧调用点改向本函数——generate→output 依赖本就存在，card.rs 已用）。
pub fn wiki_languages(config: &WikiConfig) -> Vec<String> {
    let mut languages = vec![config.wiki.language.clone()];
    languages.extend(config.wiki.expand_languages.iter().cloned());
    languages
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
        output_dir.join("wiki").join(lang).join(markdown::wiki_file_name(doc))
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
pub fn export_modules(graph: &KnowledgeGraph, cards: &[KnowledgeCard]) -> Vec<ExportModuleSnapshot> {
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
                .filter_map(|nid| graph.graph.node_weight(*nid).and_then(|n| n.file_path.clone()))
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
    let output_dir = Path::new(&config.output.dir);
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
pub fn render_all(
    documents: &[WikiDocument],
    cards: &[KnowledgeCard],
    graph: &KnowledgeGraph,
    config: &WikiConfig,
    protected: &std::collections::HashSet<String>,
) -> Result<()> {
    let output_dir = Path::new(&config.output.dir);
    let assets_dir = output_dir.join("assets");
    let languages = wiki_languages(config);

    // 按语言创建目录（扩展语言无文档时也保留空目录，保持目录结构稳定）
    for lang in &languages {
        std::fs::create_dir_all(output_dir.join("wiki").join(lang))?;
        std::fs::create_dir_all(output_dir.join("cards").join(lang))?;
    }
    std::fs::create_dir_all(&assets_dir)?;

    // 1. 写入 Wiki 页面（按文档自身语言分组写入对应目录）
    for doc in documents {
        // 路径计算与 write_document 落盘共用 wiki_page_path（人工修改保护判定依据）
        let wiki_path = wiki_page_path(output_dir, &doc.language, doc);
        // 精确关联：doc 的模块路径（join("::")）与卡片 module_name 精确相等
        // （两者同源于 chunk.module_path）。子串匹配曾误关联 src::test2 与
        // src::test（"src::test2".contains("test")）；无模块归属的全局文档
        // （module_path 为空）join 后为空串，不匹配任何卡片。
        let doc_module = doc.module_path.join("::");
        let doc_cards: Vec<&KnowledgeCard> = cards
            .iter()
            .filter(|c| c.module_name == doc_module)
            // 卡片与 wiki 页同规则保护：命中保护集的卡片跳过写盘，
            // 保留人工编辑版本（人工编辑过的卡片由指纹检测纳入保护集）
            .filter(|c| {
                !protected.contains(
                    &card_page_path(output_dir, &doc.language, &c.module_name)
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .collect();
        if protected.contains(&wiki_path.to_string_lossy().to_string()) {
            // 页面受人工修改保护：跳过页面写盘（保留人工版），但关联卡片
            // 仍写盘——人工修改记录（pending_manual_edits）随本次生成注入
            // 卡片，若一并跳过则反向同步永远无法落盘
            for card in &doc_cards {
                let card_path = card_page_path(output_dir, &doc.language, &card.module_name);
                crate::fs::write_file_atomic(&card_path, &markdown::render_knowledge_card(card))?;
            }
            continue;
        }
        write_document(doc, &doc_cards, output_dir, &doc.language)?;
    }

    // 1.5 写入 API 参考页（按模块分组的实体清单；内容与语言无关，只写主语言一份；
    // 命中保护集跳过写盘。指纹记录按同一规则：state.rs 对未落盘的 en/api.md 不记指纹）
    let primary_lang = &config.wiki.language;
    for lang in &languages {
        if lang != primary_lang {
            continue;
        }
        let api_path = api_doc_path(output_dir, lang);
        if protected.contains(&api_path.to_string_lossy().to_string()) {
            continue;
        }
        let api_doc = markdown::render_api_reference(graph);
        crate::fs::write_file_atomic(&api_path, &api_doc.content)?;
    }

    // 3. 写入 Knowledge Card 索引（JSON 格式，写入主语言目录）
    let primary_lang = &languages[0];
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
    let cards_index = output_dir.join("cards").join(primary_lang).join("_index.json");
    crate::fs::write_file_atomic(&cards_index, &serde_json::to_string_pretty(&cards_index_json)?)?;

    // 4. 生成目录页（命中保护集跳过写盘）
    let toc_path = toc_doc_path(output_dir);
    if !protected.contains(&toc_path.to_string_lossy().to_string()) {
        let toc = markdown::render_table_of_contents(documents);
        crate::fs::write_file_atomic(&toc_path, &toc)?;
    }

    // 5. 生成 Mermaid 依赖图
    let diagrams_dir = assets_dir.join("diagrams");
    std::fs::create_dir_all(&diagrams_dir)?;
    let mermaid_content = mermaid::render_module_dependency_graph(graph);
    crate::fs::write_file_atomic(&diagrams_dir.join("module-deps.mermaid"), &mermaid_content)?;

    // 5.1 模块级调用关系图（Calls 边按模块聚合）
    let call_graph_content = mermaid::render_module_call_graph(graph);
    crate::fs::write_file_atomic(&diagrams_dir.join("call-graph.mermaid"), &call_graph_content)?;

    // 5. 生成交叉引用索引
    let crossref = crossref::CrossRefIndex::build(documents);
    let broken = crossref.validate(documents);
    if !broken.is_empty() {
        tracing::warn!("发现 {} 个断链", broken.len());
        for link in &broken {
            tracing::warn!(
                "  断链: {} -> {} ({})",
                link.source_doc,
                link.broken_target,
                link.link_text
            );
        }
    }

    tracing::info!(
        "输出完成: {} 个页面, {} 个卡片, {} 个模块, 目录: {}",
        documents.len(),
        cards.len(),
        graph.modules.len(),
        config.output.dir
    );

    // AGENTS.md 引导文件：不存在才生成（幂等），人工已有 AGENTS.md 时跳过
    let _ = generate_agents_md(output_dir);

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
pub fn generate_agents_md(output_dir: &Path) -> Result<bool> {
    // AGENTS.md 写到产物目录的上级（项目根）：output_dir 通常是 .repo-wiki/ 或 wiki/，
    // 其上级即仓库根。不用 cwd——测试的 cwd 是项目根而 output_dir 是临时目录，
    // 用 cwd 会把 AGENTS.md 写进被测仓库，污染工作树。
    let Some(root) = output_dir.parent() else {
        return Ok(false);
    };
    let agents_path = root.join("AGENTS.md");
    if agents_path.exists() {
        return Ok(false);
    }
    let content = format!(
        r#"# AGENTS.md — AI 代理导航（由 repo-wiki 生成，可人工编辑）

本仓库使用 repo-wiki 维护可持续进化的项目 Wiki，产物位于 `{}/`。

## 产物布局

- `{}/wiki/{{lang}}/` — 模块页（每模块一份，含职责/实体/依赖/使用示例）
- `{}/wiki/{{lang}}/api.md` — API 参考（按模块分组）
- `{}/wiki/{{lang}}/architecture.md` — 架构概览
- `{}/wiki/{{lang}}/overview.md` — 项目概览（自底向上合成）
- `{}/cards/{{lang}}/` — Knowledge Card（AI 代理的结构化摘要，JSON 元数据+Markdown）
- `{}/assets/diagrams/` — Mermaid 调用图/依赖图

## AI 代理使用指引

1. 先读 `{}/wiki/{{lang}}/overview.md` 与 `architecture.md` 建立全局认知，
   再按需深入模块页。
2. 查找实体（函数/结构体/类）用 `repo-wiki search -q "<关键词>"`（支持
   text/semantic/hybrid 三引擎，hybrid 含调用链补全）。
3. 修改代码后运行 `repo-wiki update` 增量更新；`repo-wiki sync` 以 Git 内容
   合入；`repo-wiki lint` 检查产物健康（孤儿页/断链/过时）。
4. 人工修改产物页面后不会被自动覆盖（保护机制），修改会反向同步到卡片
   （pending_manual_edits 节）。
5. 知识沉淀：`repo-wiki note "<记录>"` 追加到 `{}/wiki/{{lang}}/_log.md`。
"#,
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display(),
        output_dir.display()
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
            last_updated: "2025-01-01T00:00:00Z".into(),
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
            pending_manual_edits: vec![],
            features: Vec::new(),
        }
    }

    #[test]
    fn test_collect_languages_default_single() {
        let config = WikiConfig::default();
        assert_eq!(wiki_languages(&config), vec!["zh"]);
    }

    #[test]
    fn test_collect_languages_with_expand() {
        let config = WikiConfig {
            wiki: crate::config::schema::WikiSection {
                language: "zh".into(),
                expand_languages: vec!["en".into(), "ja".into()],
            },
            ..Default::default()
        };
        assert_eq!(wiki_languages(&config), vec!["zh", "en", "ja"]);
    }

    /// A4：wiki 页与卡片的路径规则收敛后，路径计算必须与
    /// render_all/write_document 的落盘命名完全一致（单测锁死规则，防止漂移）
    #[test]
    fn test_wiki_and_card_path_rules() {
        let doc = make_doc("zh");
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &doc),
            Path::new("out").join("wiki").join("zh").join("src_testmodule.md")
        );
        // ArchitectureOverview 特判写 architecture.md
        let arch = WikiDocument {
            kind: DocumentKind::ArchitectureOverview,
            ..make_doc("zh")
        };
        assert_eq!(
            wiki_page_path(Path::new("out"), "zh", &arch),
            Path::new("out").join("wiki").join("zh").join("architecture.md")
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
            Path::new("out").join("cards").join("zh").join("src_testmodule.md")
        );
    }

    /// A3：人工编辑过的卡片进入保护集后，全量 generate 不覆盖（保留人工编辑版）
    #[test]
    fn test_render_all_skips_protected_card() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_protected_card_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = WikiConfig::default();
        config.output.dir = dir.to_string_lossy().into_owned();

        let card = make_card();
        let doc = make_doc("zh");
        let graph = KnowledgeGraph::default();

        // 预写"人工编辑版"卡片（与 render_all 落盘路径一致）
        let card_file = dir.join("cards").join("zh").join("src_testmodule.md");
        std::fs::create_dir_all(card_file.parent().unwrap()).unwrap();
        std::fs::write(&card_file, "人工编辑的内容").unwrap();

        // 保护集命中卡片路径 → 写盘跳过，人工编辑版保留
        let protected: std::collections::HashSet<String> =
            [card_file.to_string_lossy().to_string()].into_iter().collect();
        render_all(std::slice::from_ref(&doc), std::slice::from_ref(&card), &graph, &config, &protected).unwrap();
        let kept = std::fs::read_to_string(&card_file).unwrap();
        assert_eq!(kept, "人工编辑的内容", "被保护的卡片不应被全量 generate 覆盖");

        // 无保护时卡片正常写盘（保护语义开关验证）
        let _ = std::fs::remove_file(&card_file);
        let empty = std::collections::HashSet::new();
        render_all(&[doc], &[card], &graph, &config, &empty).unwrap();
        assert!(card_file.exists(), "未保护的卡片应正常写盘");
        assert!(
            dir.join("wiki").join("zh").join("src_testmodule.md").exists(),
            "wiki 页应正常写盘"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
