use petgraph::stable_graph::NodeIndex;
use serde::{Deserialize, Serialize};

/// 节点 ID（映射到 petgraph 的 NodeIndex）
pub type NodeId = NodeIndex<u32>;

/// 代码实体节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNode {
    /// 在 petgraph 图中的索引
    pub id: NodeId,
    /// 实体类型
    pub kind: NodeKind,
    /// 实体名称
    pub name: String,
    /// 所属文件路径（相对项目根）
    pub file_path: Option<String>,
    /// 源代码行范围 [start, end]
    pub line_range: Option<(usize, usize)>,
    /// 文档注释
    pub doc_comment: Option<String>,
    /// 函数/类型签名
    pub signature: Option<String>,
    /// 模块路径（用 :: 分隔）
    pub module_path: Vec<String>,
}

/// 节点类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeKind {
    /// 项目根
    Project,
    /// 模块/目录
    Module,
    /// 文件
    File,
    /// 结构体
    Struct,
    /// 枚举
    Enum,
    /// 函数/方法
    Function,
    /// Trait
    Trait,
    /// Trait 实现
    Impl,
    /// 类型别名
    Type,
    /// 常量
    Constant,
    /// 变量
    Variable,
    /// 接口（TypeScript/Go/Java）
    Interface,
    /// 类
    Class,
    /// 宏
    Macro,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Project => "project",
            NodeKind::Module => "module",
            NodeKind::File => "file",
            NodeKind::Struct => "struct",
            NodeKind::Enum => "enum",
            NodeKind::Function => "function",
            NodeKind::Trait => "trait",
            NodeKind::Impl => "impl",
            NodeKind::Type => "type",
            NodeKind::Constant => "constant",
            NodeKind::Variable => "variable",
            NodeKind::Interface => "interface",
            NodeKind::Class => "class",
            NodeKind::Macro => "macro",
        }
    }

    /// 优先级排序（值越小优先级越高），用于分层显示
    pub fn priority(&self) -> u8 {
        match self {
            NodeKind::Project => 0,
            NodeKind::Module => 1,
            NodeKind::File => 2,
            NodeKind::Struct | NodeKind::Class | NodeKind::Interface => 3,
            NodeKind::Trait => 4,
            NodeKind::Impl => 5,
            NodeKind::Enum => 6,
            NodeKind::Function => 7,
            NodeKind::Type => 8,
            NodeKind::Constant | NodeKind::Variable => 9,
            NodeKind::Macro => 10,
        }
    }
}
