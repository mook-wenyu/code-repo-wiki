use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use petgraph::visit::{EdgeRef, IntoNodeIdentifiers};

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

/// 在知识图谱上传播变更影响，返回所有受影响的模块名称
///
/// 从变更文件节点出发，沿 Imports/DependsOn 边双向 BFS 遍历 3 层，
/// 找到所有直接或间接受影响的模块。
pub fn propagate_impact(changed_files: &[PathBuf], graph: &KnowledgeGraph, max_depth: usize) -> Vec<String> {
    if graph.graph.node_count() == 0 {
        return Vec::new();
    }

    let file_paths: Vec<String> = changed_files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let start_nodes: Vec<NodeId> = graph
        .graph
        .node_identifiers()
        .filter(|&nid| {
            let node = &graph.graph[nid];
            node.file_path
                .as_ref()
                .map(|fp| file_paths.iter().any(|cfp| fp.contains(cfp.as_str())))
                .unwrap_or(false)
        })
        .collect();

    if start_nodes.is_empty() {
        return Vec::new();
    }

    let mut affected: HashSet<String> = HashSet::new();

    for &start in &start_nodes {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        queue.push_back((start, 0));
        visited.insert(start);

        // 起始节点本身也标记为受影响
        let start_node = &graph.graph[start];
        if !start_node.module_path.is_empty() {
            affected.insert(start_node.module_path[0].clone());
        }

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            // 双向遍历：出边（依赖别人）和入边（被别人依赖）
            // edges() 产出 current 作为 source 的边；edges_directed(_, Incoming)
            // 产出 current 作为 target 的边。合并后逐一检查。
            for edge in graph.graph.edges(current).chain(
                graph.graph.edges_directed(current, petgraph::Direction::Incoming),
            ) {
                // 确定"另一端的节点"
                let neighbor = if edge.source() == current {
                    edge.target()
                } else {
                    edge.source()
                };
                if visited.contains(&neighbor) {
                    continue;
                }

                let kind = &graph.graph[edge.id()].kind;
                if !matches!(kind, EdgeKind::Imports | EdgeKind::DependsOn | EdgeKind::Calls) {
                    continue;
                }

                let neighbor_node = &graph.graph[neighbor];
                if !neighbor_node.module_path.is_empty() {
                    affected.insert(neighbor_node.module_path[0].clone());
                }

                visited.insert(neighbor);
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    let mut result: Vec<String> = affected.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::stable_graph::StableDiGraph;
    use crate::model::{CodeEdge, CodeNode, NodeKind};

    fn make_simple_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();

        let core = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Module,
            name: "core".into(),
            file_path: Some("src/core.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["core".into()],
        });

        let net = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Module,
            name: "net".into(),
            file_path: Some("src/net.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["net".into()],
        });

        let db = g.add_node(CodeNode {
            id: NodeId::new(2),
            kind: NodeKind::Module,
            name: "db".into(),
            file_path: Some("src/db.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["db".into()],
        });

        g.add_edge(
            core, net,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(0),
                kind: EdgeKind::DependsOn,
                source: core,
                target: net,
                weight: 1.0,
                location: None,
            },
        );

        g.add_edge(
            net, db,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(1),
                kind: EdgeKind::Imports,
                source: net,
                target: db,
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
    fn test_propagate_impact() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
    }

    #[test]
    fn test_no_impact_for_unknown_file() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("unknown.rs")];
        let affected = propagate_impact(&changed, &graph, 3);
        assert!(affected.is_empty());
    }

    #[test]
    fn test_empty_graph() {
        let graph = KnowledgeGraph::default();
        let changed = vec![PathBuf::from("src/main.rs")];
        let affected = propagate_impact(&changed, &graph, 3);
        assert!(affected.is_empty());
    }

    #[test]
    fn test_impact_reverse_propagation() {
        // 测试反向传播：db.rs 变更 → net（导入 db）→ core（依赖 net）应全部受影响
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
        assert_eq!(affected.len(), 3);
    }

    #[test]
    fn test_impact_no_duplicate_modules() {
        // 验证同一模块不会重复出现在结果中
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        let unique: std::collections::HashSet<_> = affected.iter().cloned().collect();
        assert_eq!(affected.len(), unique.len());
    }
}
