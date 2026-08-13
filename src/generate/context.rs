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
use crate::search::callgraph::CallIndex;

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
/// 依据 = 搜索层预计算调用索引 `CallIndex`（符号名 → (调用者, 被调用者)）。
/// 对每个本模块实体，取其调用者符号名，经「符号名 → 模块名」映射聚合到
/// 调用方模块；同一调用方模块下收集「本模块被其调用的符号」（调用方视角
/// 看到的入口点）。
///
/// 已知局限：CallIndex 以符号名为键，跨模块同名符号会合并（先到先得），
/// 属可接受的启发式——调用方模块名的颗粒度用于提示词，不参与产物断言。
pub fn build_caller_contexts(
    chunk: &Chunk,
    graph: &KnowledgeGraph,
    call_index: &CallIndex,
    summary_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<CallerContext> {
    // 符号名 → 模块名（先到先得，与 export_modules/index 同规则）
    let mut symbol_module: HashMap<&str, &str> = HashMap::new();
    for module in &graph.modules {
        for nid in &module.node_ids {
            if let Some(node) = graph.graph.node_weight(*nid) {
                symbol_module
                    .entry(node.name.as_str())
                    .or_insert(module.name.as_str());
            }
        }
    }

    let self_module = chunk.module_path.join("::");
    // 调用方模块 → 本模块被其调用的符号名集合（BTreeMap/BTreeSet 排序去重）
    let mut caller_symbols: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for entity in &chunk.entities {
        if let Some((callers, _)) = call_index.get(&entity.name) {
            for caller in callers {
                if let Some(&cm) = symbol_module.get(caller.as_str())
                    && cm != self_module.as_str()
                {
                    caller_symbols
                        .entry(cm)
                        .or_default()
                        .insert(entity.name.as_str());
                }
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

    fn chunk_of(module_path: &str, entities: Vec<Entity>, dependencies: Vec<String>) -> Chunk {
        Chunk {
            module_path: module_path.split("::").map(|s| s.to_string()).collect(),
            entities,
            imports: vec![],
            dependencies,
            caller_modules: vec![],
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
        let index = crate::search::callgraph::CallGraph::new(&graph).build_call_index();
        // src::b 的 chunk：实体 callee 被 src::a 的 caller 调用
        let chunk_b = chunk_of("src::b", vec![make_entity("callee")], vec![]);
        let ctxs = build_caller_contexts(&chunk_b, &graph, &index, &|_| None);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].module_name, "src::a");
        // 符号 = 本模块（src::b）被 src::a 调用的入口符号
        assert_eq!(ctxs[0].symbols, vec!["callee".to_string()]);
        assert!(ctxs[0].summary.is_none());

        // src::a 的 chunk：无人调用 → 空调用方列表
        let chunk_a = chunk_of("src::a", vec![make_entity("caller")], vec![]);
        assert!(build_caller_contexts(&chunk_a, &graph, &index, &|_| None).is_empty());
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
