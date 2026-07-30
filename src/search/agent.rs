use crate::model::CodeNode;
use super::text::TextEngine;
use super::semantic::SemanticEngine;
use super::ast::AstQuery;
use super::hybrid::{self, SearchHit, rrf_merge};

/// 搜索 Agent：自动回溯的多轮搜索策略
///
/// 优先执行 FTS5（<50ms）→ 结果不足时自动启动语义搜索（~200ms）
/// → 需要精确定位时 AST 查询（~100ms）。
/// 各级结果经 RRF 合并后返回。
pub struct SearchAgent {
    text: TextEngine,
    semantic: Option<SemanticEngine>,
    rrf_k: f64,
}

impl SearchAgent {
    pub fn new(text: TextEngine, semantic: Option<SemanticEngine>) -> Self {
        Self { text, semantic, rrf_k: 60.0 }
    }

    /// 执行分层搜索。auto_backtrack 控制是否自动回退到语义引擎。
    pub fn search(&self, query: &str, top_k: usize, auto_backtrack: bool) -> Vec<SearchHit> {
        // 第一层：FTS5 全文搜索
        let text_results = match self.text.search(query, top_k) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        if auto_backtrack && text_results.len() < 3 && self.semantic.is_some() {
            let mut all = Vec::new();
            if !text_results.is_empty() {
                all.push(hybrid::text_results_to_hits(text_results));
            }
            if let Some(ref sem) = self.semantic && let Ok(sem_results) = sem.search(query, top_k * 2) {
                all.push(hybrid::semantic_results_to_hits(sem_results));
            }
            return rrf_merge(&all, top_k, self.rrf_k);
        }
        hybrid::text_results_to_hits(text_results)
    }

    /// 使用 AST 做精确定位查询
    pub fn search_ast(&self, source: &str, symbol: &str, language: &str) -> Vec<SearchHit> {
        let mut q = match AstQuery::new(language) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };
        let mut results = Vec::new();
        if let Ok(Some(m)) = q.find_definition(source, symbol) && let Some(text) = m.captures.get("name") {
            results.push(SearchHit {
                node: CodeNode {
                    id: crate::model::NodeId::new(0),
                    kind: crate::model::NodeKind::Function,
                    name: symbol.to_string(),
                    file_path: None,
                    line_range: Some((m.start_line, m.end_line)),
                    doc_comment: None,
                    signature: Some(text.clone()),
                    module_path: vec![],
                },
                score: 100.0,
                source: "ast".into(),
            });
        }
        results
    }

    pub fn text_engine(&self) -> &TextEngine { &self.text }

    pub fn set_rrf_k(&mut self, k: f64) {
        self.rrf_k = k;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeKind, NodeId};

    use std::sync::atomic::{AtomicU64, Ordering};
    static AGENT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_db_path(prefix: &str) -> std::path::PathBuf {
        let id = AGENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("{}_{}_{}.db", prefix, std::process::id(), id));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn make_text_engine() -> TextEngine {
        let path = unique_db_path("agent_text");
        let mut t = TextEngine::open(&path).unwrap();
        let _ = t.index(&CodeNode {
            id: NodeId::new(0), kind: NodeKind::Function,
            name: "add_user".into(), file_path: None, line_range: None,
            doc_comment: None, signature: Some("fn add_user(name: &str)".into()),
            module_path: vec![],
        }, "fn add_user(name: &str)");
        let _ = t.index(&CodeNode {
            id: NodeId::new(1), kind: NodeKind::Function,
            name: "delete_user".into(), file_path: None, line_range: None,
            doc_comment: None, signature: None, module_path: vec![],
        }, "");
        t
    }

    fn make_text_empty() -> TextEngine {
        let path = unique_db_path("agent_empty");
        TextEngine::open(&path).unwrap()
    }

    #[test]
    fn test_agent_text_search() {
        let agent = SearchAgent::new(make_text_engine(), None);
        let results = agent.search("add", 5, false);
        assert!(!results.is_empty());
        assert!(results[0].node.name.contains("add"));
    }

    #[test]
    fn test_agent_ast_search() {
        let agent = SearchAgent::new(make_text_empty(), None);
        let results = agent.search_ast("fn test_fn() {}", "test_fn", "rust");
        assert!(!results.is_empty());
        assert_eq!(results[0].source, "ast");
    }

    #[test]
    fn test_agent_empty_text() {
        let agent = SearchAgent::new(make_text_empty(), None);
        let results = agent.search("anything", 5, false);
        assert!(results.is_empty());
    }

    #[test]
    fn test_agent_auto_backtrack_no_semantic() {
        let agent = SearchAgent::new(make_text_engine(), None);
        let results = agent.search("zzzz_not_found", 5, true);
        assert!(results.is_empty());
    }
}