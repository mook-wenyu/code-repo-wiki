use std::collections::HashMap;

use super::text::TextEngine;
use super::semantic::SemanticSearch;
use super::hybrid::{self, SearchHit, rrf_merge};

/// 调用链索引：符号名 → (调用者列表, 被调用者列表)
type CallIndex = HashMap<String, (Vec<String>, Vec<String>)>;

/// 搜索 Agent：自动回溯的多轮搜索策略
///
/// 优先执行 FTS5（<50ms）→ 结果不足时自动启动语义搜索（~200ms）
/// → 需要精确定位时 AST 查询（~100ms）。
/// 各级结果经 RRF 合并后返回；若提供了调用链索引，还会对命中结果
/// 做调用者/被调用者补全。
pub struct SearchAgent {
    text: TextEngine,
    /// 语义引擎抽象（v6：trait object 可注入 mock，语义分支可测试）
    semantic: Option<Box<dyn SemanticSearch>>,
    rrf_k: f64,
    /// 符号名 → (调用者列表, 被调用者列表) 预计算表，None 表示不做调用链补全
    call_index: Option<CallIndex>,
}

impl SearchAgent {
    pub fn new(text: TextEngine, semantic: Option<Box<dyn SemanticSearch>>, rrf_k: f64) -> Self {
        Self { text, semantic, rrf_k, call_index: None }
    }

    /// 注入调用链索引（由 CallGraph::build_call_index 预计算），启用调用链补全
    pub fn with_call_index(mut self, index: CallIndex) -> Self {
        self.call_index = Some(index);
        self
    }

    /// 执行分层搜索。auto_backtrack 控制是否自动回退到语义引擎。
    pub fn search(&self, query: &str, top_k: usize, auto_backtrack: bool) -> Vec<SearchHit> {
        // 第一层：FTS5 全文搜索
        let text_results = match self.text.search(query, top_k) {
            Ok(r) => r,
            // U04/P3：text 索引损坏/不可读时不再静默返回空（此前空结果
            // 被当作"无命中"，掩盖索引损坏事实）——告警后仍返回空，
            // 调用方按无命中处理，但日志暴露真实原因。
            Err(e) => {
                tracing::warn!("text 索引搜索失败（按无命中处理）: {e}");
                return Vec::new();
            }
        };
        let mut hits = if auto_backtrack && text_results.len() < 3 && self.semantic.is_some() {
            let mut all = Vec::new();
            if !text_results.is_empty() {
                all.push(hybrid::text_results_to_hits(text_results));
            }
            if let Some(ref sem) = self.semantic {
                // U04/P3：语义搜索失败同样告警（此前 if let Ok 吞掉 Err，
                // 语义引擎故障时静默退化为纯 text 结果，无任何日志）。
                match sem.search(query, top_k * 2) {
                    Ok(sem_results) => {
                        all.push(hybrid::semantic_results_to_hits(sem_results));
                    }
                    Err(e) => {
                        tracing::warn!("语义搜索失败（跳过语义回溯）: {e}");
                    }
                }
            }
            rrf_merge(&all, top_k, self.rrf_k)
        } else {
            hybrid::text_results_to_hits(text_results)
        };
        // 调用链补全：对命中结果填充调用者/被调用者
        self.enrich_call_chain(&mut hits);
        hits
    }

    /// 调用链补全：按命中符号名查 call_index，填充 callers/callees；无索引时直接跳过
    fn enrich_call_chain(&self, hits: &mut [SearchHit]) {
        let Some(index) = &self.call_index else { return };
        for hit in hits.iter_mut() {
            if let Some((callers, callees)) = index.get(&hit.node.name) {
                hit.callers = callers.clone();
                hit.callees = callees.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeNode, NodeKind, NodeId};

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

    /// 可编程 mock 语义引擎（v6：SemanticSearch trait 抽象使语义分支可测试）
    ///
    /// 固定返回预置结果；无需真实 embedding 与向量库。
    struct MockSemantic {
        results: Vec<(CodeNode, f32)>,
    }

    impl SemanticSearch for MockSemantic {
        fn index(&mut self, _node: &CodeNode, _source_code: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn index_batch(&mut self, _items: &[(CodeNode, String)]) -> anyhow::Result<()> {
            Ok(())
        }
        fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<(CodeNode, f32)>> {
            Ok(self.results.clone())
        }
        fn remove_by_file(&mut self, _file_path: &str) -> anyhow::Result<usize> {
            Ok(0)
        }
        fn clear(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn entry_count(&self) -> usize {
            self.results.len()
        }
    }

    fn mock_node(name: &str) -> CodeNode {
        CodeNode {
            id: NodeId::new(0), kind: NodeKind::Function, name: name.into(),
            file_path: Some(format!("src/{name}.rs")), line_range: None,
            doc_comment: None, signature: None, module_path: vec![],
        }
    }

    /// 语义回溯分支（v5 报告 P1 缺口补测）：FTS 结果不足 3 条且语义引擎在场时，
    /// 语义结果经 RRF 合并后返回；无语义引擎时仅返回 FTS 结果。
    #[test]
    fn test_agent_auto_backtrack_with_semantic() {
        // 文本引擎空（无 FTS 命中）→ 触发回溯，语义结果应进入最终结果
        let text = make_text_empty();
        let semantic = Box::new(MockSemantic {
            results: vec![(mock_node("sem_hit"), 0.95)],
        });
        let agent = SearchAgent::new(text, Some(semantic), 60.0);
        let results = agent.search("zzz_not_in_fts", 5, true);
        assert_eq!(results.len(), 1, "语义命中应经回溯进入结果");
        assert_eq!(results[0].node.name, "sem_hit");
    }

    /// auto_backtrack=false 时不触发语义回溯（即使语义引擎在场）
    #[test]
    fn test_agent_no_backtrack_when_disabled() {
        let text = make_text_empty();
        let semantic = Box::new(MockSemantic {
            results: vec![(mock_node("sem_hit"), 0.95)],
        });
        let agent = SearchAgent::new(text, Some(semantic), 60.0);
        let results = agent.search("zzz_not_in_fts", 5, false);
        assert!(results.is_empty(), "回溯关闭时不应使用语义结果");
    }

    /// FTS 命中足够（≥3 条）时不触发语义回溯（分层搜索的成本控制语义）
    #[test]
    fn test_agent_skips_semantic_when_text_sufficient() {
        // 构造 3 条使 FTS 命中 ≥3，验证不触发回溯
        let path = unique_db_path("agent_text3");
        let mut t = TextEngine::open(&path).unwrap();
        let _ = t.index(&mock_node("add_user"), "fn add_user(name: &str)");
        let _ = t.index(&mock_node("delete_user"), "fn delete_user(id: u64)");
        let _ = t.index(&mock_node("update_user"), "fn update_user(id: u64)");
        let semantic = Box::new(MockSemantic {
            results: vec![(mock_node("sem_hit"), 0.95)],
        });
        let agent = SearchAgent::new(t, Some(semantic), 60.0);
        // 查询 "user"：FTS 命中 3 条 → 不回溯，语义结果不应混入
        let results = agent.search("user", 5, true);
        assert!(
            results.iter().all(|h| h.node.name != "sem_hit"),
            "FTS 足够时不应触发语义回溯: {:?}",
            results.iter().map(|h| h.node.name.clone()).collect::<Vec<_>>()
        );
        assert!(results.len() >= 3, "FTS 应有 3 条命中: {:?}", results.len());
    }

    #[test]
    fn test_agent_text_search() {
        let agent = SearchAgent::new(make_text_engine(), None, 60.0);
        let results = agent.search("add", 5, false);
        assert!(!results.is_empty());
        assert!(results[0].node.name.contains("add"));
    }

    #[test]
    fn test_agent_empty_text() {
        let agent = SearchAgent::new(make_text_empty(), None, 60.0);
        let results = agent.search("anything", 5, false);
        assert!(results.is_empty());
    }

    #[test]
    fn test_agent_auto_backtrack_no_semantic() {
        let agent = SearchAgent::new(make_text_engine(), None, 60.0);
        let results = agent.search("zzzz_not_found", 5, true);
        assert!(results.is_empty());
    }

    /// 构造 a→b→c 迷你调用图，验证调用链补全：查 b 得调用者 a、被调用者 c
    #[test]
    fn test_callgraph_enrichment() {
        use crate::model::{CodeEdge, EdgeKind, KnowledgeGraph};
        use crate::search::callgraph::CallGraph;
        use petgraph::stable_graph::StableDiGraph;

        let make_node = |id: u64, name: &str| CodeNode {
            id: NodeId::new(id as usize), kind: NodeKind::Function, name: name.into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec!["test".into()],
        };
        let make_edge = |source: _, target: _| CodeEdge {
            id: petgraph::stable_graph::EdgeIndex::new(0),
            kind: EdgeKind::Calls, source, target,
            weight: 1.0, location: None,
        };

        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let a = g.add_node(make_node(0, "a"));
        let b = g.add_node(make_node(1, "b"));
        let c = g.add_node(make_node(2, "c"));
        g.add_edge(a, b, make_edge(a, b));
        g.add_edge(b, c, make_edge(b, c));

        let kg = KnowledgeGraph { graph: g, modules: vec![], features: Vec::new() };
        let index = CallGraph::new(&kg).build_call_index();

        let mut t = TextEngine::open(unique_db_path("agent_callgraph")).unwrap();
        let _ = t.index(&make_node(1, "b"), "fn b()");

        let agent = SearchAgent::new(t, None, 60.0).with_call_index(index);
        let results = agent.search("b", 5, false);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.name, "b");
        assert!(results[0].callers.iter().any(|c| c == "a"));
        assert!(results[0].callees.iter().any(|c| c == "c"));
    }

    /// 无 call_index 时搜索行为不变，不报错
    #[test]
    fn test_search_without_call_index() {
        let agent = SearchAgent::new(make_text_engine(), None, 60.0);
        let results = agent.search("add", 5, false);
        assert!(!results.is_empty());
        assert!(results[0].callers.is_empty());
        assert!(results[0].callees.is_empty());
    }
}