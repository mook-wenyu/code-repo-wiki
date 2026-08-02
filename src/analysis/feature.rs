//! 实体级特征聚类（演进计划 T1.2b）
//!
//! 特征（Feature）= 跨文件协作实现同一功能的一组方法，对应论文中
//! RepoSummary 的 "functional feature" 与 RepoDoc 的功能内聚单元。
//!
//! 实现要点：
//! - 聚类单位 = Function 实体，边 = **跨文件** Calls 边（同文件内调用是
//!   内部实现细节，不构成特征协作）；
//! - 边权重 = 结构相似度与语义相似度各半融合（DIP：本模块只定义
//!   [`Embedder`] 窄接口，具体实现由 generate::embed::EmbeddingEngine 承担，
//!   lib.rs 注入）；无 embedder 或 embedding 失败时降级为纯结构聚类；
//! - 结构相似度 = 0.5（存在跨文件调用即视为协作）+ 0.5 × 公共调用邻居
//!   Jaccard（调用网络越相似，协作越紧密）；
//! - 聚类算法与模块检测一致：Leiden CPM + 固定种子（确定性输出）。

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, QualityType};
use petgraph::visit::{EdgeRef, IntoEdgeReferences, IntoNodeReferences};

use crate::model::*;

/// 特征聚类 Leiden 固定种子（与 community.rs 一致，保证可复现）
const FEATURE_LEIDEN_SEED: u64 = 42;
/// CPM 分辨率：特征粒度应比模块细（方法组），单边协作（结构权重 0.5）
/// 需满足 0.5−γ>0 才合并，取 0.4；多边协作权重更高，不受影响。
const FEATURE_RESOLUTION: f64 = 0.4;
/// 结构相似度占比（存在跨文件调用即给 0.5 基线分）
const STRUCTURE_WEIGHT: f64 = 0.5;
/// 语义相似度占比（embedding 融合）
const SEMANTIC_WEIGHT: f64 = 0.5;

/// 向量嵌入窄接口（analysis 层不依赖具体实现，实现方见 generate::embed）。
///
/// 刻意使用同步方法而非 async fn in trait：当前编译器不支持 async fn in
/// trait 的 dyn 分发（llm.rs 已记载该限制），而特征聚类需要
/// `Option<&dyn Embedder>` 做运行时注入；同步方法由实现方内部驱动
/// tokio Runtime（EmbeddingEngine 持有 Handle）。
pub trait Embedder: Send + Sync {
    /// 单文本嵌入
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// 批量嵌入
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// 余弦相似度（返回 0.0~1.0，负值截断为 0）
    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64;
}

/// 实体级特征聚类
///
/// `embedder` 为 None 或嵌入失败时降级为纯结构聚类（结构权重即总权重）。
pub fn detect_features(
    graph: &KnowledgeGraph,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<Feature>> {
    // 1. 收集 Function 实体
    let funcs: Vec<NodeId> = graph
        .graph
        .node_references()
        .filter(|(_, n)| n.kind == NodeKind::Function)
        .map(|(id, _)| id)
        .collect();
    if funcs.is_empty() {
        return Ok(Vec::new());
    }
    let func_set: HashSet<NodeId> = funcs.iter().copied().collect();

    // 2. 实体 → 所属文件（经 Contains 边反查，判定跨文件调用）
    let mut entity_to_file: HashMap<NodeId, NodeId> = HashMap::new();
    for edge in graph.graph.edge_references() {
        let kind = graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
        if kind == Some(EdgeKind::Contains) {
            entity_to_file.insert(edge.target(), edge.source());
        }
    }

    // 3. 跨文件调用边 + 调用邻居集
    let mut neighbors: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    let mut cross_edges: Vec<(NodeId, NodeId)> = Vec::new();
    for edge in graph.graph.edge_references() {
        // 结构安全证据（R1 审计）：边权重由 build_graph 创建边时同步写入，
        // 非 Option 类型；此处 expect 仅暴露未来构造代码的回归。
        let e = graph
            .graph
            .edge_weight(edge.id())
            .expect("边权重必然存在");
        if e.kind != EdgeKind::Calls {
            continue;
        }
        let (s, t) = (edge.source(), edge.target());
        if !func_set.contains(&s) || !func_set.contains(&t) {
            continue;
        }
        if entity_to_file.get(&s) == entity_to_file.get(&t) {
            continue; // 同文件内调用
        }
        neighbors.entry(s).or_default().insert(t);
        neighbors.entry(t).or_default().insert(s);
        cross_edges.push((s, t));
    }
    if cross_edges.is_empty() {
        return Ok(Vec::new());
    }

    // 4. 可选 embedding（只嵌入参与跨文件协作的实体；失败降级纯结构）
    let embeddings: Option<HashMap<NodeId, Vec<f32>>> = if let Some(emb) = embedder {
        let mut involved: Vec<NodeId> = cross_edges
            .iter()
            .flat_map(|(s, t)| [*s, *t])
            .collect();
        involved.sort();
        involved.dedup();
        let texts: Vec<String> = involved
            .iter()
            .map(|nid| {
                // 结构安全证据（R1 审计）：involved 来自 cross_edges 的端点
                //（从 graph 边引用收集，端点必然有节点权重）。
                let n = graph.graph.node_weight(*nid).expect("实体节点必然存在");
                format!(
                    "{} {:?} {}",
                    n.name,
                    n.kind,
                    n.signature.as_deref().unwrap_or("")
                )
            })
            .collect();
        match emb.embed_batch(&texts) {
            Ok(vecs) => Some(involved.into_iter().zip(vecs).collect()),
            Err(e) => {
                tracing::warn!("特征聚类 embedding 失败，降级为纯结构聚类: {e}");
                None
            }
        }
    } else {
        None
    };

    // 5. 融合边权重
    let compact: HashMap<NodeId, usize> = funcs
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i))
        .collect();
    let mut weights: HashMap<(usize, usize), f64> = HashMap::new();
    for (s, t) in &cross_edges {
        let structural = 0.5 + 0.5 * jaccard(neighbors.get(s), neighbors.get(t));
        let semantic = match (&embeddings, embedder) {
            (Some(em), Some(emb)) => match (em.get(s), em.get(t)) {
                (Some(a), Some(b)) => emb.cosine_similarity(a, b),
                _ => 0.0,
            },
            _ => 0.0,
        };
        let weight = STRUCTURE_WEIGHT * structural + SEMANTIC_WEIGHT * semantic;
        let (si, ti) = (compact[s], compact[t]);
        *weights.entry((si, ti)).or_insert(0.0) += weight;
    }

    // 6. Leiden CPM 聚类
    // 结构安全证据（R1 审计）：structural 是 0.5~1.0 的 Jaccard 变换
    //（jaccard 本身 ∈[0,1]）；semantic 来自 embedder.cosine_similarity——
    // **契约：Embedder 实现必须返回有限值**（唯一实现 EmbeddingEngine
    // 对零范数向量返回 0.0 并 clamp，恒满足）。若未来新增 Embedder
    // 实现返回 NaN/Inf，此处 add_edge 会失败并触发 panic——这是有意
    // 的 fail-loud 契约校验（票 07 裁决：信任边界违约必须暴露，而非
    // 静默降级掩盖 bug）。常量权重 STRUCTURE_WEIGHT=0.5/SEMANTIC_WEIGHT=0.5
    // 有限非负，无其他注入路径。
    let mut builder = GraphDataBuilder::new(funcs.len()).directed();
    for ((s, t), w) in &weights {
        builder
            .add_edge(*s, *t, *w)
            .expect("边权重均为有限非负数（Embedder 契约：余弦相似度必须有限）");
    }
    let data = builder.build().expect("图数据构造失败");
    let config = LeidenConfig {
        quality: QualityType::CPM,
        resolution: FEATURE_RESOLUTION,
        seed: Some(FEATURE_LEIDEN_SEED),
        ..Default::default()
    };
    // 结构安全证据（R1 审计）：leiden-rs 0.8.1 对用户输入从不 panic
    //（空图/单节点/无边图安全返回），run() 仅内部不变量可失败。
    let result = Leiden::new(config)
        .run(&data)
        .expect("Leiden 特征聚类失败");
    let membership = result.partition.as_slice();

    // 7. 组装 Feature（确定性：按社区内最小实体名排序）
    let mut groups: HashMap<usize, Vec<NodeId>> = HashMap::new();
    for (i, &comm) in membership.iter().enumerate() {
        groups.entry(comm).or_default().push(funcs[i]);
    }
    let mut features: Vec<Vec<NodeId>> = groups
        .into_values()
        .map(|mut node_ids| {
            node_ids.sort_by_key(|nid| {
                graph
                    .graph
                    .node_weight(*nid)
                    .map(|n| n.name.clone())
                    .unwrap_or_default()
            });
            node_ids
        })
        .collect();
    features.sort_by_key(|node_ids| {
        node_ids
            .first()
            .map(|nid| {
                graph
                    .graph
                    .node_weight(*nid)
                    .map(|n| n.name.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    });
    Ok(features
        .into_iter()
        .enumerate()
        .map(|(idx, node_ids)| Feature {
            name: format!("feature_{idx}"),
            node_ids,
            description: None,
        })
        .collect())
}

/// 两个邻居集的 Jaccard 相似度（|A∩B| / |A∪B|，空并集为 0）
fn jaccard(a: Option<&HashSet<NodeId>>, b: Option<&HashSet<NodeId>>) -> f64 {
    match (a, b) {
        (Some(a), Some(b)) => {
            let union: HashSet<NodeId> = a.union(b).copied().collect();
            if union.is_empty() {
                0.0
            } else {
                a.intersection(b).count() as f64 / union.len() as f64
            }
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造图：a.rs 的 fa 调用 b.rs 的 fb（跨文件），c.rs 的 fc 调用 d.rs 的 fd
    fn make_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_file =
            |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
             path: &str|
             -> (NodeId, NodeId) {
                let nid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::File,
                    name: path.into(),
                    file_path: Some(path.into()),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    module_path: vec!["src".into()],
                });
                let eid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::Function,
                    name: format!("fn_{}", path.replace(['/', '.'], "_")),
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
        let (_fa_file, fa) = add_file(g, "src/a.rs");
        let (_fb_file, fb) = add_file(g, "src/b.rs");
        let (_fc_file, fc) = add_file(g, "src/c.rs");
        let (_fd_file, fd) = add_file(g, "src/d.rs");
        // fa→fb、fc→fd 两对跨文件调用
        for (s, t) in [(fa, fb), (fc, fd)] {
            g.add_edge(
                s,
                t,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Calls,
                    source: s,
                    target: t,
                    weight: 0.7,
                    location: None,
                },
            );
        }
        kg
    }

    #[test]
    fn test_detect_features_basic() {
        let kg = make_graph();
        let features = detect_features(&kg, None).unwrap();
        assert!(features.len() >= 2, "应检出至少 2 个特征: {:?}", features);
        let names: Vec<String> = features
            .iter()
            .flat_map(|f| {
                f.node_ids
                    .iter()
                    .map(|nid| kg.graph.node_weight(*nid).unwrap().name.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(names.contains(&"fn_src_a_rs".to_string()));
        assert!(names.contains(&"fn_src_b_rs".to_string()));
    }

    #[test]
    fn test_detect_features_empty_graph() {
        let kg = KnowledgeGraph::default();
        let features = detect_features(&kg, None).unwrap();
        assert!(features.is_empty());
    }

    #[test]
    fn test_detect_features_no_cross_file_calls() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let f = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::File,
            name: "src/a.rs".into(),
            file_path: Some("src/a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec!["src".into()],
        });
        let e1 = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "f1".into(),
            file_path: Some("src/a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: Vec::new(),
        });
        let e2 = g.add_node(CodeNode {
            id: NodeId::new(2),
            kind: NodeKind::Function,
            name: "f2".into(),
            file_path: Some("src/a.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: Vec::new(),
        });
        for e in [e1, e2] {
            g.add_edge(
                f,
                e,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: f,
                    target: e,
                    weight: 1.0,
                    location: None,
                },
            );
        }
        // 同文件内调用
        g.add_edge(
            e1,
            e2,
            CodeEdge {
                id: EdgeId::new(g.edge_count()),
                kind: EdgeKind::Calls,
                source: e1,
                target: e2,
                weight: 0.7,
                location: None,
            },
        );
        let features = detect_features(&kg, None).unwrap();
        assert!(features.is_empty(), "同文件调用不构成特征");
    }
}
