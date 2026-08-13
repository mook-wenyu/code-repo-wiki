pub mod document;
pub mod edge;
pub mod node;

pub use document::*;
pub use edge::*;
pub use node::*;

use petgraph::stable_graph::StableDiGraph;
use serde::{Deserialize, Serialize};

/// 完整的知识图谱
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeGraph {
    pub graph: StableDiGraph<CodeNode, CodeEdge>,
    /// 模块聚类结果
    pub modules: Vec<ModuleCluster>,
    /// 实体级特征聚类结果（跨文件协作实现同一功能的方法组）
    #[serde(default)]
    pub features: Vec<Feature>,
}

/// 模块聚类
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleCluster {
    pub name: String,
    pub node_ids: Vec<NodeId>,
    pub cohesion: f64,
    pub coupling: f64,
    pub description: Option<String>,
}

/// 特征聚类：跨文件协作实现同一功能的一组方法（演进计划 T1.2b）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    /// 特征名（确定性命名，如 feature_0）
    pub name: String,
    /// 参与该特征的方法节点集合
    pub node_ids: Vec<NodeId>,
    /// LLM 生成的特征职责描述（卡片生成阶段填充）
    pub description: Option<String>,
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self {
            graph: StableDiGraph::new(),
            modules: Vec::new(),
            features: Vec::new(),
        }
    }
}
