use std::collections::HashSet;

use petgraph::algo::tarjan_scc;
use petgraph::visit::{EdgeRef, IntoEdgeReferences};

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

/// 从 KnowledgeGraph 生成模块依赖图（Mermaid flowchart）
///
/// 遍历图中的 Imports 和 Calls 边，生成模块级别的依赖关系图。
pub fn render_module_dependency_graph(graph: &KnowledgeGraph) -> String {
    let mut output = String::new();
    output.push_str("graph TD\n");

    let mut edges = Vec::new();
    let mut node_names: HashSet<String> = HashSet::new();

    for edge in graph.graph.edge_references() {
        let kind = &graph.graph[edge.id()].kind;
        if kind != &EdgeKind::Imports && kind != &EdgeKind::Calls {
            continue;
        }

        let source_node = &graph.graph[edge.source()];
        let target_node = &graph.graph[edge.target()];

        let source_module = module_name(source_node.module_path.as_slice());
        let target_module = module_name(target_node.module_path.as_slice());

        if source_module.is_empty() || target_module.is_empty() {
            continue;
        }

        if source_module == target_module {
            continue;
        }

        node_names.insert(source_module.clone());
        node_names.insert(target_module.clone());

        let edge_str = format!(
            "    {} --> {}\n",
            sanitize_id(&source_module),
            sanitize_id(&target_module)
        );
        if !edges.contains(&edge_str) {
            edges.push(edge_str);
        }
    }

    // 确定性输出：HashSet/HashMap 的迭代序随进程 RandomState 漂移，
    // 直接遍历会让同一张图两次渲染字节不同（确定性测试夹具无跨模块边
    // 时恒空无法暴露）；所有集合遍历前先排序，保证同输入同字节
    let mut sorted_names: Vec<&String> = node_names.iter().collect();
    sorted_names.sort();

    for name in &sorted_names {
        output.push_str(&format!(
            "    {}[\"{}\"]\n",
            sanitize_id(name),
            name
        ));
    }

    for edge in edges {
        output.push_str(&edge);
    }

    // 标注参与循环依赖的模块节点
    let cycle_module_names = collect_cycle_modules(graph);
    for name in sorted_names {
        if cycle_module_names.contains(name) {
            output.push_str(&format!(
                "    style {} fill:#ffcccc,stroke:#ff0000\n",
                sanitize_id(name)
            ));
        }
    }

    output
}

/// 收集参与循环依赖的模块名称集合
fn collect_cycle_modules(graph: &KnowledgeGraph) -> HashSet<String> {
    let sccs = tarjan_scc(&graph.graph);
    sccs.iter()
        .filter(|scc| scc.len() > 1)
        .flat_map(|scc| scc.iter().map(|&n| module_name(graph.graph[n].module_path.as_slice())))
        .filter(|m| !m.is_empty())
        .collect()
}

/// 渲染模块级调用关系图（节点=模块，边=跨模块 Calls 边聚合）
///
/// 遍历图中 Calls 边，源/目标实体映射到所属模块聚类，
/// 模块间调用聚合为一条带调用次数的边；
/// 未落入任何模块的实体与同模块调用不出现。
pub fn render_module_call_graph(graph: &KnowledgeGraph) -> String {
    // 实体节点 → 所属模块名
    let mut node_module: std::collections::HashMap<NodeId, String> = std::collections::HashMap::new();
    for module in &graph.modules {
        for nid in &module.node_ids {
            // 先到先得：graph.modules 按深度 3→1 排列（检测时深度从大到小推进），
            // 子模块先写入 → 实体归属最深的模块；父模块（src 兜底）后处理时
            // entry 已存在则跳过，避免父模块把子模块实体全部覆盖、
            // 导致跨模块调用全部被判定为"同模块调用"而被跳过（模块图恒空的根因）
            node_module.entry(*nid).or_insert_with(|| module.name.clone());
        }
    }

    // 跨模块 Calls 边按 (源模块, 目标模块) 聚合计数
    let mut edge_counts: std::collections::HashMap<(String, String), usize> = std::collections::HashMap::new();
    for edge in graph.graph.edge_references() {
        if graph.graph[edge.id()].kind != EdgeKind::Calls {
            continue;
        }
        let (Some(src), Some(tgt)) = (node_module.get(&edge.source()), node_module.get(&edge.target())) else {
            continue;
        };
        if src == tgt {
            continue;
        }
        *edge_counts.entry((src.clone(), tgt.clone())).or_insert(0) += 1;
    }

    let mut output = String::new();
    output.push_str("graph TD\n");
    let mut modules: HashSet<String> = HashSet::new();
    for (src, tgt) in edge_counts.keys() {
        modules.insert(src.clone());
        modules.insert(tgt.clone());
    }
    // 确定性输出：集合与计数表遍历前排序（同 render_module_dependency_graph）
    let mut sorted_modules: Vec<&String> = modules.iter().collect();
    sorted_modules.sort();
    let mut sorted_edges: Vec<(&(String, String), &usize)> = edge_counts.iter().collect();
    sorted_edges.sort_by(|a, b| a.0.cmp(b.0));

    for name in &sorted_modules {
        output.push_str(&format!("    {}[\"{}\"]\n", sanitize_id(name), name));
    }
    for ((src, tgt), count) in &sorted_edges {
        output.push_str(&format!("    {} -->|{}| {}\n", sanitize_id(src), count, sanitize_id(tgt)));
    }
    output
}

fn module_name(module_path: &[String]) -> String {
    // 返回完整模块路径，用 :: 连接
    // 修复：之前只返回第一段，导致深层模块被错误合并
    if module_path.is_empty() { return String::new(); }
    module_path.join("::")
}

fn sanitize_id(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::stable_graph::StableDiGraph;
    use crate::model::{CodeEdge, CodeNode, NodeKind};

    fn make_test_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();

        let a = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Module,
            name: "core".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["core".into()],
        });

        let b = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Module,
            name: "net".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["net".into()],
        });

        g.add_edge(
            a,
            b,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(0),
                kind: EdgeKind::Imports,
                source: a,
                target: b,
                weight: 1.0,
                location: None,
            },
        );

        KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        }
    }

    #[test]
    fn test_render_module_dependency_graph() {
        let graph = make_test_graph();
        let output = render_module_dependency_graph(&graph);

        assert!(output.starts_with("graph TD"));
        assert!(output.contains("core"));
        assert!(output.contains("net"));
        assert!(output.contains("-->"));
    }

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("hello-world"), "hello_world");
        assert_eq!(sanitize_id("foo::bar"), "foo__bar");
        assert_eq!(sanitize_id("valid"), "valid");
    }

    /// 构造 2 模块 3 实体 2 条跨模块调用边 + 1 条同模块调用边的小图
    fn make_call_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let mut add_fn = |name: &str, module: &str| {
            g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: name.into(),
                file_path: None,
                line_range: None,
                doc_comment: None,
                signature: None,
                module_path: vec![module.into()],
            })
        };
        let a1 = add_fn("a1", "alpha");
        let b1 = add_fn("b1", "beta");
        let b2 = add_fn("b2", "beta");
        let c1 = add_fn("c1", "gamma");
        let c2 = add_fn("c2", "gamma");
        let mut add_call = |s: _, t: _| {
            g.add_edge(
                s,
                t,
                CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(g.edge_count()),
                    kind: EdgeKind::Calls,
                    source: s,
                    target: t,
                    weight: 1.0,
                    location: None,
                },
            )
        };
        add_call(a1, b1);
        add_call(a1, b2);
        add_call(c1, c2);

        KnowledgeGraph {
            graph: g,
            modules: vec![
                crate::model::ModuleCluster {
                    name: "alpha".into(),
                    node_ids: vec![a1],
                    cohesion: 0.0,
                    coupling: 0.0,
                    description: None,
                },
                crate::model::ModuleCluster {
                    name: "beta".into(),
                    node_ids: vec![b1, b2],
                    cohesion: 0.0,
                    coupling: 0.0,
                    description: None,
                },
                crate::model::ModuleCluster {
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

    #[test]
    fn test_render_module_call_graph_aggregates_cross_module_calls() {
        let graph = make_call_graph();
        let output = render_module_call_graph(&graph);

        assert!(output.starts_with("graph TD"));
        // 两个参与跨模块调用的模块节点
        assert!(output.contains("alpha[\"alpha\"]"));
        assert!(output.contains("beta[\"beta\"]"));
        // 两条跨模块调用聚合为一条带计数的边
        assert!(output.contains("-->|2|"));
        // 同模块调用不出现（gamma 模块不应出现在图中）
        assert!(!output.contains("gamma"));
        assert!(!output.contains("|1|"));
    }

    #[test]
    fn test_render_module_call_graph_empty() {
        let graph = KnowledgeGraph::default();
        let output = render_module_call_graph(&graph);
        assert!(output.starts_with("graph TD"));
        assert_eq!(output, "graph TD\n");
    }

    /// A3：同一图两次渲染必须字节一致（HashSet/HashMap 迭代序随机，
    /// 不排序会漂移——确定性契约的回归测试，带跨模块边暴露真实场景）
    #[test]
    fn test_render_deterministic_bytes() {
        let dep1 = render_module_dependency_graph(&make_test_graph());
        let dep2 = render_module_dependency_graph(&make_test_graph());
        assert_eq!(dep1, dep2, "依赖图两次渲染必须字节一致");

        let call1 = render_module_call_graph(&make_call_graph());
        let call2 = render_module_call_graph(&make_call_graph());
        assert_eq!(call1, call2, "调用图两次渲染必须字节一致");

        // 依赖图输出顺序：节点按字典序（core 在 net 前）
        let core_pos = call1.find("alpha[\"alpha\"]").unwrap();
        let beta_pos = call1.find("beta[\"beta\"]").unwrap();
        assert!(core_pos < beta_pos, "节点应按字典序输出: {}", call1);
    }
}
