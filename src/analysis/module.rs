use std::collections::{HashMap, HashSet};

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
    /// v49 聚合注释：历史上阈值拒绝导致"全有或全无"的脆弱分界）。
    pub fn detect(&self) -> Vec<ModuleCluster> {
        let communities = detect_communities(self.graph);
        let n_comm = communities.len();
        if n_comm == 0 {
            return Vec::new();
        }

        // ===== 内聚/耦合/扩展的 O(边 + 社区) 单遍聚合（v49）=====
        // 性能动机（实机证据）：旧实现对每个社区调用 count_edges 三次
        //（cohesion、coupling、expanded 各一次），每次遍历全图边两遍——
        // 即每社区 5 遍全图边扫描，复杂度 O(社区数 × 边数)。Unity 大仓
        //（3145 文件、数百目录、约 20 万条依赖边）实测卡在 analyzing 27%
        // 之后分钟级（数亿次迭代）。重构为：边归位一次遍历 + 每社区
        // 常数次组装，复杂度降为 O(边 + 社区)。
        // 等价性论证：count_edges 对社区 C 的统计 = 遍历所有非 Contains
        // 边，两端都在 C 的扩展集合（文件 + 其直接实体）→ internal，
        // 恰一端在 → external。该统计可按边单遍完成：每条非 Contains 边
        // (s, t) 归位到两端社区——同社区记 internal，跨社区给**两个**社区
        // 各记 external（旧实现逐社区遍历时，这条边对两端社区各贡献一次
        // external）。逐社区统计与单遍聚合在数值上逐边一致，无舍入差。

        // 1) File 节点 → 社区索引（归属键）
        let mut file_comm: HashMap<NodeId, usize> = HashMap::with_capacity(n_comm * 2);
        for (idx, community) in communities.iter().enumerate() {
            for &nid in community {
                file_comm.insert(nid, idx);
            }
        }

        // 2) 单遍 Contains 边：实体 → 所属 File（归位键）+ File → 直接实体
        //   （与旧 count_edges 的"扩展集合"规则一致：File 的直接 Contains 目标）
        let mut entity_file: HashMap<NodeId, NodeId> = HashMap::new();
        let mut file_entities: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in self.graph.graph.edge_references() {
            let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
            if kind != Some(EdgeKind::Contains) {
                continue;
            }
            if file_comm.contains_key(&edge.source()) {
                entity_file.insert(edge.target(), edge.source());
                file_entities.entry(edge.source()).or_default().push(edge.target());
            }
        }

        // 3) 端点归位：File → 社区；实体 → 所属 File → 社区。
        //    Module/Project 等容器节点无社区归属（旧实现同样不计数）。
        let node_comm = |nid: NodeId| -> Option<usize> {
            file_comm
                .get(&nid)
                .copied()
                .or_else(|| entity_file.get(&nid).and_then(|f| file_comm.get(f).copied()))
        };

        // 4) 单遍非 Contains 边：internal/external 按社区聚合
        let mut internal = vec![0.0f64; n_comm];
        let mut external = vec![0.0f64; n_comm];
        for edge in self.graph.graph.edge_references() {
            let kind = self.graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
            if kind == Some(EdgeKind::Contains) {
                continue;
            }
            let (Some(sc), Some(tc)) = (node_comm(edge.source()), node_comm(edge.target())) else {
                continue;
            };
            if sc == tc {
                internal[sc] += 1.0;
            } else {
                external[sc] += 1.0;
                external[tc] += 1.0;
            }
        }

        // 5) 组装：命名/消歧与旧实现完全一致
        let mut clusters: Vec<ModuleCluster> = Vec::with_capacity(n_comm);
        // 已用模块名集合：保证产物路径唯一
        let mut used_names: HashSet<String> = HashSet::new();

        for (idx, community) in communities.iter().enumerate() {
            // 命名输入 = 社区内文件路径（确定性：communities 已按大小降序 +
            // 最小路径排序，组内再排序——file_stem 取 first 的消歧后缀依赖组内顺序，N20）
            let mut file_paths: Vec<String> = community
                .iter()
                .filter_map(|nid| {
                    self.graph
                        .graph
                        .node_weight(*nid)
                        .and_then(|n| n.file_path.clone())
                })
                .collect();
            file_paths.sort();
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

            // 扩展：File 节点 + 其直接 Contains 的实体节点（与 count_edges
            // 同一规则，但这是**持久化到 ModuleCluster.node_ids** 的集合——
            // api.md 分组、mermaid 模块图跨模块边聚合都遍历 node_ids）
            let mut expanded: Vec<NodeId> = Vec::with_capacity(community.len() * 2);
            for &f in community {
                expanded.push(f);
                if let Some(entities) = file_entities.get(&f) {
                    expanded.extend(entities.iter().copied());
                }
            }
            // 去重排序，保持确定性输出
            expanded.sort_unstable();
            expanded.dedup();

            let total = internal[idx] + external[idx];
            let (cohesion, coupling) = if total == 0.0 {
                (0.0, 0.0)
            } else {
                (internal[idx] / total, external[idx] / total)
            };
            clusters.push(ModuleCluster {
                name,
                node_ids: expanded,
                cohesion,
                coupling,
                description: None,
            });
        }

        clusters
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
    

    /// v49 大图/分流测试合成图构造器：n_dirs 个目录 dirNN/，每目录
    /// files_per_dir 个文件（各带 1 个实体），connected_pairs 指定跨目录
    /// Calls 边（dir_a 的实体 0 → dir_b 的实体 0，与 community.rs 测试同构）
    fn make_dirs_graph(
        n_dirs: usize,
        files_per_dir: usize,
        connected_pairs: &[(usize, usize)],
    ) -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let mut first_entity: HashMap<String, NodeId> = HashMap::new();
        for d in 0..n_dirs {
            let dir = format!("dir{d:02}");
            for f in 0..files_per_dir {
                let path = format!("{dir}/f{f}.rs");
                let nid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::File,
                    name: path.clone(),
                    file_path: Some(path.clone()),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                    module_path: vec![dir.clone()],
                });
                let eid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::Function,
                    name: format!("f{f}"),
                    file_path: Some(path),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                    module_path: Vec::new(),
                });
                g.add_edge(
                    nid,
                    eid,
                    CodeEdge {
                        id: EdgeId::new(g.edge_count()),
                        kind: EdgeKind::Contains,
                        source: nid,
                        target: eid,
                        weight: 1.0,
                        location: None,
                    },
                );
                first_entity.entry(dir.clone()).or_insert(eid);
            }
        }
        for (a, b) in connected_pairs {
            let ea = first_entity[&format!("dir{a:02}")];
            let eb = first_entity[&format!("dir{b:02}")];
            g.add_edge(
                ea,
                eb,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Calls,
                    source: ea,
                    target: eb,
                    weight: 0.7,
                    location: None,
                },
            );
        }
        kg
    }

    fn make_small_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let p = g.add_node(CodeNode {
            id: NodeId::new(0), kind: NodeKind::Project, name: "p".into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec![], visibility: None,
        });
        let m = g.add_node(CodeNode {
            id: NodeId::new(1), kind: NodeKind::Module, name: "m".into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()], visibility: None,
        });
        let f1 = g.add_node(CodeNode {
            id: NodeId::new(2), kind: NodeKind::File, name: "a.rs".into(),
            file_path: Some("src/a.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()], visibility: None,
        });
        let f2 = g.add_node(CodeNode {
            id: NodeId::new(3), kind: NodeKind::File, name: "b.rs".into(),
            file_path: Some("src/b.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into()], visibility: None,
        });
        let e1 = g.add_node(CodeNode {
            id: NodeId::new(4), kind: NodeKind::Function, name: "foo".into(),
            file_path: Some("src/a.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into(), "a".into()], visibility: None,
        });
        let e2 = g.add_node(CodeNode {
            id: NodeId::new(5), kind: NodeKind::Function, name: "bar".into(),
            file_path: Some("src/b.rs".into()), line_range: None, doc_comment: None,
            signature: None, module_path: vec!["src".into(), "b".into()], visibility: None,
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

    /// v49：内聚统计（单遍聚合路径）——make_small_graph 中 a.rs/b.rs 互调
    /// （e1→e2 Calls 一条）且无外部边：src 模块 cohesion 应为 1.0
    #[test]
    fn test_cohesion() {
        let kg = make_small_graph();
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        // 单目录仓库（src/a.rs + src/b.rs）→ 目录分流 → 1 个社区，命名 src
        assert_eq!(clusters.len(), 1, "应产出 1 个模块, 实际: {:?}", clusters.iter().map(|c| &c.name).collect::<Vec<_>>());
        // 互调边两端都在扩展集合内（internal=1、external=0）→ cohesion 1.0
        assert!((clusters[0].cohesion - 1.0).abs() < 1e-6, "cohesion 应为 1.0, 实际 {}", clusters[0].cohesion);
        assert!((clusters[0].coupling).abs() < 1e-6, "coupling 应为 0.0, 实际 {}", clusters[0].coupling);
    }

    /// v49：耦合统计（单遍聚合路径）——24 目录合成图走目录分流（确定性），
    /// dir00 的实体跨目录调用 dir01 的实体：dir00 社区 coupling 应为 1.0
    #[test]
    fn test_coupling() {
        let kg = make_dirs_graph(24, 2, &[(0, 1)]);
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        // ≥24 目录 → 目录分流 → 恰 24 个社区，每目录一个
        assert_eq!(clusters.len(), 24, "24 目录应产出 24 个模块, 实际 {}", clusters.len());
        let dir00 = clusters
            .iter()
            .find(|c| c.name == "dir00")
            .expect("应存在 dir00 模块");
        // dir00 扩展集合 = {dir00/f0.rs, dir00/f1.rs, 2 实体}；跨边 f0→f0(dir01)
        // 恰一端在集合内 → external=1、internal=0 → coupling 1.0
        assert!((dir00.coupling - 1.0).abs() < 1e-6, "dir00 coupling 应为 1.0, 实际 {}", dir00.coupling);
        assert!((dir00.cohesion).abs() < 1e-6, "dir00 cohesion 应为 0.0, 实际 {}", dir00.cohesion);
    }

    /// v49：单遍聚合与暴力参照（测试内独立实现的 O(社区 × 边) 统计）
    /// 逐社区数值一致——防聚合语义回归（internal/external/扩展集合规则）。
    #[test]
    fn test_aggregate_matches_bruteforce() {
        // 30 目录 × 3 文件（≥24 → 目录分流确定性）；多条跨目录链连接
        let kg = make_dirs_graph(30, 3, &[(0, 1), (1, 2), (5, 6), (6, 7), (6, 8), (20, 21)]);
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        let communities = detect_communities(&kg);
        assert_eq!(clusters.len(), communities.len(), "社区数一致");

        // 暴力参照：对每个社区独立遍历全图边（旧 count_edges 语义）
        for (idx, comm) in communities.iter().enumerate() {
            let file_set: HashSet<NodeId> = comm.iter().copied().collect();
            let mut set: HashSet<NodeId> = file_set.clone();
            for edge in kg.graph.edge_references() {
                let kind = kg.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
                if kind == Some(EdgeKind::Contains) && file_set.contains(&edge.source()) {
                    set.insert(edge.target());
                }
            }
            let (mut b_internal, mut b_external) = (0.0f64, 0.0f64);
            for edge in kg.graph.edge_references() {
                let kind = kg.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
                if kind == Some(EdgeKind::Contains) {
                    continue;
                }
                let in_s = set.contains(&edge.source());
                let in_t = set.contains(&edge.target());
                if in_s && in_t {
                    b_internal += 1.0;
                } else if in_s || in_t {
                    b_external += 1.0;
                }
            }
            let b_total = b_internal + b_external;
            let (b_cohesion, b_coupling) = if b_total == 0.0 {
                (0.0, 0.0)
            } else {
                (b_internal / b_total, b_external / b_total)
            };
            let cluster = &clusters[idx];
            assert!(
                (cluster.cohesion - b_cohesion).abs() < 1e-9 && (cluster.coupling - b_coupling).abs() < 1e-9,
                "社区 {idx} ({}) 聚合统计与暴力参照不一致: 聚合 ({}, {}) vs 暴力 ({}, {})",
                cluster.name, cluster.cohesion, cluster.coupling, b_cohesion, b_coupling
            );
            // 扩展集合（node_ids）也与暴力参照一致（File + 直接实体，排序去重）
            let mut b_nodes: Vec<NodeId> = set.into_iter().collect();
            b_nodes.sort_unstable();
            assert_eq!(cluster.node_ids, b_nodes, "社区 {idx} 的 node_ids 与暴力参照不一致");
        }
    }

    /// v49：大图冒烟——300 目录 × 10 文件（≥24 → 目录分流）全链跨目录边，
    /// detect 必须在线性时间内完成且产出 300 个模块（防 O(社区 × 边) 回归）
    #[test]
    fn test_detect_large_repo_smoke() {
        let pairs: Vec<(usize, usize)> = (0..299).map(|a| (a, a + 1)).collect();
        let kg = make_dirs_graph(300, 10, &pairs);
        let detector = ModuleDetector::new(&kg);
        let clusters = detector.detect();
        assert_eq!(clusters.len(), 300, "300 目录应产出 300 个模块, 实际 {}", clusters.len());
        // 每模块含 10 文件 + 10 实体（扩展集合完整）
        for c in &clusters {
            assert_eq!(c.node_ids.len(), 20, "模块 {} 应含 10 文件 + 10 实体", c.name);
        }
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
                signature: None, module_path: segs.iter().map(|s| s.to_string()).collect(), visibility: None,
            });
            // File → Entity 的 Contains 边（实体计入模块集合的前提）
            let eid = g.add_node(CodeNode {
                id: NodeId::new(id + 100), kind: NodeKind::Function, name: format!("f{id}"),
                file_path: Some(path.into()), line_range: None, doc_comment: None,
                signature: None, module_path: segs.iter().map(|s| s.to_string()).collect(), visibility: None,
            });
            g.add_edge(nid, eid, CodeEdge {
                id: EdgeId::new(g.edge_count()), kind: EdgeKind::Contains,
                source: nid, target: eid, weight: 1.0, location: None,
            });
            (nid, eid)
        };
        let (_tcp, etcp) = add_file(g, 0, "src/net/tcp.rs", vec!["src", "net"]);
        let (_udp, eudp) = add_file(g, 1, "src/net/udp.rs", vec!["src", "net"]);
        let _server = add_file(g, 2, "src/http/server.rs", vec!["src", "http"]);
        let _client = add_file(g, 3, "src/http/client.rs", vec!["src", "http"]);
        // tcp 实体 → udp 实体 跨文件调用：net 目录两文件聚为一社区
        g.add_edge(etcp, eudp, CodeEdge {
            id: EdgeId::new(g.edge_count()), kind: EdgeKind::Calls,
            source: etcp, target: eudp, weight: 0.7, location: None,
        });

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
