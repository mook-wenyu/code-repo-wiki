//! BM25 全文搜索引擎——SQLite FTS5 持久化
//!
//! 通过 SQLite FTS5 虚拟表实现全文搜索，BM25 排序由 SQLite 内置完成。
//! 支持并发读取（WAL 模式），写操作自动排队。

use anyhow::Result;
use std::path::Path;

use super::store::SearchStore;
use crate::model::CodeNode;

/// BM25 全文搜索引擎
///
/// 内部委托 SearchStore（SQLite FTS5）完成索引和搜索。
/// 公开 API 保持不变，供 pipeline 和 CLI 调用。
pub struct TextEngine {
    store: SearchStore,
}

impl TextEngine {
    /// 打开或创建持久化搜索引擎。
    ///
    /// path 指向 SQLite 数据库文件（.db），不存在时自动创建。
    /// 返回 (engine, need_reindex)：need_reindex=true 表示旧 schema 已
    /// 迁移重建（索引为空），调用方必须全量重索引（增量路径只补
    /// changed_files 会丢失旧实体）。
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, bool)> {
        let (store, need_reindex) = SearchStore::open(path)?;
        Ok((Self { store }, need_reindex))
    }

    /// 索引一个 CodeNode。
    pub fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        self.store
            .insert_entities_batch(&[(node.clone(), source_code.to_string())])
    }

    /// 批量索引多个实体。
    pub fn index_batch(&mut self, items: &[(CodeNode, String)]) -> Result<()> {
        self.store.insert_entities_batch(items)
    }

    /// BM25 搜索，返回 (CodeNode, score) 按相关性降序。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f64)>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.store.search_fts(query, limit)
    }

    /// 删除指定文件路径关联的所有索引条目。
    pub fn remove_by_file(&mut self, file_path: &str) -> Result<usize> {
        self.store.delete_entities_by_file(file_path)
    }

    /// 清空索引。
    pub fn clear(&mut self) -> Result<()> {
        self.store.clear_entities()
    }

    /// 当前索引中的文档数。
    pub fn doc_count(&self) -> usize {
        self.store.entity_count().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeId, NodeKind};

    fn make_node(name: &str, kind: NodeKind) -> CodeNode {
        CodeNode {
            id: NodeId::new(0),
            kind,
            name: name.into(),
            file_path: Some("src/test.rs".into()),
            line_range: Some((1, 5)),
            doc_comment: None,
            signature: Some(format!("fn {}()", name)),
            visibility: None,
            module_path: vec![],
        }
    }

    fn tmp_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "text_fts_{}_{}.db",
            label,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_index_and_search() -> Result<()> {
        let (mut engine, _) = TextEngine::open(tmp_path("index_search"))?;
        engine.index(
            &make_node("add_user", NodeKind::Function),
            "fn add_user(name: &str)",
        )?;
        engine.index(
            &make_node("delete_user", NodeKind::Function),
            "fn delete_user(id: u64)",
        )?;
        let results = engine.search("add_user", 10)?;
        assert!(!results.is_empty());
        assert!(results[0].0.name.contains("add_user"));
        Ok(())
    }

    #[test]
    fn test_empty_engine() -> Result<()> {
        let (engine, _) = TextEngine::open(tmp_path("empty"))?;
        assert!(engine.search("anything", 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn test_persistence() -> Result<()> {
        let path = tmp_path("persist");
        {
            let (mut engine, _) = TextEngine::open(&path)?;
            engine.index(&make_node("persist_test", NodeKind::Function), "fn test()")?;
        }
        let (engine, _) = TextEngine::open(&path)?;
        assert_eq!(engine.doc_count(), 1);
        let results = engine.search("persist_test", 10)?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[test]
    fn test_clear() -> Result<()> {
        let (mut engine, _) = TextEngine::open(tmp_path("clear"))?;
        engine.index(&make_node("x", NodeKind::Function), "")?;
        assert_eq!(engine.doc_count(), 1);
        engine.clear()?;
        assert_eq!(engine.doc_count(), 0);
        Ok(())
    }

    #[test]
    fn test_remove_by_file() -> Result<()> {
        let (mut engine, _) = TextEngine::open(tmp_path("remove"))?;
        let node_a = CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "alpha_unique".into(),
            file_path: Some("src/alpha.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None,
            module_path: vec![],
            visibility: None,
        };
        let node_b = CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "beta_unique".into(),
            file_path: Some("src/beta.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None,
            module_path: vec![],
            visibility: None,
        };
        engine.index_batch(&[(node_a, "alpha".into()), (node_b, "beta".into())])?;
        assert_eq!(engine.doc_count(), 2);

        let removed = engine.remove_by_file("src/alpha.rs")?;
        assert_eq!(removed, 1);
        assert_eq!(engine.doc_count(), 1);
        Ok(())
    }

    /// t12：短关键词基线——BM25 token 精确匹配对短查询可用
    /// （CoREB 论文的短查询退化是 embedding 检索问题，FTS5 不受影响；
    /// 这同时是"不引入 reranker"决策的本地证据之一）
    #[test]
    fn test_short_keyword_baseline() -> Result<()> {
        let (mut engine, _) = TextEngine::open(tmp_path("short_keyword"))?;
        engine.index(
            &make_node("a_helper", NodeKind::Function),
            "fn a_helper(x: u32)",
        )?;
        engine.index(
            &make_node("udp_send", NodeKind::Function),
            "fn udp_send(sock: u32)",
        )?;
        // 1 字符 token 查询：BM25 token 精确匹配，含单字符 token 的实体命中
        let short = engine.search("a", 10)?;
        assert!(
            short.iter().any(|(n, _)| n.name == "a_helper"),
            "1 字符 token 查询应命中 a_helper"
        );
        // 2 字符 token 精确查询
        let two = engine.search("udp", 10)?;
        assert!(
            two.iter().any(|(n, _)| n.name == "udp_send"),
            "2 字符 token 应命中"
        );
        Ok(())
    }
}
