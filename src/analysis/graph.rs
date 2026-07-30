use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use petgraph::graph::EdgeIndex;
use tracing::warn;

use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
use crate::model::*;

/// 从 FileInsight 列表构建完整知识图谱
pub fn build(insights: &[FileInsight]) -> Result<KnowledgeGraph> {
    let mut kg = KnowledgeGraph::default();
    let g = &mut kg.graph;

    let project_id = g.add_node(CodeNode {
        id: NodeId::new(g.node_count()),
        kind: NodeKind::Project,
        name: "project".into(),
        file_path: None,
        line_range: None,
        doc_comment: None,
        signature: None,
        module_path: Vec::new(),
    });

    let mut module_cache: HashMap<Vec<String>, NodeId> = HashMap::new();

    for insight in insights {
        let path = Path::new(&insight.path);
        let dir_segments: Vec<String> = path
            .parent()
            .map(|p| {
                p.components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(s) => {
                            Some(s.to_string_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let file_module_id =
            ensure_module_chain(g, &mut module_cache, project_id, &dir_segments);

        let file_id = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::File,
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            file_path: Some(insight.path.to_string_lossy().into_owned()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: dir_segments.clone(),
        });

        g.add_edge(
            file_module_id,
            file_id,
            CodeEdge {
                id: EdgeIndex::new(g.edge_count()),
                kind: EdgeKind::Contains,
                source: file_module_id,
                target: file_id,
                weight: 1.0,
                location: None,
            },
        );

        let entity_ids: Vec<(Entity, NodeId)> = insight
            .entities
            .iter()
            .map(|e| {
                let kind = kind_from_str(&e.kind);
                let mut module_path = dir_segments.clone();
                if let Some(stem) = path.file_stem() {
                    module_path.push(stem.to_string_lossy().into_owned());
                }
                let id = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind,
                    name: e.name.clone(),
                    file_path: Some(insight.path.to_string_lossy().into_owned()),
                    line_range: Some((e.line_start, e.line_end)),
                    doc_comment: e.doc_comment.clone(),
                    signature: e.signature.clone(),
                    module_path,
                });
                (e.clone(), id)
            })
            .collect();

        for (_, eid) in &entity_ids {
            g.add_edge(
                file_id,
                *eid,
                CodeEdge {
                    id: EdgeIndex::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: file_id,
                    target: *eid,
                    weight: 1.0,
                    location: None,
                },
            );
        }

        build_import_edges(g, &insight.imports, &entity_ids);
        build_impl_edges(g, &entity_ids);
        build_call_edges(g, &entity_ids);
    }

    if let Some(node) = g.node_weight_mut(project_id) {
        node.id = project_id;
    }

    kg.graph = g.clone();

    let cycles = kg.detect_cycles();
    if !cycles.is_empty() {
        warn!("检测到 {} 个循环依赖: {:?}", cycles.len(), cycles);
    }

    Ok(kg)
}

fn ensure_module_chain(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    cache: &mut HashMap<Vec<String>, NodeId>,
    project_id: NodeId,
    segments: &[String],
) -> NodeId {
    let mut parent = project_id;
    for i in 0..segments.len() {
        let prefix: Vec<String> = segments[..=i].to_vec();
        if let Some(&cached) = cache.get(&prefix) {
            parent = cached;
            continue;
        }
        let id = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::Module,
            name: segments[i].clone(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: prefix.clone(),
        });
        g.add_edge(
            parent,
            id,
            CodeEdge {
                id: EdgeIndex::new(g.edge_count()),
                kind: EdgeKind::Contains,
                source: parent,
                target: id,
                weight: 1.0,
                location: None,
            },
        );
        cache.insert(prefix, id);
        parent = id;
    }
    parent
}

fn kind_from_str(s: &str) -> NodeKind {
    match s {
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "fn" | "function" => NodeKind::Function,
        "trait" => NodeKind::Trait,
        "impl" => NodeKind::Impl,
        "type" => NodeKind::Type,
        "const" | "constant" => NodeKind::Constant,
        "variable" | "let" => NodeKind::Variable,
        "interface" => NodeKind::Interface,
        "class" => NodeKind::Class,
        "macro" => NodeKind::Macro,
        _ => {
            warn!("未知实体类型 '{}'，使用 Function 作为默认", s);
            NodeKind::Function
        }
    }
}

/// 收集所有节点名/路径为 owned 数据
///
/// name_map 允许多个节点同名（重名函数/结构体等），值类型为 Vec<NodeId>。
fn collect_node_names(g: &petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>) -> (HashMap<String, Vec<NodeId>>, HashMap<Vec<String>, NodeId>) {
    let mut name_map: HashMap<String, Vec<NodeId>> = HashMap::new();
    let mut path_map: HashMap<Vec<String>, NodeId> = HashMap::new();
    for n in g.node_indices() {
        if let Some(w) = g.node_weight(n) {
            name_map.entry(w.name.clone()).or_default().push(n);
            path_map.insert(w.module_path.clone(), n);
                    }
        }
    (name_map, path_map)
}

fn build_import_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    imports: &[ImportStmt],
    entities: &[(Entity, NodeId)],
) {
    let (name_map, path_map) = collect_node_names(g);

    for imp in imports {
        let parts: Vec<&str> = imp.source.split("::").collect();
        if parts.is_empty() {
            continue;
        }

        let target_name = parts.last().unwrap_or(&"");
        let mut targets: Vec<NodeId> = Vec::new();

        // name_map 返回 Vec<NodeId>，遍历所有同名实体（函数重载、同名结构体等）
        if let Some(nids) = name_map.get(*target_name) {
            targets = nids.clone();
        }

        // name 匹配失败时尝试路径后缀匹配
        if targets.is_empty() {
            let path_segments: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
            for (mp, &nid) in &path_map {
                if mp.ends_with(&path_segments) {
                    targets.push(nid);
                    break;
                }
            }
        }

        for target_id in &targets {
            for (_, eid) in entities {
                if g.edges_connecting(*eid, *target_id).count() == 0 {
                    g.add_edge(
                        *eid,
                        *target_id,
                        CodeEdge {
                            id: EdgeIndex::new(g.edge_count()),
                            kind: EdgeKind::Imports,
                            source: *eid,
                            target: *target_id,
                            weight: 0.8,
                            location: Some((imp.line, imp.line)),
                        },
                    );
                }
            }
        }
    }
}

fn build_impl_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    entities: &[(Entity, NodeId)],
) {
    let (name_map, _) = collect_node_names(g);

    for (entity, eid) in entities {
        if let Some(trait_name) = parse_impl_target(&entity.kind, &entity.name)
            && let Some(trait_ids) = name_map.get(&trait_name)
        {
            for &trait_id in trait_ids {
                g.add_edge(
                    *eid,
                    trait_id,
                    CodeEdge {
                        id: EdgeIndex::new(g.edge_count()),
                        kind: EdgeKind::Implements,
                        source: *eid,
                        target: trait_id,
                        weight: 1.0,
                        location: None,
                    },
                );
            }
        }
    }
}

fn parse_impl_target(kind: &str, name: &str) -> Option<String> {
    if kind != "impl" && !kind.starts_with("impl_for") {
        return None;
    }
    // entity.name 的格式可能是 "impl MyTrait for MyStruct" 或 "MyTrait for MyStruct"
    // 提取 " for " 之前的部分作为 trait 名
    if let Some(for_idx) = name.find(" for ") {
        // 跳过 "impl " 前缀（如果存在）
        let after_impl = if let Some(idx) = name.find("impl ") {
            &name[idx + 5..]
        } else {
            name
        };
        let trait_name = after_impl[..for_idx].trim().to_string();
        if !trait_name.is_empty() {
            return Some(trait_name);
        }
    }
    // 如果名字中不含 " for "，无法确定 trait 名，返回 None
    // 不猜测（例如把 struct 名当作 trait 名）
    None
}

fn build_call_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    entities: &[(Entity, NodeId)],
) {
    let (name_map, _) = collect_node_names(g);

    for (entity, eid) in entities {
        if entity.kind != "fn" && entity.kind != "function" {
            continue;
        }
        let haystack = format!(
            "{} {}",
            entity.signature.as_deref().unwrap_or(""),
            entity.doc_comment.as_deref().unwrap_or("")
        );
        for (callee_name, callee_ids) in &name_map {
            if *callee_name == entity.name {
                continue;
            }
            let pattern = format!("{}(", callee_name);
            // 单词边界检查：确保 callee_name 前不是字母/数字/下划线
            let mut search_start = 0;
            while let Some(pos) = haystack[search_start..].find(&pattern) {
                let abs_pos = search_start + pos;
                let word_boundary = abs_pos == 0
                    || !haystack.as_bytes().get(abs_pos - 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
                if word_boundary {
                    for &callee_id in callee_ids {
                        if callee_id == *eid {
                            continue;
                        }
                        if g.edges_connecting(*eid, callee_id).count() == 0 {
                            g.add_edge(
                                *eid,
                                callee_id,
                                CodeEdge {
                                    id: EdgeIndex::new(g.edge_count()),
                                    kind: EdgeKind::Calls,
                                    source: *eid,
                                    target: callee_id,
                                    weight: 0.7,
                                    location: None,
                                },
                            );
                        }
                    }
                    break;
                }
                search_start = abs_pos + 1;
            }
        }
    }
}

impl KnowledgeGraph {
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        petgraph::algo::tarjan_scc(&self.graph)
            .into_iter()
            .filter(|scc| scc.len() > 1)
            .map(|scc| scc.iter().map(|&n| self.graph[n].name.clone()).collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    use crate::model::KnowledgeGraph;
    use std::path::PathBuf;

    #[test]
    fn test_empty_insights() {
        let kg = build(&[]).unwrap();
        assert_eq!(kg.graph.node_count(), 1);
        let root = kg.graph.node_weight(NodeId::new(0)).unwrap();
        assert_eq!(root.kind, NodeKind::Project);
    }

    #[test]
    fn test_single_file_two_entities() {
        let insights = vec![FileInsight {
            path: PathBuf::from("src/lib.rs"),
            language: "rust".into(),
            entities: vec![
                Entity {
                    name: "add".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 5,
                    doc_comment: None,
                    signature: Some("fn add(a: i32, b: i32) -> i32".into()),
                },
                Entity {
                    name: "Sub".into(),
                    kind: "struct".into(),
                    line_start: 7,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("struct Sub".into()),
                },
            ],
            imports: vec![],
            doc_comments: vec![],
        }];
        let kg = build(&insights).unwrap();
        assert_eq!(kg.graph.node_count(), 5);
        assert_eq!(kg.graph.edge_count(), 4);
    }

    #[test]
    fn test_import_edge() {
        let insights = vec![
            FileInsight {
                path: PathBuf::from("src/utils.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "helper".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn helper()".into()),
                }],
                imports: vec![],
                doc_comments: vec![],
            },
            FileInsight {
                path: PathBuf::from("src/main.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "run".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("fn run()".into()),
                }],
                imports: vec![ImportStmt {
                    source: "crate::utils::helper".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
            },
        ];
        let kg = build(&insights).unwrap();
        let has_import = kg
            .graph
            .edge_indices()
            .any(|e| kg.graph.edge_weight(e).map(|w| w.kind == EdgeKind::Imports).unwrap_or(false));
        assert!(has_import);
    }

    #[test]
    fn test_detect_cycles_empty() {
        let kg = KnowledgeGraph::default();
        assert!(kg.detect_cycles().is_empty());
    }

    #[test]
    fn test_detect_cycles_with_cycle() {
        let mut kg = KnowledgeGraph::default();
        let a = kg.graph.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "func_a".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec![],
        });
        let b = kg.graph.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "func_b".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec![],
        });
        kg.graph.add_edge(a, b, CodeEdge {
            id: EdgeIndex::new(0),
            kind: EdgeKind::Calls,
            source: a,
            target: b,
            weight: 1.0,
            location: None,
        });
        kg.graph.add_edge(b, a, CodeEdge {
            id: EdgeIndex::new(1),
            kind: EdgeKind::Calls,
            source: b,
            target: a,
            weight: 1.0,
            location: None,
        });
        let cycles = kg.detect_cycles();
        assert_eq!(cycles.len(), 1);
        assert!(cycles[0].contains(&"func_a".to_string()));
        assert!(cycles[0].contains(&"func_b".to_string()));
    }
}
