//! 文件级社区检测：用 leiden-rs（Leiden 算法，CPM 质量函数）在文件依赖图上
//! 划分功能内聚的模块社区，替代旧的"目录前缀凝聚聚类"。
//!
//! 设计要点（对应演进计划 T1.2/T1.3）：
//! - 聚类单位是 **File 节点**，依赖边取跨文件的 Imports/Calls
//!   （同文件内调用是内部实现细节，不参与模块划分）；
//! - 边权重与 graph.rs 构建时一致（Imports=0.8 / Calls=0.7），
//!   同对文件间多条边按权重相加聚合；
//! - CPM 质量函数 + 固定种子（确定性输出，可复现）；
//! - 无跨文件依赖时退化为"每文件一个社区"（Leiden 对无边图无划分意义）；
//! - 社区命名三档规则见 [`community_name`]（T1.3）：公共目录前缀 →
//!   文件数最多目录 → module_{n}，保证产物路径与断链检测的确定性。

use std::collections::HashMap;

use petgraph::visit::{EdgeRef, IntoEdgeReferences, IntoNodeReferences};

use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, QualityType};

use crate::model::*;

/// Leiden 固定种子：保证同一代码库多次运行产出相同社区划分
const LEIDEN_SEED: u64 = 42;
/// CPM 分辨率参数 γ：越高社区越碎。实测：γ=0.8 时单条跨文件调用边
/// （权重 0.7）不满足合并增益，会把协作文件拆成独立社区（过碎）；
/// γ=0.5 时单边足够合并（0.7−0.5=0.2>0），多边强连接社区不受影响。
/// 对代码图而言"任何跨文件调用都表示协作"，取 0.5（演进计划 D4 实测调参）
const LEIDEN_RESOLUTION: f64 = 0.5;

/// 社区检测边权重（与 src/analysis/graph.rs 构建时的字面量同源）
pub const WEIGHT_IMPORTS: f64 = 0.8;
pub const WEIGHT_CALLS: f64 = 0.7;

/// 文件级社区检测：返回 File 节点的社区划分（每社区一个 Vec<NodeId>）
///
/// 输出确定性：社区内按 file_path 字典序排序后作为分组键排序。
pub fn detect_communities(graph: &KnowledgeGraph) -> Vec<Vec<NodeId>> {
    let file_nodes: Vec<NodeId> = graph
        .graph
        .node_references()
        .filter(|(_, n)| n.kind == NodeKind::File)
        .map(|(id, _)| id)
        .collect();

    if file_nodes.is_empty() {
        return Vec::new();
    }
    if file_nodes.len() == 1 {
        return vec![file_nodes];
    }

    // File 节点 → 紧凑索引（leiden-rs 要求 0..n 连续，StableDiGraph 有洞）
    let compact: HashMap<NodeId, usize> = file_nodes
        .iter()
        .enumerate()
        .map(|(i, &nid)| (nid, i))
        .collect();

    // 实体 → 所属 File 映射（经 Contains 边反查）。
    // 关键语义：Calls/Imports 边都挂在**实体节点**上（File 节点
    // 只有 Contains 边），社区划分的单位是 File，必须先归位。
    let mut entity_to_file: HashMap<NodeId, NodeId> = HashMap::new();
    for edge in graph.graph.edge_references() {
        let kind = graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
        if kind == Some(EdgeKind::Contains) && compact.contains_key(&edge.source()) {
            entity_to_file.insert(edge.target(), edge.source());
        }
    }
    // 端点归位：File 节点取自身，实体节点取其所属 File
    let file_of = |nid: NodeId| -> Option<NodeId> {
        if compact.contains_key(&nid) {
            Some(nid)
        } else {
            entity_to_file.get(&nid).copied()
        }
    };

    // 聚合跨文件依赖边：同对文件间多条边权重相加
    let mut edge_weights: HashMap<(usize, usize), f64> = HashMap::new();
    for edge in graph.graph.edge_references() {
        // 结构安全证据（R1 审计）：边权重由 build_graph 创建边时同步写入
        //（StableDiGraph 的边权重不是 Option），此处仅做类型层面的取值；
        // 若 graph 构造代码未来绕过权重初始化，本 expect 会立即失败暴露。
        let e = graph
            .graph
            .edge_weight(edge.id())
            .expect("边权重必然存在");
        let w = match e.kind {
            EdgeKind::Imports => WEIGHT_IMPORTS,
            EdgeKind::Calls => WEIGHT_CALLS,
            _ => continue, // 其他类型边（Contains/Implements）不参与社区划分
        };
        let (Some(sf), Some(tf)) = (file_of(edge.source()), file_of(edge.target())) else {
            continue; // 端点既不是 File 也不是实体（Module 等中间节点）
        };
        let (Some(&si), Some(&ti)) = (compact.get(&sf), compact.get(&tf)) else {
            continue;
        };
        if si == ti {
            continue; // 同文件内依赖不构成模块间关系
        }
        *edge_weights.entry((si, ti)).or_insert(0.0) += w;
    }

    if edge_weights.is_empty() {
        // 无任何跨文件依赖：Leiden 对无边图无划分意义，每文件自成社区
        return file_nodes.into_iter().map(|nid| vec![nid]).collect();
    }

    // 构建 Leiden 输入图（有向、加权；add_edge 校验权重有限且 ≥0，不会失败）
    // 结构安全证据（R1 审计）：权重来源仅两处——常量 WEIGHT_IMPORTS/
    // WEIGHT_CALLS（0.7/0.8，有限非负）与下方 edge_weights 的累加和；
    // 不存在 NaN/Inf/负值注入路径，add_edge 的 Err 分支不可达。
    let mut builder = GraphDataBuilder::new(file_nodes.len()).directed();
    for ((s, t), w) in &edge_weights {
        builder
            .add_edge(*s, *t, *w)
            .expect("边权重均为有限非负数");
    }
    let data = builder.build().expect("图数据构造失败");

    let config = LeidenConfig {
        quality: QualityType::CPM,
        resolution: LEIDEN_RESOLUTION,
        seed: Some(LEIDEN_SEED),
        ..Default::default()
    };
    // 结构安全证据（R1 审计）：leiden-rs 0.8.1 对用户输入从不 panic——
    // 空图/单节点/无边图均提前安全返回（本函数在 edge_weights 为空时
    // 已提前返回单文件社区，此处输入恒为有效图）；run() 仅返回
    // internal-error 类 Result（图内部不变量破坏，不可由调用方触发）。
    let result = Leiden::new(config)
        .run(&data)
        .expect("Leiden 社区检测失败");
    let membership = result.partition.as_slice(); // membership[i] = 节点 i 的社区 ID

    // 按社区 ID 分组回 File 节点
    let mut groups: HashMap<usize, Vec<NodeId>> = HashMap::new();
    for (i, &comm) in membership.iter().enumerate() {
        groups.entry(comm).or_default().push(file_nodes[i]);
    }

    // 确定性输出：按社区内最小 file_path 排序
    let mut communities: Vec<Vec<NodeId>> = groups.into_values().collect();
    communities.sort_by_key(|files| {
        files
            .iter()
            .map(|nid| {
                graph
                    .graph
                    .node_weight(*nid)
                    .and_then(|n| n.file_path.clone())
                    .unwrap_or_default()
            })
            .min()
            .unwrap_or_default()
    });
    communities
}

/// 社区命名三档规则（T1.3）：
///
/// 1. **公共目录前缀**：所有文件父目录的最长公共目录段 → `join("::")`
///    （最贴近"目录=模块"的直觉，且模块名→产物文件名/断链检测全部稳定）；
/// 2. **文件数最多目录**：公共前缀为空（不同根）时，按父目录分组取文件数
///    最多的目录名；
/// 3. **module_{n}**：仍为空（如根目录散文件）时按序号回退。
///
/// 纯函数：仅依赖文件路径，不依赖图结构，可独立单测。
pub fn community_name(files: &[String], fallback_index: usize) -> String {
    if files.is_empty() {
        return format!("module_{fallback_index}");
    }

    let dirs: Vec<Vec<String>> = files.iter().map(|p| dir_segments(p)).collect();

    // 档 1：所有文件父目录的最长公共目录段
    let min_len = dirs.iter().map(|d| d.len()).min().unwrap_or(0);
    let mut common = 0usize;
    'outer: for i in 0..min_len {
        let seg = &dirs[0][i];
        for other in &dirs[1..] {
            if other.get(i) != Some(seg) {
                break 'outer;
            }
        }
        common = i + 1;
    }
    if common > 0 {
        return dirs[0][..common].join("::");
    }

    // 档 2：文件数最多的父目录（根目录散文件用占位键，最终落档 3）
    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    for d in &dirs {
        let key = if d.is_empty() {
            "<root>".to_string()
        } else {
            d.join("::")
        };
        *dir_counts.entry(key).or_insert(0) += 1;
    }
    if let Some((best, _)) = dir_counts
        .into_iter()
        .max_by_key(|(name, count)| (*count, name.clone()))
        && best != "<root>"
    {
        return best;
    }

    // 档 3：确定性回退
    format!("module_{fallback_index}")
}

/// 提取路径的父目录段（不含文件名与盘符 Prefix，Windows 安全）
fn dir_segments(path: &str) -> Vec<String> {
    use std::path::Component;
    std::path::Path::new(path)
        .parent()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个小型知识图谱：src/a.rs、src/b.rs 跨文件调用，src/net/tcp.rs 独立
    fn make_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_file =
            |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
             path: &str|
             -> (NodeId, NodeId) {
                let module_path: Vec<String> = dir_segments(path);
                let nid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::File,
                    name: path.into(),
                    file_path: Some(path.into()),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    module_path,
                });
                // File → Entity 的 Contains 边
                let eid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::Function,
                    name: format!("f{}", nid.index()),
                    file_path: Some(path.into()),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
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
                (nid, eid)
            };
        let (_a, ea) = add_file(g, "src/a.rs");
        let (_b, eb) = add_file(g, "src/b.rs");
        let _tcp = add_file(g, "src/net/tcp.rs");
        // a 的实体 → b 的实体（跨文件 Calls）
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
        kg
    }

    #[test]
    fn test_detect_communities_basic() {
        let kg = make_graph();
        let communities = detect_communities(&kg);
        // a/b 有跨文件调用 → 同社区；tcp.rs 无边 → 自成社区
        assert_eq!(communities.len(), 2, "应产出 2 个社区");
        let ab = communities
            .iter()
            .find(|c| c.len() == 2)
            .expect("应存在含 2 文件的社区");
        let paths: Vec<String> = ab
            .iter()
            .map(|nid| kg.graph.node_weight(*nid).unwrap().file_path.clone().unwrap())
            .collect();
        assert!(paths.contains(&"src/a.rs".to_string()));
        assert!(paths.contains(&"src/b.rs".to_string()));
    }

    #[test]
    fn test_detect_communities_empty_graph() {
        let kg = KnowledgeGraph::default();
        assert!(detect_communities(&kg).is_empty());
    }

    #[test]
    fn test_detect_communities_single_file() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::File,
            name: "src/main.rs".into(),
            file_path: Some("src/main.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["src".into()],
        });
        let communities = detect_communities(&kg);
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0].len(), 1);
    }

    #[test]
    fn test_community_name_common_prefix() {
        let files = vec!["src/net/tcp.rs".to_string(), "src/net/udp.rs".to_string()];
        assert_eq!(community_name(&files, 0), "src::net");
    }

    #[test]
    fn test_community_name_single_file() {
        let files = vec!["src/config.rs".to_string()];
        assert_eq!(community_name(&files, 3), "src");
    }

    #[test]
    fn test_community_name_most_populated_dir() {
        // 无公共前缀（不同根）：按文件数最多目录回退
        let files = vec![
            "app/main.rs".to_string(),
            "app/util.rs".to_string(),
            "lib/helper.rs".to_string(),
        ];
        assert_eq!(community_name(&files, 1), "app");
    }

    #[test]
    fn test_community_name_fallback() {
        let files = vec!["main.rs".to_string()];
        assert_eq!(community_name(&files, 7), "module_7");
        assert_eq!(community_name(&[], 2), "module_2");
    }
}
