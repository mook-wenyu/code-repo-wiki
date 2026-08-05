//! SQLite 存储层：为搜索引擎提供 FTS5 全文索引
//!
//! 设计要点：
//! - WAL 模式：支持多读单写并发
//! - busy_timeout 5s：写锁等待而非立即失败
//! - FTS5 虚拟表：BM25 排序由 SQLite 内置完成
//!
//! 向量持久化已迁出（v6）：语义检索改用 sqlite-vec 扩展
//! （src/search/vecdb.rs），本文件只承载 FTS5 全文搜索。

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::model::CodeNode;

/// SQLite 搜索引擎存储
///
/// 管理一个 SQLite 数据库文件，包含：
/// - `entities` FTS5 虚拟表（全文搜索）
pub struct SearchStore {
    conn: Connection,
}

impl SearchStore {
    /// 打开或创建数据库，初始化表结构。
    ///
    /// 设置 WAL 模式和 busy_timeout 以支持并发读取。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .context("打开 SQLite 数据库失败")?;

        // WAL 模式：允许并发读，写操作排队
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("设置 WAL 模式失败")?;
        // 写锁等待 5 秒
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("设置 busy_timeout 失败")?;

        // 创建 FTS5 全文搜索虚拟表
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS entities USING fts5(
                name,
                kind,
                signature,
                source,
                file_path,
                node_json
            );"
        ).context("创建 FTS5 表失败")?;

        Ok(Self { conn })
    }

    // ==================== FTS5 全文搜索 ====================

    /// 批量插入实体到 FTS5 表
    pub fn insert_entities_batch(&self, items: &[(CodeNode, String)]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO entities (name, kind, signature, source, file_path, node_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ).context("准备 FTS5 插入语句失败")?;

        for (node, source) in items {
            let node_json = serde_json::to_string(node)
                .context("序列化 CodeNode 失败")?;
            stmt.execute(rusqlite::params![
                node.name,
                node.kind.as_str(),
                node.signature.as_deref().unwrap_or(""),
                source,
                // file_path 列写入时统一归一化（票 08）：该列是删除/过滤的
                // 索引键，全链路（写入、删除、比较）必须同基准。node_json
                // 里的 file_path 保留平台原样（用于搜索结果展示与节点信息）。
                crate::incremental::norm_sep(node.file_path.as_deref().unwrap_or("")).as_str(),
                node_json,
            ]).context("插入 FTS5 条目失败")?;
        }
        Ok(())
    }

    /// FTS5 全文搜索，按 BM25 相关性排序
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f64)>> {
        // FTS5 MATCH 查询，bm25() 返回负数（越小越相关）
        let sql = format!(
            "SELECT node_json, bm25(entities) as rank
             FROM entities
             WHERE entities MATCH ?1
             ORDER BY rank
             LIMIT {}",
            limit
        );
        let mut stmt = self.conn.prepare(&sql)
            .context("准备 FTS5 查询语句失败")?;

        let rows = stmt.query_map(rusqlite::params![query], |row| {
            let node_json: String = row.get(0)?;
            let rank: f64 = row.get(1)?;
            Ok((node_json, rank))
        }).context("执行 FTS5 查询失败")?;

        let mut results = Vec::new();
        for row in rows {
            let (node_json, rank) = row.context("读取 FTS5 结果行失败")?;
            if let Ok(node) = serde_json::from_str::<CodeNode>(&node_json) {
                // bm25() 返回负数，取反作为正分数
                results.push((node, -rank));
            }
        }
        Ok(results)
    }

    /// 删除指定文件的所有 FTS5 条目
    ///
    /// 参数与 file_path 列同基准：写入时已归一化（票 08），删除键也归一化，
    /// 保证 Windows 反斜杠路径（调用方传入）与入库正斜杠键精确匹配。
    pub fn delete_entities_by_file(&self, file_path: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM entities WHERE file_path = ?1",
            rusqlite::params![crate::incremental::norm_sep(file_path)],
        ).context("删除 FTS5 条目失败")?;
        Ok(count)
    }

    /// 获取 FTS5 表中的文档总数
    pub fn entity_count(&self) -> Result<usize> {
        let count: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM entities",
            [],
            |row| row.get(0),
        ).context("查询 FTS5 文档数失败")?;
        Ok(count)
    }

    /// 清空 FTS5 表
    pub fn clear_entities(&self) -> Result<()> {
        self.conn.execute("DELETE FROM entities", [])
            .context("清空 FTS5 表失败")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeId, NodeKind};

    fn tmp_db_path(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("store_test_{}_{}.db", label, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn make_node(name: &str, file: &str) -> CodeNode {
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: name.into(),
            file_path: Some(file.into()),
            line_range: Some((1, 10)),
            doc_comment: None,
            signature: Some(format!("fn {}()", name)), visibility: None,
            module_path: vec![],
        }
    }

    #[test]
    fn test_fts_insert_and_search() {
        let store = SearchStore::open(tmp_db_path("fts")).unwrap();
        let items = vec![
            (make_node("authenticate", "src/auth.rs"), "fn authenticate(user: &str)".to_string()),
            (make_node("save_session", "src/storage.rs"), "fn save_session(id: u64)".to_string()),
        ];
        store.insert_entities_batch(&items).unwrap();
        assert_eq!(store.entity_count().unwrap(), 2);

        let results = store.search_fts("authenticate", 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0.name, "authenticate");
    }

    #[test]
    fn test_fts_delete_by_file() {
        let store = SearchStore::open(tmp_db_path("fts_del")).unwrap();
        let items = vec![
            (make_node("alpha", "src/a.rs"), "alpha code".to_string()),
            (make_node("beta", "src/b.rs"), "beta code".to_string()),
        ];
        store.insert_entities_batch(&items).unwrap();

        let removed = store.delete_entities_by_file("src/a.rs").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.entity_count().unwrap(), 1);
    }
}
