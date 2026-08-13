use crate::model::CodeNode;
use serde::Serialize;

/// 搜索结果条目
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub node: CodeNode,
    pub score: f64,
    /// 来源引擎: "text", "semantic", "hybrid"
    pub source: String,
    /// 调用者名称列表（调用链补全填充，默认空）
    #[serde(default)]
    pub callers: Vec<String>,
    /// 被调用者名称列表（调用链补全填充，默认空）
    #[serde(default)]
    pub callees: Vec<String>,
}

/// RRF 混合排序：将多引擎结果按 Reciprocal Rank Fusion 合并
///
/// 标准 RRF：score = Σ k/(k+rank)，rank 为各引擎内部排名（0 起）。
/// 注意：此前实现混入"引擎序号×10"的偏移（rank + engine_rank*10），
/// 导致同一文档在不同引擎的排名权重被引擎顺序污染——已修正为标准实现。
///
/// k 语义（P2-7）：k 越小排名头部权重越陡（强命中更突出），越大越平滑
/// （共识投票化）。SIGIR'09 原文 k=60 面向多路融合；两路（text+semantic）
/// 融合 2025 共识建议 20-40——默认值见 config::schema::SEARCH_RRF_K。
/// 候选深度与最终 top_k 分离：RRF 接收各引擎已截断的 top 候选（调用方
/// 传入），融合后按 top_k 截断。
pub fn rrf_merge(results: &[Vec<SearchHit>], top_k: usize, k: f64) -> Vec<SearchHit> {
    use std::collections::HashMap;
    // (名称, 文件路径) → 累计 RRF 分数。去重键必须含 file_path：同名函数
    // 在不同文件（不同模块）是不同实体，仅按名称去重会把它们折叠成一条
    // 结果，语义检索的"定位"价值被破坏（N2 修复）。
    let mut scores: HashMap<(String, String, usize), (f64, CodeNode)> = HashMap::new();
    for list in results {
        for (rank, hit) in list.iter().enumerate() {
            let key = (
                hit.node.name.clone(),
                hit.node.file_path.clone().unwrap_or_default(),
                // P2-2：同文件同名实体（重载/多 impl 方法）按行范围区分——
                // 仅 (name, file_path) 会折叠重载为一条结果，语义检索的
                // "定位到具体定义"价值被破坏
                hit.node.line_range.map(|(s, _)| s).unwrap_or(0),
            );
            let entry = scores.entry(key).or_insert_with(|| (0.0, hit.node.clone()));
            // 标准 RRF: k / (k + rank)
            entry.0 += k / (k + rank as f64);
        }
    }
    let mut merged: Vec<SearchHit> = scores
        .into_iter()
        .map(|(_key, (score, node))| SearchHit {
            node,
            score,
            source: "hybrid".into(),
            callers: vec![],
            callees: vec![],
        })
        .collect();
    // P2-1：同分条目按 NodeId 稳定排序（tie-break 唯一且跨运行确定）——
    // 否则同分顺序由 HashMap 迭代序决定，同一查询两次结果顺序漂移
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.node.id.index().cmp(&b.node.id.index()))
    });
    merged.truncate(top_k);
    merged
}

/// 将 BM25 结果转为 SearchHit
pub fn text_results_to_hits(results: Vec<(CodeNode, f64)>) -> Vec<SearchHit> {
    results
        .into_iter()
        .map(|(node, score)| SearchHit {
            node,
            score,
            source: "text".into(),
            callers: vec![],
            callees: vec![],
        })
        .collect()
}

/// 将语义结果转为 SearchHit
pub fn semantic_results_to_hits(results: Vec<(CodeNode, f32)>) -> Vec<SearchHit> {
    results
        .into_iter()
        .map(|(node, score)| SearchHit {
            node,
            score: score as f64,
            source: "semantic".into(),
            callers: vec![],
            callees: vec![],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeId, NodeKind};
    use petgraph::stable_graph::NodeIndex;

    fn make_node(name: &str) -> CodeNode {
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: name.into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None,
            module_path: vec![],
            visibility: None,
        }
    }

    #[test]
    fn test_rrf_merge_single_source() {
        let hits = vec![
            SearchHit {
                node: make_node("foo"),
                score: 1.0,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            },
            SearchHit {
                node: make_node("bar"),
                score: 0.5,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            },
        ];
        let result = rrf_merge(&[hits], 5, 60.0);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].node.name, "foo");
    }

    #[test]
    fn test_rrf_merge_dedup() {
        let t = vec![SearchHit {
            node: make_node("foo"),
            score: 1.0,
            source: "text".into(),
            callers: vec![],
            callees: vec![],
        }];
        let s = vec![SearchHit {
            node: make_node("foo"),
            score: 0.9,
            source: "semantic".into(),
            callers: vec![],
            callees: vec![],
        }];
        let result = rrf_merge(&[t, s], 5, 60.0);
        assert_eq!(result.len(), 1);
    }

    /// 同名不同文件是不同实体：去重键含 file_path，两引擎命中不同文件的
    /// 同名函数必须保留两条结果（N2 防回归）。
    #[test]
    fn test_rrf_merge_keeps_same_name_different_file() {
        let mut a = make_node("foo");
        a.file_path = Some("src/a.rs".into());
        let mut b = make_node("foo");
        b.file_path = Some("src/b.rs".into());
        let text = vec![SearchHit {
            node: a.clone(),
            score: 1.0,
            source: "text".into(),
            callers: vec![],
            callees: vec![],
        }];
        let semantic = vec![SearchHit {
            node: b,
            score: 0.9,
            source: "semantic".into(),
            callers: vec![],
            callees: vec![],
        }];
        let result = rrf_merge(&[text, semantic], 5, 60.0);
        assert_eq!(result.len(), 2, "同名不同文件的实体不应被折叠");
    }

    /// P2-2：同文件同名实体（重载/多 impl 方法）按行范围区分——
    /// 两引擎命中同文件同名的不同定义必须保留两条结果
    #[test]
    fn test_rrf_merge_keeps_same_file_same_name_different_line() {
        let mut a = make_node("foo");
        a.file_path = Some("src/a.rs".into());
        a.line_range = Some((10, 30));
        let mut b = make_node("foo");
        b.file_path = Some("src/a.rs".into());
        b.line_range = Some((40, 60));
        let text = vec![SearchHit {
            node: a.clone(),
            score: 1.0,
            source: "text".into(),
            callers: vec![],
            callees: vec![],
        }];
        let semantic = vec![SearchHit {
            node: b,
            score: 0.9,
            source: "semantic".into(),
            callers: vec![],
            callees: vec![],
        }];
        let result = rrf_merge(&[text, semantic], 5, 40.0);
        assert_eq!(
            result.len(),
            2,
            "同文件同名的重载定义不应被折叠: {:?}",
            result
        );
    }

    /// P2-1：同分条目按 NodeId 稳定排序——两次调用顺序一致
    #[test]
    fn test_rrf_merge_tie_break_deterministic() {
        let mut a = make_node("a");
        a.id = NodeIndex::new(3);
        let mut b = make_node("b");
        b.id = NodeIndex::new(1);
        let mut c = make_node("c");
        c.id = NodeIndex::new(2);
        // 三文档各占独立引擎列表（各自 rank 0）→ RRF 分数相同（40/(40+0)=1.0），
        // 构成真正的同分场景——排序由 tie-break（NodeId 升序）决定
        let hits: Vec<Vec<SearchHit>> = vec![
            vec![SearchHit {
                node: a,
                score: 1.0,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            }],
            vec![SearchHit {
                node: b,
                score: 1.0,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            }],
            vec![SearchHit {
                node: c,
                score: 1.0,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            }],
        ];
        let r1 = rrf_merge(&hits, 5, 40.0);
        let r2 = rrf_merge(&hits.clone(), 5, 40.0);
        let names1: Vec<&str> = r1.iter().map(|h| h.node.name.as_str()).collect();
        let names2: Vec<&str> = r2.iter().map(|h| h.node.name.as_str()).collect();
        assert_eq!(names1, names2, "同分条目顺序必须跨运行稳定");
        // NodeId 升序：b(1) c(2) a(3)
        assert_eq!(names1, vec!["b", "c", "a"], "tie-break 应按 NodeId 升序");
    }

    /// 双引擎交叉排名：同一文档在两引擎排名不同时，融合排序只由各引擎
    /// 内部排名决定，与引擎先后顺序无关（标准 RRF 性质，防引擎偏移回归）。
    #[test]
    fn test_rrf_merge_cross_engine_ranking() {
        // 文档 a：text 第 1、semantic 第 3；文档 b：text 第 2、semantic 第 1
        let text = vec![
            SearchHit {
                node: make_node("a"),
                score: 1.0,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            },
            SearchHit {
                node: make_node("b"),
                score: 0.9,
                source: "text".into(),
                callers: vec![],
                callees: vec![],
            },
        ];
        let semantic = vec![
            SearchHit {
                node: make_node("b"),
                score: 1.0,
                source: "semantic".into(),
                callers: vec![],
                callees: vec![],
            },
            SearchHit {
                node: make_node("x"),
                score: 0.8,
                source: "semantic".into(),
                callers: vec![],
                callees: vec![],
            },
            SearchHit {
                node: make_node("a"),
                score: 0.6,
                source: "semantic".into(),
                callers: vec![],
                callees: vec![],
            },
        ];
        // 调换引擎顺序，融合分数必须完全一致
        let f1 = rrf_merge(&[text.clone(), semantic.clone()], 5, 60.0);
        let f2 = rrf_merge(&[semantic, text], 5, 60.0);
        assert_eq!(f1.len(), f2.len());
        for (h1, h2) in f1.iter().zip(f2.iter()) {
            assert_eq!(h1.node.name, h2.node.name);
            assert!((h1.score - h2.score).abs() < 1e-9);
        }
        // b 在双引擎均靠前（1+1 排名），应排在 a（1+3）之前
        assert_eq!(f1[0].node.name, "b");
    }

    /// P2-7：k 越小排名头部权重越陡（强命中更突出）——
    /// rank0 与 rank1 的分数差在 k=20 时大于 k=60 时（SEARCH_RRF_K 取
    /// 20 的依据，防回归到 60 的共识投票平滑化）
    #[test]
    fn test_rrf_merge_k_sensitivity() {
        let make_list = || {
            vec![
                SearchHit {
                    node: make_node("a"),
                    score: 1.0,
                    source: "text".into(),
                    callers: vec![],
                    callees: vec![],
                },
                SearchHit {
                    node: make_node("b"),
                    score: 0.9,
                    source: "text".into(),
                    callers: vec![],
                    callees: vec![],
                },
            ]
        };
        let r20 = rrf_merge(&[make_list()], 5, 20.0);
        let r60 = rrf_merge(&[make_list()], 5, 60.0);
        assert_eq!(r20[0].node.name, "a");
        assert_eq!(r60[0].node.name, "a");
        let gap20 = r20[0].score - r20[1].score;
        let gap60 = r60[0].score - r60[1].score;
        assert!(
            gap20 > gap60,
            "k=20 头部权重应更陡: gap20={gap20}, gap60={gap60}"
        );
    }

    #[test]
    fn test_text_results_to_hits() {
        let r = vec![(make_node("x"), 2.0)];
        let hits = text_results_to_hits(r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "text");
    }

    #[test]
    fn test_semantic_results_to_hits() {
        let r = vec![(make_node("x"), 0.9)];
        let hits = semantic_results_to_hits(r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "semantic");
    }
}
