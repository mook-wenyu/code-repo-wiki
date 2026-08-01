use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use petgraph::visit::{EdgeRef, IntoNodeIdentifiers};

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

use super::change::{EntityChangeKind, EntityChangeSet};

/// 在知识图谱上传播变更影响，返回所有受影响的模块名称
///
/// 从变更文件节点出发，沿 Imports/Calls 边双向 BFS 遍历 3 层，
/// 找到所有直接或间接受影响的模块。
pub fn propagate_impact(changed_files: &[PathBuf], graph: &KnowledgeGraph, max_depth: usize) -> Vec<String> {
    if graph.graph.node_count() == 0 {
        return Vec::new();
    }

    let file_paths: Vec<String> = changed_files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let start_nodes = find_start_nodes(&file_paths, graph);
    if start_nodes.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<String> = propagate_from(start_nodes, graph, max_depth).into_iter().collect();
    result.sort();
    result
}

/// 将受影响模块名反查为文件路径集合（T2 传播闭环接线）
///
/// 影响传播（propagate_impact / propagate_impact_semantic）产出的是
/// `CodeNode.module_path.join("::")`（目录 + 文件 stem 派生，如
/// "src::net::tcp"），与 `ModuleCluster.name`（社区名，如 "src"）不是
/// 同一套命名——因此反查按 **File 节点的 module_path 精确匹配**，
/// 而非社区名匹配。生成过滤（run_generation_filtered）用返回值把
/// 受影响模块的文件并入变更集，实现"签名变更 → 依赖方模块文档
/// 重生成"的语义传播闭环。
pub fn module_files(affected_modules: &[String], graph: &KnowledgeGraph) -> Vec<PathBuf> {
    let target: HashSet<&str> = affected_modules.iter().map(|s| s.as_str()).collect();
    let mut files: Vec<PathBuf> = graph
        .graph
        .node_identifiers()
        .filter_map(|nid| {
            let node = &graph.graph[nid];
            if node.kind != crate::model::NodeKind::File {
                return None;
            }
            let mp = node.module_path.join("::");
            if target.contains(mp.as_str()) {
                node.file_path.as_ref().map(PathBuf::from)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// 语义影响传播：区分接口级与实现级变化（演进计划 T2.2）
///
/// 分类语义（change.rs）：
/// - 接口级变化（新增/删除/签名变更）：会影响调用方，向依赖方双向传播；
/// - 实现级变化（仅函数体修改）：只影响本模块产物。
///
/// 粒度说明：生成过滤是文件级（run_generation_filtered 按 changed_files），
/// 因此接口级判定也按文件——文件内任一实体是接口级变化，整个文件按
/// 接口级传播（保守，宁多勿漏）。
///
/// `entity_changes` 为空（FileWatch 策略无 git 信息、或分类失败）时
/// 回退保守的双向传播（现状行为），保证不丢影响。
pub fn propagate_impact_semantic(
    changed_files: &[PathBuf],
    entity_changes: &EntityChangeSet,
    graph: &KnowledgeGraph,
    max_depth: usize,
) -> Vec<String> {
    if graph.graph.node_count() == 0 {
        return Vec::new();
    }
    if entity_changes.changes.is_empty() {
        return propagate_impact(changed_files, graph, max_depth);
    }

    // 接口级变化的文件集合
    let interface_files: HashSet<String> = entity_changes
        .changes
        .iter()
        .filter(|c| matches!(c.kind, EntityChangeKind::Added | EntityChangeKind::Removed | EntityChangeKind::SignatureChanged))
        .map(|c| c.file.to_string_lossy().to_string())
        .collect();

    let mut affected: HashSet<String> = HashSet::new();
    for file in changed_files {
        let fp = file.to_string_lossy().to_string();
        let start_nodes = find_start_nodes(std::slice::from_ref(&fp), graph);
        if start_nodes.is_empty() {
            continue;
        }
        if interface_files.contains(&fp) {
            // 接口级：双向传播
            affected.extend(propagate_from(start_nodes, graph, max_depth));
        } else {
            // 实现级：仅起点所在模块
            for nid in start_nodes {
                let module = graph.graph[nid].module_path.join("::");
                if !module.is_empty() {
                    affected.insert(module);
                }
            }
        }
    }

    let mut result: Vec<String> = affected.into_iter().collect();
    result.sort();
    result
}

/// 按文件路径（子串匹配）找到图中的起点节点
fn find_start_nodes(file_paths: &[String], graph: &KnowledgeGraph) -> Vec<NodeId> {
    graph
        .graph
        .node_identifiers()
        .filter(|&nid| {
            let node = &graph.graph[nid];
            node.file_path
                .as_ref()
                .map(|fp| file_paths.iter().any(|cfp| fp.contains(cfp.as_str())))
                .unwrap_or(false)
        })
        .collect()
}

/// 从起点集合双向 BFS 传播影响，返回受影响模块名集合（起点自身计入）
fn propagate_from(start_nodes: Vec<NodeId>, graph: &KnowledgeGraph, max_depth: usize) -> HashSet<String> {
    let mut affected: HashSet<String> = HashSet::new();

    for &start in &start_nodes {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        queue.push_back((start, 0));
        visited.insert(start);

        // 起始节点本身也标记为受影响
        let start_node = &graph.graph[start];
        if !start_node.module_path.is_empty() {
            affected.insert(start_node.module_path.join("::"));
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
                if !matches!(kind, EdgeKind::Imports | EdgeKind::Calls) {
                    continue;
                }

                let neighbor_node = &graph.graph[neighbor];
                if !neighbor_node.module_path.is_empty() {
                    affected.insert(neighbor_node.module_path.join("::"));
                }

                visited.insert(neighbor);
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    affected
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
                kind: EdgeKind::Imports,
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
        features: Vec::new(),
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

    /// 语义传播：仅实现级变化（BodyChanged）→ 只影响本模块，不向依赖方传播
    #[test]
    fn test_semantic_body_change_only_local() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::BodyChanged,
                old_range: Some((1, 5)),
                new_range: Some((1, 8)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        // 仅 db 自身（net 导入 db、core 依赖 net 都不应受影响）
        assert_eq!(affected, vec!["db".to_string()]);
    }

    /// 语义传播：签名变化（接口级）→ 向依赖方传播
    #[test]
    fn test_semantic_signature_change_propagates() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::SignatureChanged,
                old_range: Some((1, 5)),
                new_range: Some((1, 5)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
    }

    /// 语义传播：删除（接口级）→ 向依赖方传播
    #[test]
    fn test_semantic_removed_propagates() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::Removed,
                old_range: Some((1, 5)),
                new_range: None,
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert!(affected.contains(&"net".to_string()), "删除应传播到导入方");
        assert!(affected.contains(&"core".to_string()));
    }

    /// 语义传播：无实体变化信息 → 回退保守双向传播（与 propagate_impact 一致）
    #[test]
    fn test_semantic_empty_changes_falls_back() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet::default();
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert_eq!(affected.len(), 3, "空实体变化应回退双向传播");
    }

    /// T2 传播闭环：受影响模块名（module_path.join("::") 形式）反查文件路径
    #[test]
    fn test_module_files_resolves_affected_modules() {
        // 构造 File 节点图（反查按 File 节点匹配；make_simple_graph 的节点是 Module 类型不适用）
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        for (i, (path, segs)) in [
            ("src/net.rs", vec!["net"]),
            ("src/db.rs", vec!["db"]),
            ("src/core.rs", vec!["core"]),
        ]
        .into_iter()
        .enumerate()
        {
            g.add_node(CodeNode {
                id: NodeId::new(i),
                kind: NodeKind::File,
                name: path.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                module_path: segs.into_iter().map(|s| s.to_string()).collect(),
            });
        }
        let graph = KnowledgeGraph { graph: g, modules: vec![], features: Vec::new() };

        let files = module_files(&["net".into(), "db".into()], &graph);
        assert_eq!(files.len(), 2, "应反查出 net.rs 与 db.rs");
        assert!(files.contains(&PathBuf::from("src/net.rs")));
        assert!(files.contains(&PathBuf::from("src/db.rs")));
        // 未知模块名 → 空
        assert!(module_files(&["not_exist".into()], &graph).is_empty());
        // 空输入 → 空
        assert!(module_files(&[], &graph).is_empty());
        // 去重：同一受影响名重复出现只反查一次
        let files2 = module_files(&["net".into(), "net".into()], &graph);
        assert_eq!(files2.len(), 1);
    }
}
