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

    // 预计算模块名 → 节点集合的映射，用于依赖分析
    let module_node_ids: std::collections::HashMap<&str, HashSet<NodeIndex>> = modules
        .iter()
        .map(|m| (m.name.as_str(), m.node_ids.iter().copied().collect()))
        .collect();

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

        for insight in insights {
            if module_file_paths.contains(&insight.path) {
                entities.extend(insight.entities.clone());
                imports.extend(insight.imports.clone());
                if !file_paths.contains(&insight.path) {
                    file_paths.push(insight.path.clone());
                }
            }
        }

        entities.sort_by(|a, b| a.name.cmp(&b.name));
        entities.dedup_by(|a, b| a.name == b.name);

        let module_path: Vec<String> = module.name.split("::").map(|s| s.to_string()).collect();

        // 计算实际依赖：从本模块节点出发，通过 Imports/DependsOn 边到达其他模块的节点
        let mut deps: Vec<String> = Vec::new();
        for (&other_name, other_set) in &module_node_ids {
            if other_name == module.name {
                continue;
            }
            let has_dep = module.node_ids.iter().any(|nid| {
                graph.graph.edges(*nid).any(|e| {
                    let kind = &graph.graph[e.id()].kind;
                    (kind == &EdgeKind::Imports || kind == &EdgeKind::DependsOn)
                        && other_set.contains(&e.target())
                })
            });
            if has_dep {
                deps.push(other_name.to_string());
            }
        }

        chunks.push(Chunk {
            module_path,
            entities,
            imports,
            dependencies: deps,
            file_paths,
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
            summary: None,
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
}
