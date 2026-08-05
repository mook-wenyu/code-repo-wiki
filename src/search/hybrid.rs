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
pub fn rrf_merge(results: &[Vec<SearchHit>], top_k: usize, k: f64) -> Vec<SearchHit> {
    use std::collections::HashMap;
    // (名称, 文件路径) → 累计 RRF 分数。去重键必须含 file_path：同名函数
    // 在不同文件（不同模块）是不同实体，仅按名称去重会把它们折叠成一条
    // 结果，语义检索的"定位"价值被破坏（N2 修复）。
    let mut scores: HashMap<(String, String), (f64, CodeNode)> = HashMap::new();
    for list in results {
        for (rank, hit) in list.iter().enumerate() {
            let key = (
                hit.node.name.clone(),
                hit.node.file_path.clone().unwrap_or_default(),
            );
            let entry = scores.entry(key).or_insert_with(|| (0.0, hit.node.clone()));
            // 标准 RRF: k / (k + rank)
            entry.0 += k / (k + rank as f64);
        }
    }
    let mut merged: Vec<SearchHit> = scores.into_iter()
        .map(|(_key, (score, node))| SearchHit {
            node, score, source: "hybrid".into(),
            callers: vec![], callees: vec![],
        })
        .collect();
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(top_k);
    merged
}

/// 将 BM25 结果转为 SearchHit
pub fn text_results_to_hits(results: Vec<(CodeNode, f64)>) -> Vec<SearchHit> {
    results.into_iter().map(|(node, score)| SearchHit {
        node, score, source: "text".into(),
        callers: vec![], callees: vec![],
    }).collect()
}

/// 将语义结果转为 SearchHit
pub fn semantic_results_to_hits(results: Vec<(CodeNode, f32)>) -> Vec<SearchHit> {
    results.into_iter().map(|(node, score)| SearchHit {
        node, score: score as f64, source: "semantic".into(),
        callers: vec![], callees: vec![],
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeKind, NodeId};

    fn make_node(name: &str) -> CodeNode {
        CodeNode {
            id: NodeId::new(0), kind: NodeKind::Function, name: name.into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec![], visibility: None,
        }
    }

    #[test]
    fn test_rrf_merge_single_source() {
        let hits = vec![
            SearchHit { node: make_node("foo"), score: 1.0, source: "text".into(), callers: vec![], callees: vec![] },
            SearchHit { node: make_node("bar"), score: 0.5, source: "text".into(), callers: vec![], callees: vec![] },
        ];
        let result = rrf_merge(&[hits], 5, 60.0);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].node.name, "foo");
    }

    #[test]
    fn test_rrf_merge_dedup() {
        let t = vec![
            SearchHit { node: make_node("foo"), score: 1.0, source: "text".into(), callers: vec![], callees: vec![] },
        ];
        let s = vec![
            SearchHit { node: make_node("foo"), score: 0.9, source: "semantic".into(), callers: vec![], callees: vec![] },
        ];
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
        let text = vec![
            SearchHit { node: a.clone(), score: 1.0, source: "text".into(), callers: vec![], callees: vec![] },
        ];
        let semantic = vec![
            SearchHit { node: b, score: 0.9, source: "semantic".into(), callers: vec![], callees: vec![] },
        ];
        let result = rrf_merge(&[text, semantic], 5, 60.0);
        assert_eq!(result.len(), 2, "同名不同文件的实体不应被折叠");
    }

    /// 双引擎交叉排名：同一文档在两引擎排名不同时，融合排序只由各引擎
    /// 内部排名决定，与引擎先后顺序无关（标准 RRF 性质，防引擎偏移回归）。
    #[test]
    fn test_rrf_merge_cross_engine_ranking() {
        // 文档 a：text 第 1、semantic 第 3；文档 b：text 第 2、semantic 第 1
        let text = vec![
            SearchHit { node: make_node("a"), score: 1.0, source: "text".into(), callers: vec![], callees: vec![] },
            SearchHit { node: make_node("b"), score: 0.9, source: "text".into(), callers: vec![], callees: vec![] },
        ];
        let semantic = vec![
            SearchHit { node: make_node("b"), score: 1.0, source: "semantic".into(), callers: vec![], callees: vec![] },
            SearchHit { node: make_node("x"), score: 0.8, source: "semantic".into(), callers: vec![], callees: vec![] },
            SearchHit { node: make_node("a"), score: 0.6, source: "semantic".into(), callers: vec![], callees: vec![] },
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
