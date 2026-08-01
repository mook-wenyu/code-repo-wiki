use std::collections::HashSet;

use petgraph::visit::{EdgeRef, IntoEdgeReferences};


use crate::model::*;

use super::community::{community_name, detect_communities};

/// 模块边界检测器
pub struct ModuleDetector<'a> {
    graph: &'a KnowledgeGraph,
}

impl<'a> ModuleDetector<'a> {
    pub fn new(graph: &'a KnowledgeGraph) -> Self {
        Self { graph }
    }

    /// 执行模块检测，返回模块聚类列表
    ///
    /// 算法（演进计划 T1.2）：Leiden 社区检测（CPM 质量函数）在跨文件
    /// Imports/Calls/DependsOn 依赖图上划分 File 节点社区；每个社区命名
    /// 走 [`community_name`] 三档规则（公共目录前缀 → 文件数最多目录 →
    /// module_{n}），重名时追加文件 stem 消歧（模块名是 wiki 产物文件名
    /// 的唯一来源，重名会互相覆盖）。
    ///
    /// cohesion/coupling 仅作为**描述性元数据**写入 ModuleCluster（见
    /// count_edges 注释：历史上阈值拒绝导致"全有或全无"的脆弱分界）。
    pub fn detect(&self) -> Vec<ModuleCluster> {
        let communities = detect_communities(self.graph);
        let mut clusters: Vec<ModuleCluster> = Vec::with_capacity(communities.len());
        // 已用模块名集合：保证产物路径唯一
        let mut used_names: HashSet<String> = HashSet::new();

        for (idx, community) in communities.iter().enumerate() {
            // 命名输入 = 社区内文件路径（确定性：communities 已按最小路径排序）
            let file_paths: Vec<String> = community
                .iter()
                .filter_map(|nid| {
                    self.graph
                        .graph
                        .node_weight(*nid)
                        .and_then(|n| n.file_path.clone())
                })
                .collect();
            let mut name = community_name(&file_paths, idx);
            if used_names.contains(&name) {
                // 消歧：单文件社区与同目录社区重名时，追加文件 stem
                if let Some(stem) = file_stem(&file_paths) {
                    let alt = format!("{name}::{stem}");
                    if !used_names.contains(&alt) {
                        name = alt;
                    } else {
                        name = format!("module_{idx}");
                    }
                } else {
                    name = format!("module_{idx}");
                }
            }
            used_names.insert(name.clone());

            let cohesion = self.calculate_cohesion(community);
            let coupling = self.calculate_coupling(community);

            // 扩展：File 节点 + 其直接 Contains 的实体节点（与 count_edges 同一规则，
            // 但这是**持久化到 ModuleCluster.node_ids** 的集合——api.md 分组、
            // mermaid 模块图跨模块边聚合都遍历 node_ids，若只含 File 节点则
            // 实体清单为空、模块图全空）
            let file_set: HashSet<NodeId> = community.iter().copied().collect();
            let mut expanded: HashSet<NodeId> = file_set.clone();
            for edge in self.graph.graph.edge_references() {
                let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
                if kind == Some(EdgeKind::Contains) && file_set.contains(&edge.source()) {
                    expanded.insert(edge.target());
                }
            }
            // 去重排序，保持确定性输出
            let mut unique: Vec<NodeId> = expanded.into_iter().collect();
            unique.sort();
            clusters.push(ModuleCluster {
                name,
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
}

/// 取社区内第一个文件的 stem（重名消歧用，如 "src/net/tcp.rs" → "tcp"）
fn file_stem(files: &[String]) -> Option<String> {
    files
        .first()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().into_owned())
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
        // 添加外部边（TypeReference 已随未构建边类型删除，改用 Calls）
        kg.graph.add_edge(
            NodeId::new(4), NodeId::new(5),
            CodeEdge {
                id: EdgeId::new(kg.graph.edge_count()), kind: EdgeKind::Calls,
                source: NodeId::new(4), target: NodeId::new(5), weight: 0.5, location: None,
            },
        );
        let detector = ModuleDetector::new(&kg);
        let ids = vec![NodeId::new(2)]; // 只包含 a.rs（扩展后含 foo）
        let coupling = detector.calculate_coupling(&ids);
        // 扩展后 set={f1,e1};跨边界: e1→e2 的 Calls 两条（原有 + 新增）→ e2 在集合外 => 2
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
        // node_ids = 2 文件 + 2 实体（File→Entity Contains 扩展），
        // api.md/mermaid 依赖实体节点在集合内
        assert_eq!(clusters[0].node_ids.len(), 4, "模块应包含 2 文件 + 2 实体节点");
        // 且 4 个节点确实是 2 File + 2 Function
        let kinds: Vec<_> = clusters[0]
            .node_ids
            .iter()
            .map(|nid| kg.graph.node_weight(*nid).unwrap().kind.clone())
            .collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == NodeKind::File).count(),
            2,
            "应含 2 个文件节点: {:?}",
            kinds
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == NodeKind::Function).count(),
            2,
            "应含 2 个实体节点: {:?}",
            kinds
        );
    }

    /// 多目录社区检测：src/net（2 文件）有跨文件调用 → Leiden 聚为一个社区；
    /// src/http 两文件互不相连 → 各成单文件社区（同目录重名经文件 stem 消歧）
    #[test]
    fn test_detect_multiple_directories() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_file = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>, id: usize, path: &str, segs: Vec<&str>| -> (NodeId, NodeId) {
            let nid = g.add_node(CodeNode {
                id: NodeId::new(id), kind: NodeKind::File, name: path.into(),
                file_path: Some(path.into()), line_range: None, doc_comment: None,
                signature: None, module_path: segs.iter().map(|s| s.to_string()).collect(),
            });
            // File → Entity 的 Contains 边（实体计入模块集合的前提）
            let eid = g.add_node(CodeNode {
                id: NodeId::new(id + 100), kind: NodeKind::Function, name: format!("f{id}"),
                file_path: Some(path.into()), line_range: None, doc_comment: None,
                signature: None, module_path: segs.iter().map(|s| s.to_string()).collect(),
            });
            g.add_edge(nid, eid, CodeEdge {
                id: EdgeId::new(g.edge_count()), kind: EdgeKind::Contains,
                source: nid, target: eid, weight: 1.0, location: None,
            });
            (nid, eid)
        };
        let (tcp, etcp) = add_file(g, 0, "src/net/tcp.rs", vec!["src", "net"]);
        let (_udp, eudp) = add_file(g, 1, "src/net/udp.rs", vec!["src", "net"]);
        let _server = add_file(g, 2, "src/http/server.rs", vec!["src", "http"]);
        let _client = add_file(g, 3, "src/http/client.rs", vec!["src", "http"]);
        // tcp 实体 → udp 实体 跨文件调用：net 目录两文件聚为一社区
        g.add_edge(etcp, eudp, CodeEdge {
            id: EdgeId::new(g.edge_count()), kind: EdgeKind::Calls,
            source: etcp, target: eudp, weight: 0.7, location: None,
        });
        let _ = tcp;

        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        let names: Vec<&str> = clusters.iter().map(|c| c.name.as_str()).collect();
        // 社区检测语义：net 两文件一个社区；http 两文件各成社区（重名消歧）
        assert!(
            names.contains(&"src::net"),
            "应检出 src::net 社区，实际: {names:?}"
        );
        assert!(
            names.contains(&"src::http"),
            "应检出 src::http 单文件社区，实际: {names:?}"
        );
        // 名字必须唯一（模块名 → wiki 产物文件名，重名互相覆盖）
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "模块名必须唯一: {names:?}");
        // net 社区含 2 文件 + 2 实体（Contains 扩展）
        let net = clusters.iter().find(|c| c.name == "src::net").unwrap();
        assert_eq!(net.node_ids.len(), 4, "src::net 应含 2 文件 + 2 实体");
    }
}
