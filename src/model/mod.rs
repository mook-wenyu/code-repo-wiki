pub mod node;
pub mod edge;
pub mod document;

pub use node::*;
pub use edge::*;
pub use document::*;

use petgraph::stable_graph::StableDiGraph;
use serde::{Deserialize, Serialize};

/// 完整的知识图谱
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeGraph {
    pub graph: StableDiGraph<CodeNode, CodeEdge>,
    /// 模块聚类结果
    pub modules: Vec<ModuleCluster>,
}

/// 模块聚类
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleCluster {
    pub name: String,
    pub node_ids: Vec<NodeId>,
    pub cohesion: f64,
    pub coupling: f64,
    pub description: Option<String>,
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self {
            graph: StableDiGraph::new(),
            modules: Vec::new(),
        }
    }
}
