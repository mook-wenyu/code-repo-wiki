use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;

use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
use crate::model::{EdgeKind, KnowledgeGraph, ModuleCluster, NodeKind};

/// AST 感知的数据块，用于 LLM 生成
///
/// 每个 Chunk 代表一个生成单元——可以是一个模块或单个文件。
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 模块路径（如 ["crate", "generate", "llm"]）
    pub module_path: Vec<String>,
    /// 块中的实体列表
    pub entities: Vec<Entity>,
    /// 块的导入语句
    pub imports: Vec<ImportStmt>,
    /// 依赖的其他模块名
    pub dependencies: Vec<String>,
    /// 关联的源文件路径
    pub file_paths: Vec<PathBuf>,
    /// 每个实体所属的源文件（与 entities 平行；空表示未记录，
    /// 供增量场景按实体级过滤摘要生成——演进计划 T2.3）
    pub entity_sources: Vec<PathBuf>,
}

impl Chunk {
    /// 是否为空块（无实体和导入）
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.imports.is_empty()
    }

    /// 实体数量
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
}

/// 从知识图谱构建 NodeId → 文件路径的映射
///
/// 只包含 File 类型的节点，用于 chunk_by_module 的精确路径匹配。
pub fn build_node_to_file_map(graph: &KnowledgeGraph) -> HashMap<NodeIndex, PathBuf> {
    let mut map = HashMap::new();
    for n in graph.graph.node_indices() {
        if let Some(w) = graph.graph.node_weight(n)
            && w.kind == NodeKind::File
            && let Some(ref fp) = w.file_path
        {
            map.insert(n, PathBuf::from(fp));
        }
    }
    map
}

/// 构建节点 → 模块名映射（模块归属唯一真源）
///
/// 命名坑：`CodeNode.module_path`（目录+文件 stem 派生名）≠
/// `ModuleCluster.name`（社区名，如 `src::net`）。依赖方/调用方模块
/// 归属必须走 `ModuleCluster.node_ids → 节点 → 模块名` 的映射，
/// 不得用 `node.module_path`（否则同目录文件归错模块）。
/// 同名节点多归属时先到先得（与 export_modules/index 同规则）。
pub fn build_node_to_module_map(
    modules: &[ModuleCluster],
) -> std::collections::HashMap<NodeIndex, &str> {
    modules
        .iter()
        .flat_map(|m| m.node_ids.iter().map(move |id| (*id, m.name.as_str())))
        .collect()
}

/// 以模块为单位进行 AST 感知分块
///
/// 将 FileInsight 按 ModuleCluster 分组，通过 node_id → file_path 映射精确匹配，
/// 同一模块的多个文件合并到一个 Chunk。
/// 返回的 Chunk 列表与 modules 一一对应。
pub fn chunk_by_module(
    insights: &[FileInsight],
    modules: &[ModuleCluster],
    graph: &KnowledgeGraph,
) -> Vec<Chunk> {
    let node_to_file = build_node_to_file_map(graph);
    let mut chunks = Vec::with_capacity(modules.len());

    // P2-20：节点 → 模块 反向索引（单遍构建 O(V)）——依赖分析不再对每个
    // 模块双层扫描（原 O(模块数 × 节点数 × 出边数)），改为每模块单遍扫出边、
    // 反向索引 O(1) 定位目标模块（总 O(V + E × 模块数)）。
    // 复用独立函数 build_node_to_module_map（context.rs 也消费同一映射，
    // 保证依赖/调用方归属两处口径一致）。
    let node_to_module = build_node_to_module_map(modules);

    for module in modules {
        // 收集该模块中所有 File 节点的路径
        let module_file_paths: HashSet<&PathBuf> = module
            .node_ids
            .iter()
            .filter_map(|nid| node_to_file.get(nid))
            .collect();

        let mut entities = Vec::new();
        let mut imports = Vec::new();
        let mut file_paths = Vec::new();
        let mut entity_sources = Vec::new();

        for insight in insights {
            if module_file_paths.contains(&insight.path) {
                // 记录实体 → 源文件归属（与 entities 平行，供 T2.3 实体级过滤）
                for _ in &insight.entities {
                    entity_sources.push(insight.path.clone());
                }
                entities.extend(insight.entities.clone());
                imports.extend(insight.imports.clone());
                if !file_paths.contains(&insight.path) {
                    file_paths.push(insight.path.clone());
                }
            }
        }

        // 实体与文件归属按同一键（name）排序去重，保证两者仍平行
        let mut paired: Vec<(Entity, PathBuf)> = entities.into_iter().zip(entity_sources).collect();
        paired.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        // N4：同名实体去重——不同文件定义同名实体时按排序后首个保留，
        // 此前静默丢弃无任何提示；告警暴露去重事实（名称冲突常见于
        // 重载/同名导出，模块页与搜索索引只保留一个定义）
        let before = paired.len();
        paired.dedup_by(|a, b| a.0.name == b.0.name);
        if paired.len() < before {
            tracing::warn!(
                "模块 {} 去重 {} 个同名实体（保留排序后首个定义）",
                module.name,
                before - paired.len()
            );
        }
        let entities: Vec<Entity> = paired.iter().map(|(e, _)| e.clone()).collect();
        let entity_sources: Vec<PathBuf> = paired.iter().map(|(_, f)| f.clone()).collect();

        let module_path: Vec<String> = module.name.split("::").map(|s| s.to_string()).collect();

        // 计算实际依赖：从本模块节点出发，通过 Imports ∪ Calls 边到达其他模块的节点。
        // Imports 表达文件级导入，Calls 表达函数级调用——只取 Imports 会漏掉
        // 「无 use 但直接跨模块调用」的模块级耦合（模块页依赖/交叉引用缺失）。
        // BTreeSet 天然有序去重——取代原 deps.sort()，确定性契约不变（卡片
        // frontmatter 的 dependencies 顺序跨次稳定，评测不漂移）
        let mut deps: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for nid in &module.node_ids {
            for e in graph.graph.edges(*nid) {
                if !matches!(
                    graph.graph[e.id()].kind,
                    EdgeKind::Imports | EdgeKind::Calls
                ) {
                    continue;
                }
                if let Some(&target_mod) = node_to_module.get(&e.target())
                    && target_mod != module.name.as_str()
                {
                    deps.insert(target_mod);
                }
            }
        }
        let deps: Vec<String> = deps.into_iter().map(|s| s.to_string()).collect();

        chunks.push(Chunk {
            module_path,
            entities,
            imports,
            dependencies: deps,
            file_paths,
            entity_sources,
        });
    }

    chunks
}

/// 以文件为单位分块（适用于 Level 0，无模块聚类信息时回退）
pub fn chunk_by_file(insight: &FileInsight) -> Chunk {
    let module_path: Vec<String> = insight
        .path
        .parent()
        .and_then(|p| {
            // 只取普通目录组件，过滤盘符（Prefix）/根目录（RootDir）等
            // 否则 Windows 绝对路径会生成含 ":\" 的 module_path，导致 wiki 文件名非法
            p.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .reduce(|a, b| format!("{}::{}", a, b))
        })
        .map(|s| s.split("::").map(|p| p.to_string()).collect())
        .unwrap_or_default();

    Chunk {
        module_path,
        entities: insight.entities.clone(),
        imports: insight.imports.clone(),
        dependencies: Vec::new(),
        file_paths: vec![insight.path.clone()],
        entity_sources: insight
            .entities
            .iter()
            .map(|_| insight.path.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::Entity;
    use crate::model::KnowledgeGraph;

    fn make_entity(name: &str, kind: &str) -> Entity {
        Entity {
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 10,
            doc_comment: None,
            signature: None,
            visibility: None,
        }
    }

    fn make_insight(file_name: &str, entities: Vec<Entity>) -> FileInsight {
        FileInsight {
            path: PathBuf::from(file_name),
            language: "rust".into(),
            entities,
            imports: Vec::new(),
            doc_comments: Vec::new(),
            source: String::new(),
        }
    }

    #[test]
    fn test_chunk_by_file() {
        let entity = make_entity("MyStruct", "struct");
        let insight = make_insight("src/lib.rs", vec![entity]);
        let chunk = chunk_by_file(&insight);

        assert_eq!(chunk.entity_count(), 1);
        assert!(!chunk.module_path.is_empty());
    }

    #[test]
    fn test_empty_chunk() {
        let entities = vec![make_entity("Foo", "fn")];
        let insight = make_insight("src/main.rs", entities);

        let chunk = chunk_by_file(&insight);
        assert!(!chunk.is_empty());

        let empty_chunk = Chunk {
            module_path: vec![],
            entities: vec![],
            imports: vec![],
            dependencies: vec![],
            file_paths: vec![],
            entity_sources: vec![],
        };
        assert!(empty_chunk.is_empty());
    }

    #[test]
    fn test_chunk_by_module_empty_modules() {
        let graph = KnowledgeGraph::default();
        let insight = make_insight("src/lib.rs", vec![make_entity("Foo", "fn")]);
        let chunks = chunk_by_module(&[insight], &[], &graph);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_build_node_to_file_map() {
        let mut g = petgraph::stable_graph::StableDiGraph::<
            crate::model::CodeNode,
            crate::model::CodeEdge,
        >::new();
        let file_id = g.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: crate::model::NodeKind::File,
            name: "lib.rs".into(),
            file_path: Some("src/lib.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into()],
        });
        let fn_id = g.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(1),
            kind: crate::model::NodeKind::Function,
            name: "foo".into(),
            file_path: Some("src/lib.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "lib".into()],
        });
        let kg = KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        };
        let map = build_node_to_file_map(&kg);
        assert_eq!(map.len(), 1); // 只含 File 节点
        assert!(map.contains_key(&file_id));
        assert!(!map.contains_key(&fn_id));
        assert_eq!(map[&file_id], std::path::PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_chunk_by_module_groups_entities_by_module() {
        // 构造两个模块的图谱：src::a（file_a.rs 含实体 e1）、src::b（file_b.rs 含实体 e2），
        // file_a 通过 Imports 边依赖 file_b，验证实体归属、依赖分析与 entity_sources 平行性
        let mut graph = KnowledgeGraph::default();

        let file_a = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: NodeKind::File,
            name: "file_a.rs".into(),
            file_path: Some("src/a/file_a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let e1 = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(1),
            kind: NodeKind::Function,
            name: "e1".into(),
            file_path: Some("src/a/file_a.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: Some("fn e1()".into()),
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let file_b = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(2),
            kind: NodeKind::File,
            name: "file_b.rs".into(),
            file_path: Some("src/b/file_b.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });
        let e2 = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(3),
            kind: NodeKind::Function,
            name: "e2".into(),
            file_path: Some("src/b/file_b.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: Some("fn e2()".into()),
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });

        // 文件包含实体；a 依赖 b（Imports 边 file_a → file_b）
        graph.graph.add_edge(
            file_a,
            e1,
            crate::model::CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(0),
                kind: EdgeKind::Contains,
                source: file_a,
                target: e1,
                weight: 1.0,
                location: None,
            },
        );
        graph.graph.add_edge(
            file_b,
            e2,
            crate::model::CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(1),
                kind: EdgeKind::Contains,
                source: file_b,
                target: e2,
                weight: 1.0,
                location: None,
            },
        );
        graph.graph.add_edge(
            file_a,
            file_b,
            crate::model::CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(2),
                kind: EdgeKind::Imports,
                source: file_a,
                target: file_b,
                weight: 1.0,
                location: None,
            },
        );

        // 模块按名字典序传入：chunk 与 modules 输入一一对应（chunk_by_module 不重排序）
        let modules = vec![
            ModuleCluster {
                name: "src::a".into(),
                node_ids: vec![file_a, e1],
                cohesion: 0.9,
                coupling: 0.1,
                description: None,
            },
            ModuleCluster {
                name: "src::b".into(),
                node_ids: vec![file_b, e2],
                cohesion: 0.9,
                coupling: 0.1,
                description: None,
            },
        ];
        let insights = vec![
            make_insight("src/a/file_a.rs", vec![make_entity("e1", "fn")]),
            make_insight("src/b/file_b.rs", vec![make_entity("e2", "fn")]),
        ];

        let chunks = chunk_by_module(&insights, &modules, &graph);

        // 两个模块各生成一个 chunk，模块路径按名字拆分为 ["src", "a"] / ["src", "b"]
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0].module_path,
            vec!["src".to_string(), "a".to_string()]
        );
        assert_eq!(
            chunks[1].module_path,
            vec!["src".to_string(), "b".to_string()]
        );
        // 每个 chunk 只包含自己模块的实体
        assert_eq!(chunks[0].entities.len(), 1);
        assert_eq!(chunks[0].entities[0].name, "e1");
        assert_eq!(chunks[1].entities[0].name, "e2");
        // a 依赖 b：Imports 边被依赖分析捕获
        assert_eq!(chunks[0].dependencies, vec!["src::b".to_string()]);
        assert!(chunks[1].dependencies.is_empty());
        // entity_sources 与 entities 平行对应（每个实体记录其源文件）
        assert_eq!(
            chunks[0].entity_sources,
            vec![std::path::PathBuf::from("src/a/file_a.rs")]
        );
        assert_eq!(
            chunks[1].entity_sources,
            vec![std::path::PathBuf::from("src/b/file_b.rs")]
        );
    }

    /// 依赖边扩展（Imports ∪ Calls）：跨模块 Calls 边必须进入 dependencies——
    /// 只取 Imports 会漏掉「无 use 但直接调用」的模块耦合（模块页依赖/交叉引用缺失）。
    /// 调用方上下文已迁出本模块（context.rs 按 Calls 入边实时推导），Chunk 不再
    /// 承载 caller_modules 字段，此处只验证 Calls 边进依赖。
    #[test]
    fn test_chunk_by_module_calls_edge_enters_dependencies() {
        let mut graph = KnowledgeGraph::default();

        let file_a = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(0),
            kind: NodeKind::File,
            name: "file_a.rs".into(),
            file_path: Some("src/a/file_a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let caller_fn = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/a/file_a.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: Some("fn caller()".into()),
            visibility: None,
            module_path: vec!["src".into(), "a".into()],
        });
        let file_b = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(2),
            kind: NodeKind::File,
            name: "file_b.rs".into(),
            file_path: Some("src/b/file_b.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });
        let callee_fn = graph.graph.add_node(crate::model::CodeNode {
            id: petgraph::stable_graph::NodeIndex::new(3),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/b/file_b.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: Some("fn callee()".into()),
            visibility: None,
            module_path: vec!["src".into(), "b".into()],
        });

        for (s, t) in [
            (file_a, caller_fn),
            (file_b, callee_fn),
            (caller_fn, callee_fn), // 跨模块 Calls 边（无 Imports 边）
        ] {
            graph.graph.add_edge(
                s,
                t,
                crate::model::CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(graph.graph.edge_count()),
                    kind: if (s, t) == (caller_fn, callee_fn) {
                        EdgeKind::Calls
                    } else {
                        EdgeKind::Contains
                    },
                    source: s,
                    target: t,
                    weight: 1.0,
                    location: None,
                },
            );
        }

        let modules = vec![
            ModuleCluster {
                name: "src::a".into(),
                node_ids: vec![file_a, caller_fn],
                cohesion: 0.9,
                coupling: 0.1,
                description: None,
            },
            ModuleCluster {
                name: "src::b".into(),
                node_ids: vec![file_b, callee_fn],
                cohesion: 0.9,
                coupling: 0.1,
                description: None,
            },
        ];
        let insights = vec![
            make_insight("src/a/file_a.rs", vec![make_entity("caller", "fn")]),
            make_insight("src/b/file_b.rs", vec![make_entity("callee", "fn")]),
        ];

        let chunks = chunk_by_module(&insights, &modules, &graph);

        // a 调用 b：Calls 边被依赖分析捕获（Imports ∪ Calls）
        assert_eq!(
            chunks[0].dependencies,
            vec!["src::b".to_string()],
            "Calls 边应进入 dependencies"
        );
        // b 无出边 → dependencies 为空
        assert!(chunks[1].dependencies.is_empty());
    }
}
