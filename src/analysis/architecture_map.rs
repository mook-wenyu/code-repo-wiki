//! 预构建架构地图（v0.9 W2：确定性合成，常驻小地图 1-2k token）
//!
//! 面向 Agent 的常驻架构小地图：只放稳定模块边界与依赖，不写易变源码路径。
//! 两种数据来源，均**不新增 LLM 调用**：
//! - 模块职责一句话：复用现有 LLM 缓存 `.state/module_descriptions.json`
//!   （generate 的 describe_modules 落盘，≤30 字），缺失注明「无描述」；
//! - 模块级依赖：来自现有知识图谱（imports/calls 边聚合到模块级），
//!   纯静态数据、确定性合成（同输入必同输出，可测试）。
//!
//! 本模块同时是 MCP 工具 `wiki_get_dependencies` 的共享数据源（DRY：
//! 架构地图与 MCP 工具走同一 `module_dependencies` 聚合函数）。
//! 产物路径精确为 `{output_dir}/wiki/{主语言}/architecture-map.md`
//! （install 注入块与 AGENTS.md 已按此路径引用，路径写错 agent 会读空文件）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

/// 模块级依赖关系（架构地图与 MCP 工具共用的单一聚合结果）
///
/// - `dependencies`：本模块依赖的模块（依赖方，本模块 → 目标模块）
/// - `dependents`：依赖本模块的模块（被依赖方，源模块 → 本模块）
pub struct ModuleDeps {
    pub name: String,
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
}

/// 从知识图谱聚合模块级依赖（确定性合成核心，架构地图与 MCP 工具共用）
///
/// 聚合规则（与 output::mermaid / generate::index 同口径，保证全仓库
/// 模块依赖概念一致，不另立口径）：
/// 1. 节点 → 所属模块：先到先得（graph.modules 按深度 3→1 排列，子模块
///    先写入，父模块（src 兜底）不覆盖子模块实体）；
/// 2. 只取**跨模块**的 Imports + Calls 边（排除 Contains 包含关系与
///    Implements 实现关系——二者不构成依赖耦合）；
/// 3. 模块 A→B 当 A 内任一实体/文件 import/call B 内实体；同一对模块的
///    多条边合并为一条（模块级语义，去重）；
/// 4. 输出按模块名字典序、依赖列表有序（BTreeSet 迭代天然有序）——
///    确定性契约：同输入同输出。
///
/// 空图谱/无跨模块边 → 返回空 Vec（合法空结果，调用方按空节输出，不报错）。
pub fn module_dependencies(graph: &KnowledgeGraph) -> Vec<ModuleDeps> {
    use petgraph::visit::{EdgeRef, IntoEdgeReferences};

    // 节点 → 所属模块：先到先得（与 mermaid.rs / index.rs / export_modules
    // 同一规则，保证跨模块依赖聚合口径全仓库一致）
    let mut node_module: HashMap<NodeId, String> = HashMap::new();
    for module in &graph.modules {
        for nid in &module.node_ids {
            node_module
                .entry(*nid)
                .or_insert_with(|| module.name.clone());
        }
    }

    // 跨模块依赖边集合（源模块, 目标模块）；BTreeSet 天然去重 + 有序迭代
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for edge in graph.graph.edge_references() {
        if !matches!(
            graph.graph[edge.id()].kind,
            EdgeKind::Calls | EdgeKind::Imports
        ) {
            continue;
        }
        let (Some(src), Some(tgt)) = (
            node_module.get(&edge.source()),
            node_module.get(&edge.target()),
        ) else {
            continue;
        };
        if src != tgt {
            edges.insert((src.clone(), tgt.clone()));
        }
    }

    // 双向聚合：dependencies（源→目标）与 dependents（目标←源）
    let mut deps_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut deps_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (src, tgt) in &edges {
        deps_map.entry(src.clone()).or_default().insert(tgt.clone());
        deps_by.entry(tgt.clone()).or_default().insert(src.clone());
    }

    // 按模块组装（graph.modules 顺序本就确定性：社区按大小降序+路径排序，
    // 这里仍按名字排序保证跨版本稳定；BTreeSet 迭代有序，依赖列表天然排序）
    let mut deps: Vec<ModuleDeps> = graph
        .modules
        .iter()
        .map(|m| {
            let dependencies: Vec<String> = deps_map
                .get(&m.name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let dependents: Vec<String> = deps_by
                .get(&m.name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            ModuleDeps {
                name: m.name.clone(),
                dependencies,
                dependents,
            }
        })
        .collect();
    deps.sort_by(|a, b| a.name.cmp(&b.name));
    deps
}

/// 模块描述缓存条目（与 generate/wiki.rs 的 CacheEntry 同构；
/// 键 = `{模块名}@{语言}`，只复用 description 字段）
#[derive(serde::Deserialize)]
struct DescEntry {
    #[allow(dead_code)]
    fingerprint: String,
    description: String,
}

/// 读取 `.state/module_descriptions.json`，提取指定语言的「模块 → 一句话职责」
///
/// 键格式 `{模块名}@{语言}`（describe_modules 落盘约定）。缓存缺失/损坏/
/// 语言不匹配 → 返回空 Map（调用方按「无描述」降级渲染，不报错——
/// 描述是增强信息，非必需数据；「无描述」是合法空结果不是兜底）。
pub fn load_module_descriptions(output_dir: &Path, lang: &str) -> HashMap<String, String> {
    let path = output_dir.join(".state").join("module_descriptions.json");
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(entries) = serde_json::from_str::<HashMap<String, DescEntry>>(&content) else {
        return HashMap::new();
    };
    let suffix = format!("@{lang}");
    entries
        .into_iter()
        .filter_map(|(key, entry)| {
            key.strip_suffix(&suffix)
                .map(|module| (module.to_string(), entry.description))
        })
        .collect()
}

/// 渲染架构地图（确定性合成：同输入必同输出，全部遍历有序）
///
/// 结构（业界共识：常驻小地图 1-2k token，只放稳定模块边界与依赖）：
/// - `# 架构地图` 头部
/// - `## 模块总览`：`模块名 — 一句话职责`（来自 module_descriptions.json
///   缓存，缺失注明「无描述」）
/// - `## 模块依赖`：`模块名 → 依赖: X, Y；被依赖: Z, W`（来自知识图谱
///   imports/calls 边聚合；空列表输出「无」）
///
/// 空图谱（如纯文本仓库无实体 → graph.modules 为空）输出空架构地图并
/// 注明原因（合法空结果，非兜底），不报错。
pub fn render_architecture_map(
    graph: &KnowledgeGraph,
    descriptions: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    out.push_str("# 架构地图\n\n");
    out.push_str("> 预构建架构知识：模块边界与依赖（确定性合成，非 LLM 生成）。\n\n");

    if graph.modules.is_empty() {
        out.push_str("知识图谱为空（无模块聚类，可能为纯文本仓库或无实体），架构地图为空。\n");
        return out;
    }

    // 模块按名称字典序（确定性：不依赖 graph.modules 的内部顺序）
    let mut modules: Vec<&crate::model::ModuleCluster> = graph.modules.iter().collect();
    modules.sort_by(|a, b| a.name.cmp(&b.name));

    // 模块总览：模块名 — 一句话职责（复用既有 LLM 缓存，不新增调用）
    out.push_str("## 模块总览\n\n");
    for m in &modules {
        let desc = descriptions
            .get(&m.name)
            .map(String::as_str)
            .unwrap_or("无描述");
        out.push_str(&format!("- {} — {}\n", m.name, desc));
    }

    // 模块依赖：imports/calls 边聚合到模块级（与 MCP wiki_get_dependencies
    // 同一数据源；空列表输出「无」而非省略行——保持每模块一行结构完整）
    let deps = module_dependencies(graph);
    out.push('\n');
    out.push_str("## 模块依赖\n\n");
    for d in &deps {
        let deps_str = if d.dependencies.is_empty() {
            "无".to_string()
        } else {
            d.dependencies.join(", ")
        };
        let deps_by_str = if d.dependents.is_empty() {
            "无".to_string()
        } else {
            d.dependents.join(", ")
        };
        out.push_str(&format!(
            "- {} → 依赖: {}；被依赖: {}\n",
            d.name, deps_str, deps_by_str
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeEdge, CodeNode, ModuleCluster, NodeKind};
    use petgraph::stable_graph::StableDiGraph;

    /// 构造 3 模块 6 实体的合成图：alpha 调用 beta（跨模块 Calls）、
    /// gamma 导入 alpha（跨模块 Imports）、beta 调用自身 beta2（同模块）。
    /// 期望模块级依赖：alpha→[beta]；beta→[]（自调用被排除）；
    /// gamma→[alpha]；被依赖：alpha←[gamma]；beta←[alpha]。
    fn make_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let mut add_fn = |name: &str| {
            g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: name.into(),
                file_path: None,
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: vec![],
            })
        };
        let a1 = add_fn("a1");
        let b1 = add_fn("b1");
        let b2 = add_fn("b2");
        let c1 = add_fn("c1");
        let c2 = add_fn("c2");
        let mut add_edge = |src: _, tgt: _, kind: EdgeKind| {
            g.add_edge(
                src,
                tgt,
                CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(g.edge_count()),
                    kind,
                    source: src,
                    target: tgt,
                    weight: 1.0,
                    location: None,
                },
            )
        };
        add_edge(a1, b1, EdgeKind::Calls); // alpha → beta
        add_edge(b1, b2, EdgeKind::Calls); // 同模块自调用，应排除
        add_edge(c1, a1, EdgeKind::Imports); // gamma → alpha
        add_edge(a1, b1, EdgeKind::Imports); // 同对模块重复边，应去重

        KnowledgeGraph {
            graph: g,
            modules: vec![
                ModuleCluster {
                    name: "alpha".into(),
                    node_ids: vec![a1],
                    cohesion: 0.0,
                    coupling: 0.0,
                    description: None,
                },
                ModuleCluster {
                    name: "beta".into(),
                    node_ids: vec![b1, b2],
                    cohesion: 0.0,
                    coupling: 0.0,
                    description: None,
                },
                ModuleCluster {
                    name: "gamma".into(),
                    node_ids: vec![c1, c2],
                    cohesion: 0.0,
                    coupling: 0.0,
                    description: None,
                },
            ],
            features: Vec::new(),
        }
    }

    /// 模块级依赖聚合正确性：跨模块 Calls/Imports 聚合、同模块边排除、
    /// 同对模块多条边去重、双向 dependents 收集
    #[test]
    fn test_module_dependencies_aggregation() {
        let deps = module_dependencies(&make_graph());
        assert_eq!(deps.len(), 3, "应返回全部 3 个模块");

        let alpha = deps.iter().find(|d| d.name == "alpha").unwrap();
        assert_eq!(alpha.dependencies, vec!["beta"], "alpha 应依赖 beta");
        assert_eq!(alpha.dependents, vec!["gamma"], "alpha 应被 gamma 依赖");

        let beta = deps.iter().find(|d| d.name == "beta").unwrap();
        assert_eq!(
            beta.dependencies,
            Vec::<String>::new(),
            "beta 同模块自调用应排除"
        );
        assert_eq!(beta.dependents, vec!["alpha"], "beta 应被 alpha 依赖");

        let gamma = deps.iter().find(|d| d.name == "gamma").unwrap();
        assert_eq!(gamma.dependencies, vec!["alpha"], "gamma 应依赖 alpha");
        assert_eq!(gamma.dependents, Vec::<String>::new(), "gamma 无被依赖方");
    }

    /// 确定性：同输入两次聚合必须逐字节一致（遍历全有序，无 HashMap 迭代序）
    #[test]
    fn test_module_dependencies_deterministic() {
        let g = make_graph();
        let first = module_dependencies(&g);
        let second = module_dependencies(&g);
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.dependencies, b.dependencies);
            assert_eq!(a.dependents, b.dependents);
        }
    }

    /// 空图谱：返回空 Vec（合法空结果），渲染输出空架构地图并注明
    #[test]
    fn test_empty_graph() {
        let graph = KnowledgeGraph::default();
        assert!(module_dependencies(&graph).is_empty(), "空图谱无依赖");
        let out = render_architecture_map(&graph, &HashMap::new());
        assert!(out.contains("架构地图"), "应含标题");
        assert!(out.contains("为空"), "应注明空图谱原因: {out}");
    }

    /// 渲染确定性 + 结构：模块总览含职责/无描述标注，模块依赖含依赖/被依赖
    #[test]
    fn test_render_architecture_map_deterministic_and_complete() {
        let mut descriptions = HashMap::new();
        descriptions.insert("alpha".to_string(), "核心算法模块".to_string());
        // beta/gamma 无描述 → 应注明「无描述」
        let out = render_architecture_map(&make_graph(), &descriptions);

        let again = render_architecture_map(&make_graph(), &descriptions);
        assert_eq!(out, again, "同输入两次渲染必须字节一致");

        assert!(out.starts_with("# 架构地图"), "应含主标题");
        assert!(out.contains("## 模块总览"), "应含模块总览节");
        assert!(out.contains("## 模块依赖"), "应含模块依赖节");
        assert!(out.contains("- alpha — 核心算法模块"), "应含模块职责");
        assert!(out.contains("- beta — 无描述"), "无描述模块应注明");
        assert!(
            out.contains("- alpha → 依赖: beta；被依赖: gamma"),
            "依赖聚合行"
        );
        assert!(
            out.contains("- beta → 依赖: 无；被依赖: alpha"),
            "无依赖输出「无」"
        );
    }

    /// 模块描述缓存读取：键=`模块@语言`，过滤指定语言，损坏/缺失返回空
    #[test]
    fn test_load_module_descriptions() {
        let dir = std::env::temp_dir().join(format!("rw_arch_map_desc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".state")).unwrap();
        let path = dir.join(".state/module_descriptions.json");
        std::fs::write(
            &path,
            r#"{"src::net@zh":{"fingerprint":"f1","description":"网络层"},"src::http@en":{"fingerprint":"f2","description":"HTTP"}}"#,
        )
        .unwrap();

        let zh = load_module_descriptions(&dir, "zh");
        assert_eq!(zh.get("src::net").map(String::as_str), Some("网络层"));
        assert!(!zh.contains_key("src::http"), "en 条目不应混入 zh 结果");
        assert_eq!(zh.len(), 1, "应只取主语言条目");

        // 损坏缓存 → 空 Map（调用方降级「无描述」，不报错）
        std::fs::write(&path, "{not-json").unwrap();
        assert!(load_module_descriptions(&dir, "zh").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
