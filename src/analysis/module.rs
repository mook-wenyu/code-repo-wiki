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
        let (internal, external) = self.count_edges(node_ids);
        let total = internal + external;
        if total == 0.0 {
            return 0.0;
        }
        internal / total
    }

    /// 计算模块耦合度（0.0~1.0）
    fn calculate_coupling(&self, node_ids: &[NodeId]) -> f64 {
        let (internal, external) = self.count_edges(node_ids);
        let total = internal + external;
        if total == 0.0 {
            return 0.0;
        }
        external / total
    }

    /// 统计集合内部和跨集合边数
    ///
    /// 关键语义：聚类单位是 File 节点，但调用/导入/实现边都挂在
    /// **实体节点**上（File 节点只有 Contains 边）。因此先按
    /// File→Entity 的 Contains 边把集合扩展为"文件 + 其直接包含的实体"，
    /// 边统计才有意义；随后统计时**排除全部 Contains 边**（结构性边，
    /// 只表达归属，不是依赖关系），否则 Module→File/File→Entity 的
    /// Contains 会把 external 撑爆、cohesion 恒压到 0，导致任何目录都
    /// 无法通过阈值（此前模块检测在全量图上恒产出 0 个模块的根因）。
    fn count_edges(&self, node_ids: &[NodeId]) -> (f64, f64) {
        // 1. 集合扩展：File 节点 + 其直接 Contains 的实体节点
        let file_set: HashSet<NodeId> = node_ids.iter().copied().collect();
        let mut set: HashSet<NodeId> = file_set.clone();
        for edge in self.graph.graph.edge_references() {
            let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
            if kind == Some(EdgeKind::Contains) && file_set.contains(&edge.source()) {
                set.insert(edge.target());
            }
        }

        // 2. 边统计：排除 Contains（结构性），只数依赖类边
        let mut internal = 0.0;
        let mut external = 0.0;
        for edge in self.graph.graph.edge_references() {
            let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
            if kind == Some(EdgeKind::Contains) {
                continue;
            }
            let s = edge.source();
            let t = edge.target();
            let in_s = set.contains(&s);
            let in_t = set.contains(&t);
            if in_s && in_t {
                internal += 1.0;
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
        // 只传文件节点（集合会按 Contains 边自动扩展为 文件+实体）
        let ids = vec![NodeId::new(2), NodeId::new(3)];
        let c = detector.calculate_cohesion(&ids);
        // 扩展后 set={f1,f2,e1,e2};内部非 Contains: e1→e2 (Calls) = 1
        // Contains 结构性边全部排除;无外部边
        // 总非 Contains = 1, 内聚 = 1.0
        let expected = 1.0;
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
        let ids = vec![NodeId::new(2)]; // 只包含 a.rs（扩展后含 foo）
        let coupling = detector.calculate_coupling(&ids);
        // 扩展后 set={f1,e1};跨边界: e1→e2 的 Calls 和 TypeReference → e2 在集合外 => 2
        // 总非 Contains = 2; coupling = 2/2 = 1.0
        dbg!(coupling);
        assert!((coupling - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_detect() {
        let kg = make_small_graph();
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        // a.rs 与 b.rs 同前缀 "src"，内部互调（e1→e2）且无外部依赖：
        // cohesion=1.0>0.3, coupling=0<0.7 → 应检出 1 个模块
        assert_eq!(clusters.len(), 1, "应检出 src 模块，实际: {:?}", clusters.iter().map(|c| &c.name).collect::<Vec<_>>());
        assert_eq!(clusters[0].name, "src");
        assert_eq!(clusters[0].node_ids.len(), 2, "模块应包含 2 个文件节点");
    }
}
