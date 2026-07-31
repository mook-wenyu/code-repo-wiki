use std::collections::HashMap;
use petgraph::visit::{EdgeRef, IntoEdgeReferences};
use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

/// 调用图查询 — 在 KnowledgeGraph 上提供调用者/被调用者等查询
pub struct CallGraph<'a> {
    graph: &'a KnowledgeGraph,
}

impl<'a> CallGraph<'a> {
    pub fn new(graph: &'a KnowledgeGraph) -> Self {
        Self { graph }
    }

    /// 返回指定符号调用的所有符号（被调用者）
    pub fn callee_of(&self, name: &str) -> Vec<NodeId> {
        let mut callees = Vec::new();
        for n in self.graph.graph.node_indices() {
            if let Some(w) = self.graph.graph.node_weight(n) && w.name == name {
                for e in self.graph.graph.edges(n) {
                    if e.weight().kind == EdgeKind::Calls {
                        callees.push(e.target());
                    }
                }
            }
        }
        callees
    }

    /// 返回调用指定符号的所有符号（调用者）
    pub fn caller_of(&self, name: &str) -> Vec<NodeId> {
        let mut callers = Vec::new();
        for n in self.graph.graph.node_indices() {
            if let Some(w) = self.graph.graph.node_weight(n) && w.name == name {
                for e in self.graph.graph.edges_directed(n, petgraph::Direction::Incoming) {
                    if e.weight().kind == EdgeKind::Calls {
                        callers.push(e.source());
                    }
                }
            }
        }
        callers
    }

    /// 返回所有 Calls 边的列表
    pub fn all_call_edges(&self) -> Vec<(NodeId, NodeId)> {
        let mut edges = Vec::new();
        for e in self.graph.graph.edge_references() {
            if e.weight().kind == EdgeKind::Calls {
                edges.push((e.source(), e.target()));
            }
        }
        edges
    }

    /// 构建符号名 → (调用者列表, 被调用者列表) 预计算表。
    /// 一次性遍历所有 Calls 边，避免查询时重复扫描全图。
    pub fn build_call_index(&self) -> HashMap<String, (Vec<String>, Vec<String>)> {
        let mut index: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
        for (src, dst) in self.all_call_edges() {
            if let (Some(s), Some(d)) = (self.graph.graph.node_weight(src), self.graph.graph.node_weight(dst)) {
                index.entry(d.name.clone()).or_default().0.push(s.name.clone());
                index.entry(s.name.clone()).or_default().1.push(d.name.clone());
            }
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeNode, CodeEdge, NodeKind};
    use petgraph::stable_graph::StableDiGraph;

    fn make_test_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let caller = g.add_node(CodeNode {
            id: NodeId::new(0), kind: NodeKind::Function,
            name: "caller".into(), file_path: None,
            line_range: None, doc_comment: None, signature: None,
            module_path: vec!["test".into()],
        });
        let callee = g.add_node(CodeNode {
            id: NodeId::new(1), kind: NodeKind::Function,
            name: "callee".into(), file_path: None,
            line_range: None, doc_comment: None, signature: None,
            module_path: vec!["test".into()],
        });
        g.add_edge(caller, callee, CodeEdge {
            id: petgraph::stable_graph::EdgeIndex::new(0),
            kind: EdgeKind::Calls, source: caller, target: callee,
            weight: 1.0, location: None,
        });
        KnowledgeGraph { graph: g, modules: vec![] }
    }

    #[test]
    fn test_callee_of() {
        let kg = make_test_graph();
        let cg = CallGraph::new(&kg);
        let callees = cg.callee_of("caller");
        assert_eq!(callees.len(), 1);
    }

    #[test]
    fn test_caller_of() {
        let kg = make_test_graph();
        let cg = CallGraph::new(&kg);
        let callers = cg.caller_of("callee");
        assert_eq!(callers.len(), 1);
    }
}
