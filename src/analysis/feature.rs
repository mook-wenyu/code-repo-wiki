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
/// CPM 分辨率：特征粒度应比模块细（方法组），取 0.4——实现事实：
/// 单边协作结构权重 0.5，CPM 合并判据是 边权−γ>0（即 0.5−0.4=0.1），
/// 因此单边协作**不**合并（需 ≥0.4 才满足）；多边协作权重更高，可合并。
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

/// 带块索引的批量嵌入结果（并发分块后按索引重排，保证 zip 不错位）
type IndexedEmbedBatch = (usize, Result<Vec<Vec<f32>>>);

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
        let e = graph.graph.edge_weight(edge.id()).expect("边权重必然存在");
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
        let mut involved: Vec<NodeId> = cross_edges.iter().flat_map(|(s, t)| [*s, *t]).collect();
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
        // v52 T11：embed_batch 对全部跨文件协作端点串行调用（大仓分钟级阻塞
        // analyzing 进度）。embedding 是 IO+等待型负载，小并发即可显著提速且
        // 不撞网关限流（LLM 通道已 128 并发，embedding 仍是串行瓶颈）。
        // 并发在调用侧做（Embedder trait 同步签名不改）；结果按 involved 的
        // 原始顺序重排后再组装，保证 zip 不错位、确定性契约不破。
        const EMBED_THREADS: usize = 4;
        let chunk_size = texts.len().div_ceil(EMBED_THREADS).max(1);
        let results: Vec<IndexedEmbedBatch> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(EMBED_THREADS);
            for (chunk_idx, chunk) in texts.chunks(chunk_size).enumerate() {
                handles.push(scope.spawn(move || (chunk_idx, emb.embed_batch(chunk))));
            }
            handles
                .into_iter()
                .map(|h| match h.join() {
                    Ok(r) => r,
                    Err(_) => {
                        // 子线程 panic 视为 embedding 失败，走既有降级路径（纯结构聚类），
                        // 不让一个线程的 panic 炸掉整个流水线。
                        let msg = "特征聚类 embedding 子线程 panic，降级为纯结构聚类";
                        tracing::warn!("{msg}");
                        (0usize, Err(anyhow::anyhow!(msg)))
                    }
                })
                .collect()
        });
        let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        let mut failed = false;
        for (_, chunk_result) in results {
            match chunk_result {
                Ok(chunk_vecs) => vecs.extend(chunk_vecs),
                Err(e) => {
                    tracing::warn!("特征聚类 embedding 失败，降级为纯结构聚类: {e}");
                    failed = true;
                }
            }
        }
        if failed {
            None
        } else {
            Some(involved.into_iter().zip(vecs).collect())
        }
    } else {
        None
    };

    // 5. 融合边权重
    let compact: HashMap<NodeId, usize> = funcs.iter().enumerate().map(|(i, &n)| (n, i)).collect();
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
    let result = Leiden::new(config).run(&data).expect("Leiden 特征聚类失败");
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
    // v52 T11：过滤单节点社区（孤立/弱连接实体在 Leiden 中各自成团，
    // 不构成"功能特征"，只是卡片"特征追溯"的噪音）。过滤后 idx 连续重编号。
    features.retain(|node_ids| node_ids.len() >= 2);
    features.sort_by(|a, b| {
        let key = |ids: &Vec<NodeId>| {
            ids.iter()
                .map(|nid| {
                    graph
                        .graph
                        .node_weight(*nid)
                        .map(|n| n.name.clone())
                        .unwrap_or_default()
                })
                .collect::<Vec<String>>()
        };
        let ka = key(a);
        let kb = key(b);
        // v52 T11（reviewer 修正）：名称序列可相同（不同特征含同名实体），
        // 稳定排序会保留 groups 的 HashMap 迭代序（跨运行随机）——追加
        // NodeId 最小元素比较，排序键唯一化，保证特征编号跨运行确定。
        ka.cmp(&kb)
            .then_with(|| a.iter().min().cmp(&b.iter().min()))
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

    /// 测试用 mock 嵌入器：按实体名前缀返回固定向量——
    /// a.rs/b.rs 实体为 [1.0, 0.0]（fa/fb 语义相似），c.rs/d.rs 实体为 [0.0, 1.0]（fc/fd 相似）。
    /// 余弦相似度跨组为 0、组内为 1，使 {fa,fb}、{fc,fd} 各自权重 0.75 ≥ γ=0.4 成簇。
    /// v52 T11：原测试靠 4 个单例社区凑数通过（假绿）；单例过滤后须走 semantic 通道。
    /// 同时覆盖 embedding 并发化路径（texts=4 分 4 块并行）。
    struct MockEmbedder;

    impl Embedder for MockEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(
                if text.starts_with("fn_src_a_rs") || text.starts_with("fn_src_b_rs") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                },
            )
        }
        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            texts.iter().map(|t| self.embed(t)).collect()
        }
        fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
            let dot: f64 = a
                .iter()
                .zip(b)
                .map(|(x, y)| (*x as f64) * (*y as f64))
                .sum();
            let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot / (na * nb)
            }
        }
    }

    /// 构造图：a.rs 的 fa 调用 b.rs 的 fb（跨文件），c.rs 的 fc 调用 d.rs 的 fd
    fn make_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_file = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
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
                visibility: None,
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
                visibility: None,
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
        // fa→fb、fc→fd 两对跨文件调用（semantic 通道由 MockEmbedder 提供，见 test_detect_features_basic）
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
        let features = detect_features(&kg, Some(&MockEmbedder)).unwrap();
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
            visibility: None,
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
            visibility: None,
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
            visibility: None,
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

    /// v52 T11：两特征名称序列相同（同名实体）时，NodeId 最小元素决胜——
    /// 保证特征编号跨运行确定（reviewer MEDIUM 建议的回归锚点）。
    /// 构造：a.rs::util 调 b.rs::helper、c.rs::util 调 d.rs::helper；
    /// MockEmbedder 对两个名字均返回同一向量（不以 fn_src_a_rs 开头 → [0,1]），
    /// 两簇 cosine=1.0 → 权重 0.75 各自成簇；两簇名称序列均为 ["helper","util"]，
    /// 排序键相同 → tie-break 走 min(NodeId)，断言 util_a 特征必在 util_c 特征前。
    #[test]
    fn test_detect_features_same_name_tiebreak() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_entity = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
                          path: &str,
                          name: &str|
         -> NodeId {
            let fid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::File,
                name: path.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: vec!["src".into()],
            });
            let eid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: name.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: Vec::new(),
            });
            g.add_edge(
                fid,
                eid,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: fid,
                    target: eid,
                    weight: 1.0,
                    location: None,
                },
            );
            eid
        };
        let util_a = add_entity(g, "src/a.rs", "util");
        let helper_b = add_entity(g, "src/b.rs", "helper");
        let util_c = add_entity(g, "src/c.rs", "util");
        let helper_d = add_entity(g, "src/d.rs", "helper");
        for (s, t) in [(util_a, helper_b), (util_c, helper_d)] {
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
        let features = detect_features(&kg, Some(&MockEmbedder)).unwrap();
        assert_eq!(features.len(), 2, "应检出 2 个特征: {:?}", features);
        let names_of = |f: &Feature| -> Vec<String> {
            f.node_ids
                .iter()
                .map(|nid| kg.graph.node_weight(*nid).unwrap().name.clone())
                .collect()
        };
        let first_names = names_of(&features[0]);
        let second_names = names_of(&features[1]);
        assert_eq!(
            first_names, second_names,
            "两特征名称序列应相同，以触发 tie-break"
        );
        let first_min = features[0].node_ids.iter().min().copied().unwrap();
        let second_min = features[1].node_ids.iter().min().copied().unwrap();
        assert!(first_min < second_min, "tie-break 应按 NodeId 最小元素排序");
        let features2 = detect_features(&kg, Some(&MockEmbedder)).unwrap();
        let min2: Vec<NodeId> = features2
            .iter()
            .map(|f| f.node_ids.iter().min().copied().unwrap())
            .collect();
        assert_eq!(min2[0], first_min, "两次调用顺序应一致（确定性）");
        assert_eq!(min2[1], second_min);
    }

    /// v52 T11（test_engineer 缺口 (b)）：跨文件调用存在但语义+结构权重均
    /// 不足（Jaccard=0、无 embedding）→ Leiden 全单例 → 单例过滤后 0 特征。
    /// 锚定 feature.rs:230 retain(>=2) 过滤路径（make_graph 的 fa/fb 邻居集
    /// 互斥，权重 0.25 < γ=0.4，与 test_detect_features_basic 构造同图但
    /// 不传 embedder——验证纯结构通道下无特征、不 panic）。
    #[test]
    fn test_detect_features_all_singletons_filtered() {
        let kg = make_graph();
        let features = detect_features(&kg, None).unwrap();
        assert!(
            features.is_empty(),
            "纯结构通道下两对调用应全单例并被过滤: {:?}",
            features
        );
    }

    /// v52 T11（test_engineer 缺口 (a)）：embed_batch 失败 → 整体降级纯结构
    /// （src/analysis/feature.rs:144-155 failed → None），行为等价于 embedder=None。
    /// 注入恒失败 embedder，断言不 panic 且结果与 None 路径一致。
    #[test]
    fn test_detect_features_embedding_failure_degrades() {
        struct FailingEmbedder;
        impl Embedder for FailingEmbedder {
            fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
                Err(anyhow::anyhow!("注入失败：embedding 不可用"))
            }
            fn embed_batch(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
                Err(anyhow::anyhow!("注入失败：embedding 不可用"))
            }
            fn cosine_similarity(&self, _a: &[f32], _b: &[f32]) -> f64 {
                0.0
            }
        }
        let kg = make_graph();
        let degraded = detect_features(&kg, Some(&FailingEmbedder)).unwrap();
        let baseline = detect_features(&kg, None).unwrap();
        assert_eq!(degraded.len(), baseline.len(), "降级结果应与纯结构一致");
        assert!(
            degraded.is_empty(),
            "两对互斥调用降级后应无特征: {:?}",
            degraded
        );
    }
}
