use std::collections::HashSet;

use petgraph::algo::tarjan_scc;
use petgraph::visit::{EdgeRef, IntoEdgeReferences};

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

/// 从 KnowledgeGraph 生成模块依赖图（Mermaid flowchart）
///
/// 遍历图中的 Imports 和 DependsOn 边，生成模块级别的依赖关系图。
pub fn render_module_dependency_graph(graph: &KnowledgeGraph) -> String {
    let mut output = String::new();
    output.push_str("graph TD\n");

    let mut edges = Vec::new();
    let mut node_names: HashSet<String> = HashSet::new();

    for edge in graph.graph.edge_references() {
        let kind = &graph.graph[edge.id()].kind;
        if kind != &EdgeKind::DependsOn && kind != &EdgeKind::Imports && kind != &EdgeKind::Calls {
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

    for name in &node_names {
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
    for name in &node_names {
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

/// 为单个模块生成内部实体关系图
pub fn render_entity_graph(
    module: &crate::model::ModuleCluster,
    graph: &KnowledgeGraph,
) -> String {
    let mut output = String::new();
    output.push_str("graph LR\n");

    let node_set: HashSet<NodeId> = module.node_ids.iter().cloned().collect();

    for edge in graph.graph.edge_references() {
        if !node_set.contains(&edge.source()) || !node_set.contains(&edge.target()) {
            continue;
        }

        let source = &graph.graph[edge.source()];
        let target = &graph.graph[edge.target()];
        let kind = &graph.graph[edge.id()].kind;

        let s_id = sanitize_id(&source.name);
        let t_id = sanitize_id(&target.name);

        output.push_str(&format!("    {}[\"{}\"]\n", s_id, source.name));
        output.push_str(&format!("    {}[\"{}\"]\n", t_id, target.name));

        let arrow = match kind {
            EdgeKind::Contains => "-->",
            EdgeKind::Calls => "==>",
            EdgeKind::Imports => "-.->",
            EdgeKind::Implements => "-.->",
            EdgeKind::DependsOn => "-->",
            EdgeKind::Extends => "-->",
            EdgeKind::TypeReference => "-.->",
        };

        output.push_str(&format!("    {} {} {}\n", s_id, arrow, t_id));
    }

    output
}

/// 在 Mermaid 中标注循环依赖
pub fn render_cycle_diagram(cycles: &[Vec<NodeId>], graph: &KnowledgeGraph) -> String {
    let mut output = String::new();
    output.push_str("graph TD\n");
    output.push_str("    style cycle fill:#ffcccc,stroke:#ff0000\n\n");

    for (i, cycle) in cycles.iter().enumerate() {
        output.push_str(&format!("    subgraph Cycle{}\n", i + 1));

        for pair in cycle.windows(2) {
            let source = &graph.graph[pair[0]];
            let target = &graph.graph[pair[1]];
            let s_id = sanitize_id(&format!("{}_{}", source.name, i));
            let t_id = sanitize_id(&format!("{}_{}", target.name, i));

            output.push_str(&format!("        {}[\"{}\"]\n", s_id, source.name));
            output.push_str(&format!("        {}[\"{}\"]\n", t_id, target.name));
            output.push_str(&format!("        {}-->|cycle|{}\n", s_id, t_id));
        }

        if cycle.len() > 1 {
            let first = &graph.graph[cycle[0]];
            let last = &graph.graph[cycle[cycle.len() - 1]];
            let f_id = sanitize_id(&format!("{}_{}", first.name, 0));
            let l_id = sanitize_id(&format!("{}_{}", last.name, cycles.len() - 1));
            output.push_str(&format!("        {}-->|cycle|{}\n", l_id, f_id));
        }

        output.push_str("    end\n\n");
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
                kind: EdgeKind::DependsOn,
                source: a,
                target: b,
                weight: 1.0,
                location: None,
            },
        );

        KnowledgeGraph {
            graph: g,
            modules: vec![],
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

    #[test]
    fn test_render_cycle_diagram_empty() {
        let graph = make_test_graph();
        let output = render_cycle_diagram(&[], &graph);
        assert!(output.starts_with("graph TD"));
    }
}
