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
        signature: None, visibility: None,
        module_path: Vec::new(),
    });

    let mut module_cache: HashMap<Vec<String>, NodeId> = HashMap::new();
    // 跨文件调用边候选：(实体, 节点, 函数体文本)，全图实体构建完成后统一匹配
    let mut call_candidates: Vec<(Entity, NodeId, String)> = Vec::new();

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
            signature: None, visibility: None,
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
                    visibility: e.visibility.clone(),
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
        // 收集 (实体, 节点, 函数体文本) —— 跨文件调用边需在全图实体
        // 构建完成后统一匹配（每个函数用其函数体文本找被调用函数名）
        call_candidates.extend(entity_ids.iter().filter_map(|(e, eid)| {
            if e.kind != "fn" && e.kind != "function" {
                return None;
            }
            Some((e.clone(), *eid, extract_body(&insight.source, e.line_start, e.line_end)))
        }));
    }

    // 全部实体构建完成后统一构建调用边：此时 name_map 覆盖全图符号，
    // 跨文件调用（本文件函数调用其他文件函数）才能被解析
    build_call_edges(g, &call_candidates);

    if let Some(node) = g.node_weight_mut(project_id) {
        node.id = project_id;
    }

    kg.graph = g.clone();

    let cycles = kg.detect_cycles();
    if !cycles.is_empty() {
        warn!("检测到 {} 个循环依赖: {:?}", cycles.len(), cycles);
    }

    // 模块检测接线:图构建完成后运行社区检测,结果写回 kg.modules。
    // 生成层(generate/mod.rs)、渲染层(markdown/html/mermaid)均以
    // graph.modules 为模块分组的唯一来源;此前仅 lib.rs 显式调用
    // detect_modules 且结果只进 stats,modules 恒空导致按模块生成
    // 从未生效。检测失败向上传播(无兜底)。
    kg.modules = crate::analysis::detect_modules(&kg)?;

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
            signature: None, visibility: None,
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
        // v19 t03：parser 合法产出 kind="mod"（Rust mod 声明 rust.rs、
        // C# namespace csharp.rs），此前落入默认分支产生「未知实体类型」
        // warn 并误标 Function。Module 为容器节点，api.md 渲染已跳过。
        "mod" => NodeKind::Module,
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "fn" | "function" => NodeKind::Function,
        "trait" => NodeKind::Trait,
        "impl" => NodeKind::Impl,
        "type" => NodeKind::Type,
        "const" | "constant" | "static" => NodeKind::Constant,
        "variable" | "let" | "property" => NodeKind::Variable,
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

/// 按行号区间从文件源码中提取函数体文本（供调用边匹配使用）
///
/// line_start/line_end 为 1-based 行号；越界时安全截断（不 panic）。
fn extract_body(source: &str, line_start: usize, line_end: usize) -> String {
    source
        .lines()
        .skip(line_start.saturating_sub(1))
        .take(line_end.saturating_sub(line_start).saturating_add(1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构建调用边（函数 → 被调用函数）
///
/// 在**全图实体构建完成后**调用一次：name_map 此时包含所有文件的函数符号，
/// 才能解析跨文件调用（此前逐文件构建时 name_map 只含已处理文件，跨文件
/// 调用全部丢失，真实图上 Calls 边几乎为零）。
/// 匹配载体 = 函数体文本（按行号从文件源码切片），而非仅签名+文档注释
/// （签名几乎不含调用信息，旧实现导致 Calls 边数量失真）。
fn build_call_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    call_candidates: &[(Entity, NodeId, String)],
) {
    let (name_map, _) = collect_node_names(g);

    for (entity, eid, body) in call_candidates {
        for (callee_name, callee_ids) in &name_map {
            if *callee_name == entity.name {
                continue;
            }
            let pattern = format!("{}(", callee_name);
            // 单词边界检查：确保 callee_name 前不是字母/数字/下划线
            let mut search_start = 0;
            while let Some(pos) = body[search_start..].find(&pattern) {
                let abs_pos = search_start + pos;
                let word_boundary = abs_pos == 0
                    || !body.as_bytes().get(abs_pos - 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
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

    /// v19 t03：parser 合法产出 kind="mod"（Rust mod / C# namespace），
    /// 此前落入默认分支产生「未知实体类型」warn 并误标 Function。
    #[test]
    fn test_kind_from_str_supports_mod() {
        assert_eq!(kind_from_str("mod"), NodeKind::Module);
        assert_eq!(kind_from_str("struct"), NodeKind::Struct);
        assert_eq!(kind_from_str("fn"), NodeKind::Function);
    }

    /// v21 t06：parser 合法产出 kind="static"（Rust static_item 静态变量）
    /// 与 kind="property"（C# 属性）——此前落入默认分支产生「未知实体
    /// 类型 'static'/'property'」warn 并误标 Function。static 语义上是
    /// 常量，property 语义上是字段/变量，归入对应 NodeKind。
    #[test]
    fn test_kind_from_str_supports_static_and_property() {
        assert_eq!(kind_from_str("static"), NodeKind::Constant);
        assert_eq!(kind_from_str("property"), NodeKind::Variable);
        assert_eq!(kind_from_str("function"), NodeKind::Function);
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
                    summary: None, visibility: None,
                },
                Entity {
                    name: "Sub".into(),
                    kind: "struct".into(),
                    line_start: 7,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("struct Sub".into()),
                    summary: None, visibility: None,
                },
            ],
            imports: vec![],
            doc_comments: vec![],
            source: String::new(),
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
                    summary: None, visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
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
                    summary: None, visibility: None,
                }],
                imports: vec![ImportStmt {
                    source: "crate::utils::helper".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
                source: String::new(),
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
            signature: None, visibility: None,
            module_path: vec![],
        });
        let b = kg.graph.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "func_b".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
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

    /// t04：build_call_edges 跨文件调用边——a.rs 定义 callee，b.rs 的 caller
    /// 正文含 "callee(" 应产生 Calls 边（此前该核心功能零单测）
    #[test]
    fn test_build_call_edges_cross_file() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        // call_candidates：(实体, 节点, 函数体源码)
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                summary: None, visibility: None,
            },
            caller,
            "pub fn caller() { callee(42) }".to_string(),
        )];
        build_call_edges(&mut g, &candidates);
        assert_eq!(
            g.edges_connecting(caller, callee).count(),
            1,
            "跨文件调用应产生一条 Calls 边"
        );
        let edge = g.edges_connecting(caller, callee).next().unwrap();
        assert_eq!(edge.weight().kind, EdgeKind::Calls);
    }

    /// t04：单词边界——mycallee( 不应误匹配 callee(
    #[test]
    fn test_build_call_edges_word_boundary() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                summary: None, visibility: None,
            },
            caller,
            "pub fn caller() { mycallee(1) }".to_string(),
        )];
        build_call_edges(&mut g, &candidates);
        assert_eq!(
            g.edges_connecting(caller, callee).count(),
            0,
            "mycallee( 不是对 callee 的调用（前缀字母不构成调用）"
        );
    }

    /// t04：同名自调用排除——callee 调用同名函数不建边；多次出现只建一条边
    #[test]
    fn test_build_call_edges_self_name_skipped_and_dedup() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        // 两个候选都调用 callee（同名实体跳过自身；同一模式重复出现去重）
        let candidates = vec![
            (
                Entity {
                    name: "callee".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: None,
                    summary: None, visibility: None,
                },
                callee,
                "pub fn callee() { callee(1); callee(2) }".to_string(),
            ),
            (
                Entity {
                    name: "other".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: None,
                    summary: None, visibility: None,
                },
                g.add_node(CodeNode {
                    id: NodeId::new(1),
                    kind: NodeKind::Function,
                    name: "other".into(),
                    file_path: Some("src/c.rs".into()),
                    line_range: Some((1, 3)),
                    doc_comment: None,
                    signature: None, visibility: None,
                    module_path: vec![],
                }),
                "pub fn other() { callee(3) }".to_string(),
            ),
        ];
        build_call_edges(&mut g, &candidates);
        // 自调用（callee→callee）不建边
        assert_eq!(
            g.edges_connecting(callee, callee).count(),
            0,
            "同名实体（自调用）应跳过"
        );
        // other→callee 只建一条（去重）
        let other = g.node_indices().find(|&n| g[n].name == "other").unwrap();
        assert_eq!(
            g.edges_connecting(other, callee).count(),
            1,
            "同一调用模式多次出现只建一条边"
        );
    }

    /// t04：无调用时零边
    #[test]
    fn test_build_call_edges_no_call() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let _callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                summary: None, visibility: None,
            },
            caller,
            "pub fn caller() { let x = 1; }".to_string(),
        )];
        build_call_edges(&mut g, &candidates);
        assert_eq!(g.edge_count(), 0, "无调用应零边");
    }
