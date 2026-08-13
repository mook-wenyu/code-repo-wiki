//! 项目级上下文注入：依赖模块 / 调用方上下文的构建
//!
//! 生成层此前只把模块自身实体/导入喂给 LLM，模块间关系（谁依赖谁、
//! 谁调用谁）从未进入任何 prompt——模块页的「依赖关系」与卡片的
//! 设计意图全靠 LLM 猜。本模块把真实调用图（Calls 边）与依赖边
//! （chunk.dependencies）转成可注入的上下文，供 prompt 层渲染。
//!
//! 确定性契约：模块名/符号名一律 BTreeSet 排序去重（跨次稳定，
//! 与 chunk.rs 的 dependencies 排序契约一致）。模块归属唯一真源
//! 是 `ModuleCluster.node_ids → 节点 → 模块名`（build_node_to_module_map），
//! 不得用 `CodeNode.module_path`。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use petgraph::visit::EdgeRef;

use crate::generate::chunk::Chunk;
use crate::model::{EdgeKind, KnowledgeCard, KnowledgeGraph};

/// 依赖模块上下文：模块名 + 可选摘要（卡片阶段无卡片 → 摘要为 None）
#[derive(Debug, Clone)]
pub struct DependencyContext {
    pub module_name: String,
    pub summary: Option<String>,
}

/// 调用方模块上下文：模块名 + 被调用的符号 + 可选摘要
#[derive(Debug, Clone)]
pub struct CallerContext {
    pub module_name: String,
    pub symbols: Vec<String>,
    pub summary: Option<String>,
}

/// collect_caller_context 的调用方/被调用方各侧模块数上限
const CALLER_CALLEE_LIMIT: usize = 5;

/// 构建依赖模块上下文列表
///
/// 每个 chunk.dependencies 一个 DependencyContext：
/// - 摘要优先取依赖方卡片的 summary（wiki 阶段卡片已就绪，`cards` 表可查）；
/// - `summary_of` 为兜底闭包（如模块职责描述缓存），卡片阶段传空表 + 恒 None
///   闭包 → 摘要全部为 None，只给模块名（卡片并行阶段拿不到依赖方卡片）。
///
/// 顺序沿用 chunk.dependencies（BTreeSet 已排序，确定性契约不变）。
pub fn build_dependency_contexts(
    chunk: &Chunk,
    cards: &HashMap<String, &KnowledgeCard>,
    summary_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<DependencyContext> {
    chunk
        .dependencies
        .iter()
        .map(|dep| DependencyContext {
            module_name: dep.clone(),
            summary: cards
                .get(dep)
                .map(|c| c.summary.clone())
                .or_else(|| summary_of(dep)),
        })
        .collect()
}

/// 构建调用方模块上下文列表（wiki 页「## 调用方」节的数据源）
///
/// 依据 = 图上真实 Calls 入边（谁调用本模块）按源模块聚合，与
/// `collect_caller_context` 同款技术（真实边 + NodeId 精确模块归属）：
/// - 模块归属走 `ModuleCluster.node_ids → 节点 → 模块名`（build_node_to_module_map），
///   不依赖符号名；
/// - 同一调用方模块下收集「本模块被其调用的符号名」（调用方视角看到的入口点）。
///
/// 为何弃用 CallIndex：旧实现按符号名先到先得 + 纯名字键聚合调用方，跨模块
/// 同名符号（`new`/`parse`/`default`/`search` 高频重名）会把其他模块同名符号
/// 的调用方串进本模块——调用方节混入非真实调用方，属提示词输入噪声。按边
/// 扫描后模块归属精确，同名符号互不串扰。确定性契约：调用方模块 BTreeMap
/// 排序、符号 BTreeSet 去重（跨次稳定）。
pub fn build_caller_contexts(
    chunk: &Chunk,
    graph: &KnowledgeGraph,
    summary_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<CallerContext> {
    let self_module = chunk.module_path.join("::");
    let node_to_module = crate::generate::chunk::build_node_to_module_map(&graph.modules);
    // 本模块的节点集合（graph.modules 按模块名定位；Level 0 无模块时为空）
    let self_nodes: BTreeSet<_> = graph
        .modules
        .iter()
        .filter(|m| m.name == self_module)
        .flat_map(|m| m.node_ids.iter().copied())
        .collect();
    // 调用方模块 → 本模块被其调用的符号名集合（BTreeMap/BTreeSet 排序去重）
    let mut caller_symbols: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for nid in self_nodes {
        for e in graph
            .graph
            .edges_directed(nid, petgraph::Direction::Incoming)
        {
            if graph.graph[e.id()].kind != EdgeKind::Calls {
                continue;
            }
            let Some(&cm) = node_to_module.get(&e.source()) else {
                continue;
            };
            if cm == self_module.as_str() {
                continue;
            }
            // 被调符号名 = 本模块该节点的符号名（调用方视角的入口点）
            if let Some(node) = graph.graph.node_weight(nid) {
                caller_symbols
                    .entry(cm)
                    .or_default()
                    .insert(node.name.as_str());
            }
        }
    }

    caller_symbols
        .into_iter()
        .map(|(module, symbols)| CallerContext {
            module_name: module.to_string(),
            symbols: symbols.into_iter().map(|s| s.to_string()).collect(),
            summary: summary_of(module),
        })
        .collect()
}

/// 收集本模块的调用方与被调用方上下文（卡片设计意图的数据源）
///
/// 返回单个 CallerContext 承载「谁调用我 / 我调用谁」两侧信息：
/// - `symbols`：调用方模块名（≤CALLER_CALLEE_LIMIT）与被调用方模块名
///   （≤CALLER_CALLEE_LIMIT），分别前缀「调用方: / 被调用方:」以便
///   prompt 逐条渲染时区分两侧；
/// - `summary`：本模块职责一行描述（来自 summary_of，如模块描述缓存）。
///
/// 依据 = 图上的真实 Calls 边（入边=调用方、出边=被调用方），不从代码
/// 硬编（硬编=复述 how，WHY 必须由 LLM 基于真实调用关系推断）。
/// graph.modules 中找不到本模块（Level 0 文件级分块）时返回空上下文。
pub fn collect_caller_context(
    chunk: &Chunk,
    graph: &KnowledgeGraph,
    summary_of: &dyn Fn(&str) -> Option<String>,
) -> CallerContext {
    let self_module = chunk.module_path.join("::");
    let node_to_module = crate::generate::chunk::build_node_to_module_map(&graph.modules);

    // 只扫本模块的节点：入边 Calls → 调用方模块；出边 Calls → 被调用方模块
    let mut callers: BTreeSet<&str> = BTreeSet::new();
    let mut callees: BTreeSet<&str> = BTreeSet::new();
    let mut found = false;
    for module in &graph.modules {
        if module.name != self_module {
            continue;
        }
        found = true;
        for nid in &module.node_ids {
            for e in graph.graph.edges(*nid) {
                if graph.graph[e.id()].kind == EdgeKind::Calls
                    && let Some(&tm) = node_to_module.get(&e.target())
                    && tm != self_module.as_str()
                {
                    callees.insert(tm);
                }
            }
            for e in graph
                .graph
                .edges_directed(*nid, petgraph::Direction::Incoming)
            {
                if graph.graph[e.id()].kind == EdgeKind::Calls
                    && let Some(&sm) = node_to_module.get(&e.source())
                    && sm != self_module.as_str()
                {
                    callers.insert(sm);
                }
            }
        }
    }

    let mut symbols: Vec<String> = Vec::new();
    if found {
        // 两侧各限 CALLER_CALLEE_LIMIT（先排序再截断，跨次稳定）
        symbols.extend(
            callers
                .into_iter()
                .take(CALLER_CALLEE_LIMIT)
                .map(|m| format!("调用方: {m}")),
        );
        symbols.extend(
            callees
                .into_iter()
                .take(CALLER_CALLEE_LIMIT)
                .map(|m| format!("被调用方: {m}")),
        );
    }

    // 先取摘要再移动 self_module（String 非 Copy，移动后不可再借用）
    let summary = summary_of(&self_module);
    CallerContext {
        module_name: self_module,
        symbols,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::Entity;
    use crate::model::{CodeEdge, CodeNode, ModuleCluster, NodeKind};
    use petgraph::stable_graph::StableDiGraph;

    fn make_entity(name: &str) -> Entity {
        Entity {
            name: name.to_string(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 5,
            doc_comment: None,
            signature: None,
            visibility: None,
        }
    }

    /// 构造两模块图：src::a（fn caller）调用 src::b（fn callee）
    fn two_module_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let fa = g.add_node(CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: NodeKind::File,
            name: "a.rs".into(),
            file_path: Some("src/a/a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let caller = g.add_node(CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/a/a.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let fb = g.add_node(CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(2),
            kind: NodeKind::File,
            name: "b.rs".into(),
            file_path: Some("src/b/b.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });
        let callee = g.add_node(CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(3),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/b/b.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });
        let mut idx = 0usize;
        let mut add_edge = |s, t, kind, g: &mut StableDiGraph<CodeNode, CodeEdge>| {
            g.add_edge(
                s,
                t,
                CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(idx),
                    kind,
                    source: s,
                    target: t,
                    weight: 1.0,
                    location: None,
                },
            );
            idx += 1;
        };
        add_edge(fa, caller, EdgeKind::Contains, &mut g);
        add_edge(fb, callee, EdgeKind::Contains, &mut g);
        add_edge(caller, callee, EdgeKind::Calls, &mut g);

        KnowledgeGraph {
            graph: g,
            modules: vec![
                ModuleCluster {
                    name: "src::a".into(),
                    node_ids: vec![fa, caller],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
                ModuleCluster {
                    name: "src::b".into(),
                    node_ids: vec![fb, callee],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
            ],
            features: Vec::new(),
        }
    }

    /// 构造测试用 CodeNode（File 与 Function 节点通用，line_range 对测试逻辑无影响）
    fn graph_node(name: &str, kind: NodeKind, file: &str, module: &[&str]) -> CodeNode {
        CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind,
            name: name.into(),
            file_path: Some(file.into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: module.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 构造四模块图：src::a 与 src::b 各含同名函数 `new`，src::c 调 a 的 new、
    /// src::d 调 b 的 new——验证同名符号按 Calls 边 + NodeId 精确归属，
    /// 不按符号名合并（旧 CallIndex 实现会把 c 串进 b 的调用方）。
    fn same_name_new_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let file_a = g.add_node(graph_node(
            "a.rs",
            NodeKind::File,
            "src/a/a.rs",
            &["src", "a"],
        ));
        let a_new = g.add_node(graph_node(
            "new",
            NodeKind::Function,
            "src/a/a.rs",
            &["src", "a"],
        ));
        let file_c = g.add_node(graph_node(
            "c.rs",
            NodeKind::File,
            "src/c/c.rs",
            &["src", "c"],
        ));
        let c_caller = g.add_node(graph_node(
            "caller",
            NodeKind::Function,
            "src/c/c.rs",
            &["src", "c"],
        ));
        let file_b = g.add_node(graph_node(
            "b.rs",
            NodeKind::File,
            "src/b/b.rs",
            &["src", "b"],
        ));
        let b_new = g.add_node(graph_node(
            "new",
            NodeKind::Function,
            "src/b/b.rs",
            &["src", "b"],
        ));
        let file_d = g.add_node(graph_node(
            "d.rs",
            NodeKind::File,
            "src/d/d.rs",
            &["src", "d"],
        ));
        let d_caller = g.add_node(graph_node(
            "caller",
            NodeKind::Function,
            "src/d/d.rs",
            &["src", "d"],
        ));

        let mut idx = 0usize;
        let mut add_edge = |s, t, kind, g: &mut StableDiGraph<CodeNode, CodeEdge>| {
            g.add_edge(
                s,
                t,
                CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(idx),
                    kind,
                    source: s,
                    target: t,
                    weight: 1.0,
                    location: None,
                },
            );
            idx += 1;
        };
        for (s, t) in [
            (file_a, a_new),
            (file_c, c_caller),
            (file_b, b_new),
            (file_d, d_caller),
        ] {
            add_edge(s, t, EdgeKind::Contains, &mut g);
        }
        add_edge(c_caller, a_new, EdgeKind::Calls, &mut g);
        add_edge(d_caller, b_new, EdgeKind::Calls, &mut g);

        KnowledgeGraph {
            graph: g,
            modules: vec![
                ModuleCluster {
                    name: "src::a".into(),
                    node_ids: vec![file_a, a_new],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
                ModuleCluster {
                    name: "src::b".into(),
                    node_ids: vec![file_b, b_new],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
                ModuleCluster {
                    name: "src::c".into(),
                    node_ids: vec![file_c, c_caller],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
                ModuleCluster {
                    name: "src::d".into(),
                    node_ids: vec![file_d, d_caller],
                    cohesion: 0.9,
                    coupling: 0.1,
                    description: None,
                },
            ],
            features: Vec::new(),
        }
    }

    fn chunk_of(module_path: &str, entities: Vec<Entity>, dependencies: Vec<String>) -> Chunk {
        Chunk {
            module_path: module_path.split("::").map(|s| s.to_string()).collect(),
            entities,
            imports: vec![],
            dependencies,
            file_paths: vec![],
            entity_sources: vec![],
        }
    }

    #[test]
    fn test_build_dependency_contexts_uses_cards_then_fallback() {
        let chunk = chunk_of("src::a", vec![], vec!["src::b".into()]);
        let empty_cards: HashMap<String, &KnowledgeCard> = HashMap::new();
        // 卡片阶段：空表 + None 闭包 → 只给模块名，摘要 None
        let ctxs = build_dependency_contexts(&chunk, &empty_cards, &|_| None);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].module_name, "src::b");
        assert!(ctxs[0].summary.is_none());

        // wiki 阶段：cards 表命中 → 摘要来自卡片
        let card = KnowledgeCard {
            module_name: "src::b".into(),
            module_type: "module".into(),
            summary: "b 的卡片摘要".into(),
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
            features: vec![],
            design_rationale: None,
        };
        let mut cards: HashMap<String, &KnowledgeCard> = HashMap::new();
        cards.insert("src::b".to_string(), &card);
        let ctxs = build_dependency_contexts(&chunk, &cards, &|_| None);
        assert_eq!(ctxs[0].summary.as_deref(), Some("b 的卡片摘要"));

        // 卡片未命中时回退 summary_of 闭包
        let ctxs = build_dependency_contexts(&chunk, &empty_cards, &|m| Some(format!("{m} 职责")));
        assert_eq!(ctxs[0].summary.as_deref(), Some("src::b 职责"));
    }

    #[test]
    fn test_build_caller_contexts_groups_by_caller_module() {
        let graph = two_module_graph();
        // src::b 的 chunk：实体 callee 被 src::a 的 caller 调用（Calls 入边推导）
        let chunk_b = chunk_of("src::b", vec![make_entity("callee")], vec![]);
        let ctxs = build_caller_contexts(&chunk_b, &graph, &|_| None);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].module_name, "src::a");
        // 符号 = 本模块（src::b）被 src::a 调用的入口符号
        assert_eq!(ctxs[0].symbols, vec!["callee".to_string()]);
        assert!(ctxs[0].summary.is_none());

        // src::a 的 chunk：无人调用 → 空调用方列表
        let chunk_a = chunk_of("src::a", vec![make_entity("caller")], vec![]);
        assert!(build_caller_contexts(&chunk_a, &graph, &|_| None).is_empty());
    }

    /// 同名符号归属失真回归：src::a 与 src::b 各含同名函数 `new`，src::c 调
    /// a 的 new、src::d 调 b 的 new。旧实现按符号名（CallIndex 先到先得合并）
    /// 会把 c 串进 b 的调用方；按 Calls 边 + NodeId 精确归属后，各模块调用方
    /// 节只含真实调用方，不含同名非调用方。
    #[test]
    fn test_build_caller_contexts_same_name_symbols_not_mixed() {
        let graph = same_name_new_graph();
        // a 的 new 被 src::c 调用 → 调用方 = {src::c}
        let chunk_a = chunk_of("src::a", vec![make_entity("new")], vec![]);
        let ctxs_a = build_caller_contexts(&chunk_a, &graph, &|_| None);
        assert_eq!(ctxs_a.len(), 1);
        assert_eq!(ctxs_a[0].module_name, "src::c");
        assert_eq!(ctxs_a[0].symbols, vec!["new".to_string()]);

        // b 的 new 被 src::d 调用 → 调用方 = {src::d}，不得混入 src::c
        //（c 调的是 a 的同名 new，不是 b 的）
        let chunk_b = chunk_of("src::b", vec![make_entity("new")], vec![]);
        let ctxs_b = build_caller_contexts(&chunk_b, &graph, &|_| None);
        assert_eq!(ctxs_b.len(), 1);
        assert_eq!(ctxs_b[0].module_name, "src::d");
        assert_eq!(ctxs_b[0].symbols, vec!["new".to_string()]);
        assert!(
            !ctxs_b.iter().any(|c| c.module_name == "src::c"),
            "同名非调用方不得混入调用方节: {:?}",
            ctxs_b
        );
    }

    #[test]
    fn test_collect_caller_context_bundles_callers_and_callees() {
        let graph = two_module_graph();
        // src::a 调用 src::b：a 的上下文 = 调用方空、被调用方含 src::b
        let chunk_a = chunk_of("src::a", vec![make_entity("caller")], vec![]);
        let ctx = collect_caller_context(&chunk_a, &graph, &|m| Some(format!("{m} 职责")));
        assert_eq!(ctx.module_name, "src::a");
        assert_eq!(ctx.symbols, vec!["被调用方: src::b".to_string()]);
        assert_eq!(ctx.summary.as_deref(), Some("src::a 职责"));

        // src::b 被 src::a 调用：b 的上下文 = 调用方含 src::a
        let chunk_b = chunk_of("src::b", vec![make_entity("callee")], vec![]);
        let ctx = collect_caller_context(&chunk_b, &graph, &|_| None);
        assert_eq!(ctx.symbols, vec!["调用方: src::a".to_string()]);
    }

    #[test]
    fn test_collect_caller_context_empty_for_unknown_module() {
        // Level 0 文件级分块：graph.modules 无本模块 → 空上下文（不 panic）
        let graph = two_module_graph();
        let chunk = chunk_of("src::unknown", vec![], vec![]);
        let ctx = collect_caller_context(&chunk, &graph, &|_| None);
        assert!(ctx.symbols.is_empty());
        assert_eq!(ctx.module_name, "src::unknown");
    }
}
