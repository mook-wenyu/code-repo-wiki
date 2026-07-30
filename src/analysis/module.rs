use std::collections::{HashMap, HashSet};

use petgraph::visit::{EdgeRef, IntoEdgeReferences, IntoNodeReferences};


use crate::model::*;

/// 模块边界检测器
pub struct ModuleDetector<'a> {
    graph: &'a KnowledgeGraph,
}

impl<'a> ModuleDetector<'a> {
    pub fn new(graph: &'a KnowledgeGraph) -> Self {
        Self { graph }
    }

    /// 执行模块检测，返回模块聚类列表
    /// 算法：基于目录前缀的凝聚聚类 + Louvain 启发式
    pub fn detect(&self) -> Vec<ModuleCluster> {
        let mut clusters: Vec<ModuleCluster> = Vec::new();

        // 收集所有 File 及上级 Module 节点
        let file_nodes: Vec<NodeId> = self
            .graph
            .graph
            .node_references()
            .filter(|(_, n)| n.kind == NodeKind::File)
            .map(|(id, _)| id)
            .collect();

        if file_nodes.is_empty() {
            return clusters;
        }

        // 1. 按文件路径的目录前缀分组（深度 1~3）
        let mut depth_candidates: Vec<HashMap<Vec<String>, Vec<NodeId>>> = Vec::new();
        for depth in 1..=3 {
            let mut groups: HashMap<Vec<String>, Vec<NodeId>> = HashMap::new();
            for &nid in &file_nodes {
                if let Some(path) = self.graph.graph.node_weight(nid).and_then(|n| n.file_path.as_ref()) {
                    let prefix = path_prefix(path, depth);
                    groups.entry(prefix).or_default().push(nid);
                }
            }
            depth_candidates.push(groups);
        }

        // 2. 计算每个候选的内聚度和耦合度，保留合格的
        let mut scored: Vec<(Vec<String>, f64, f64, Vec<NodeId>)> = Vec::new();
        let mut assigned: HashSet<NodeId> = HashSet::new();

        // 深度从大到小（3→1）优先分配
        for groups in depth_candidates.iter().rev() {
            for (prefix, nodes) in groups {
                if nodes.len() < 2 {
                    continue; // 单文件不成模块
                }
                // 检查节点是否已被分配
                let unassigned: Vec<NodeId> =
                    nodes.iter().filter(|n| !assigned.contains(n)).copied().collect();
                if unassigned.is_empty() {
                    continue;
                }

                let all_ids: Vec<NodeId> = nodes.clone();
                let cohesion = self.calculate_cohesion(&all_ids);
                let coupling = self.calculate_coupling(&all_ids);

                if cohesion > 0.3 && coupling < 0.7 {
                    for &n in &unassigned {
                        assigned.insert(n);
                    }
                    scored.push((prefix.clone(), cohesion, coupling, all_ids));
                }
            }
        }

        // 3. 对未归属节点分配到最近的模块
        let unassigned: Vec<NodeId> = file_nodes.iter().filter(|n| !assigned.contains(n)).copied().collect();
        for nid in unassigned {
            if let Some(best) = self.find_nearest_module(nid, &scored) {
                for (_, _, _, nodes) in scored.iter_mut() {
                    if nodes.contains(&best) && !nodes.contains(&nid) {
                        nodes.push(nid);
                        break;
                    }
                }
            }
        }

        // 4. 构建输出
        for (prefix, cohesion, coupling, nodes) in scored {
            let name = prefix.join("::");
            // 去重
            let mut unique: Vec<NodeId> = nodes.clone();
            unique.sort();
            unique.dedup();
            clusters.push(ModuleCluster {
                name: name.clone(),
                node_ids: unique,
                cohesion,
                coupling,
                description: None,
            });
        }

        clusters
    }

    /// 计算模块内聚度（0.0~1.0）
    fn calculate_cohesion(&self, node_ids: &[NodeId]) -> f64 {
        let set: HashSet<NodeId> = node_ids.iter().copied().collect();
        let (internal, external) = self.count_edges(&set);
        let total = internal + external;
        if total == 0.0 {
            return 0.0;
        }
        internal / total
    }

    /// 计算模块耦合度（0.0~1.0）
    fn calculate_coupling(&self, node_ids: &[NodeId]) -> f64 {
        let set: HashSet<NodeId> = node_ids.iter().copied().collect();
        let (internal, external) = self.count_edges(&set);
        let total = internal + external;
        if total == 0.0 {
            return 0.0;
        }
        external / total
    }

    /// 统计集合内部和跨集合边数
    fn count_edges(&self, set: &HashSet<NodeId>) -> (f64, f64) {
        let mut internal = 0.0;
        let mut external = 0.0;
        for edge in self.graph.graph.edge_references() {
            let s = edge.source();
            let t = edge.target();
            let in_s = set.contains(&s);
            let in_t = set.contains(&t);
            if in_s && in_t {
                // 只算非 Contains 强边
                let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
                if kind != Some(EdgeKind::Contains) {
                    internal += 1.0;
                }
            } else if in_s || in_t {
                external += 1.0;
            }
        }
        (internal, external)
    }

    /// 找到最近的已分配模块
    fn find_nearest_module(
        &self,
        nid: NodeId,
        clusters: &[(Vec<String>, f64, f64, Vec<NodeId>)],
    ) -> Option<NodeId> {
        let node_path = self
            .graph
            .graph
            .node_weight(nid)
            .and_then(|n| n.file_path.as_ref())
            .map(|p| p.to_lowercase());

        let mut best: Option<(usize, &NodeId)> = None;
        for (_prefix, _, _, nodes) in clusters {
            for cn in nodes {
                let cp = self.graph.graph.node_weight(*cn).and_then(|n| n.file_path.as_ref()).map(|p| p.to_lowercase());
                if let (Some(np), Some(cp)) = (&node_path, cp) {
                    let common = common_prefix_length(np, &cp);
                    if common > best.map(|(l, _)| l).unwrap_or(0) {
                        best = Some((common, cn));
                    }
                }
            }
        }
        best.map(|(_, n)| *n)
    }
}

/// 取路径的前 depth 个目录段
fn path_prefix(path: &str, depth: usize) -> Vec<String> {
    use std::path::Component;
    let p = std::path::Path::new(path);
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .take(depth)
        .collect()
}

/// 计算两个路径字符串的公共目录段数
fn common_prefix_length(a: &str, b: &str) -> usize {
    use std::path::Component;
    let segs_a: Vec<_> = std::path::Path::new(a)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect();
    let segs_b: Vec<_> = std::path::Path::new(b)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect();
    segs_a
        .iter()
        .zip(segs_b.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    

    fn make_small_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let p = g.add_node(CodeNode {
            id: NodeId::new(0), kind: NodeKind::Project, name: "p".into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec![],
        });
        let m = g.add_node(CodeNode {
            id: NodeId::new(1), kind: NodeKind::Module, name: "m".into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()],
        });
        let f1 = g.add_node(CodeNode {
            id: NodeId::new(2), kind: NodeKind::File, name: "a.rs".into(),
            file_path: Some("src/a.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()],
        });
        let f2 = g.add_node(CodeNode {
            id: NodeId::new(3), kind: NodeKind::File, name: "b.rs".into(),
            file_path: Some("src/b.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()],
        });
        let e1 = g.add_node(CodeNode {
            id: NodeId::new(4), kind: NodeKind::Function, name: "foo".into(),
            file_path: Some("src/a.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into(), "a".into()],
        });
        let e2 = g.add_node(CodeNode {
            id: NodeId::new(5), kind: NodeKind::Function, name: "bar".into(),
            file_path: Some("src/b.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into(), "b".into()],
        });
        // 添加内部 Contains 边
        for (src, tgt) in &[(p, m), (m, f1), (m, f2), (f1, e1), (f2, e2)] {
            g.add_edge(*src, *tgt, CodeEdge {
                id: EdgeId::new(g.edge_count()), kind: EdgeKind::Contains,
                source: *src, target: *tgt, weight: 1.0, location: None,
            });
        }
        // 内部 Calls 边（e1 → e2）
        g.add_edge(e1, e2, CodeEdge {
            id: EdgeId::new(g.edge_count()), kind: EdgeKind::Calls,
            source: e1, target: e2, weight: 0.7, location: None,
        });
        kg
    }

    #[test]
    fn test_cohesion() {
        let kg = make_small_graph();
        let detector = ModuleDetector::new(&kg);
        // 包含 a.rs 和 b.rs 的集合
        let ids = vec![NodeId::new(2), NodeId::new(3), NodeId::new(4), NodeId::new(5)];
        let c = detector.calculate_cohesion(&ids);
        // 内部非 Contains: e1→e2 (Calls) = 1
        // 外部 Contains(仅一端在集合内): m→f1 + m→f2 = 2
        // 总非+外 = 1 + 2 = 3, 内聚 = 1/3
        let expected = 1.0 / 3.0;
        assert!((c - expected).abs() < 1e-6);
    }

    #[test]
    fn test_coupling() {
        let mut kg = make_small_graph();
        // 添加外部边
        kg.graph.add_edge(
            NodeId::new(4), NodeId::new(5),
            CodeEdge {
                id: EdgeId::new(kg.graph.edge_count()), kind: EdgeKind::TypeReference,
                source: NodeId::new(4), target: NodeId::new(5), weight: 0.5, location: None,
            },
        );
        let detector = ModuleDetector::new(&kg);
        let ids = vec![NodeId::new(2), NodeId::new(4)]; // 只包含 a.rs 和 foo
        let coupling = detector.calculate_coupling(&ids);
        // 内部非 Contains: 0; 跨边界: e1→e2 的 Calls 和 TypeReference → e2 在集合外 => 2
        // 总非 Contains = 2; coupling = 2/2 = 1.0
        dbg!(coupling);
        assert!((coupling - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_detect() {
        let kg = make_small_graph();
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        // 验证模块检测不 panic；检测结果是算法自身截断的结果
        assert!(clusters.len() <= 4);
    }
}
