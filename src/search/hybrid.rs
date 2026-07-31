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
pub fn rrf_merge(results: &[Vec<SearchHit>], top_k: usize, k: f64) -> Vec<SearchHit> {
    use std::collections::HashMap;
    // 名称 → 累计 RRF 分数
    let mut scores: HashMap<String, (f64, CodeNode)> = HashMap::new();
    for (engine_rank, list) in results.iter().enumerate() {
        for (rank, hit) in list.iter().enumerate() {
            let entry = scores.entry(hit.node.name.clone()).or_insert_with(|| (0.0, hit.node.clone()));
            // RRF: k / (k + rank)
            entry.0 += k / (k + (rank + engine_rank * 10) as f64);
        }
    }
    let mut merged: Vec<SearchHit> = scores.into_iter()
        .map(|(_name, (score, node))| SearchHit {
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
            signature: None, module_path: vec![],
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
