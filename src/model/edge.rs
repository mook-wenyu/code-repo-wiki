use petgraph::stable_graph::EdgeIndex;
use serde::{Deserialize, Serialize};

use super::node::NodeId;

/// 边 ID（映射到 petgraph 的 EdgeIndex）
pub type EdgeId = EdgeIndex<u32>;

/// 关系边
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    pub id: EdgeId,
    pub kind: EdgeKind,
    pub source: NodeId,
    pub target: NodeId,
    /// 关系权重（0.0~1.0），用于影响传播和聚类
    pub weight: f64,
    /// 位置信息（如导入语句所在行）
    pub location: Option<(usize, usize)>,
}

/// 关系类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EdgeKind {
    /// 包含关系（Project → Module → File → Entity）
    Contains,
    /// 调用关系（函数 A → 函数 B）
    Calls,
    /// 导入/引用关系（文件 → 模块/文件）
    Imports,
    /// 实现关系（Impl → Trait）
    Implements,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Contains => "contains",
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Implements => "implements",
        }
    }

}
