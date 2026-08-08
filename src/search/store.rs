//! SQLite 存储层：为搜索引擎提供 FTS5 全文索引
//!
//! 设计要点：
//! - WAL 模式：支持多读单写并发
//! - busy_timeout 5s：写锁等待而非立即失败
//! - FTS5 虚拟表：BM25 排序由 SQLite 内置完成
//!
//! 向量持久化已迁出（v6）：语义检索改用 sqlite-vec 扩展
//! （src/search/vecdb.rs），本文件只承载 FTS5 全文搜索。
//!
//! v36 schema v2：新增 `tokens` 列承载 CJK 2-gram（见 tokenize.rs）。
//! FTS5 默认分词器 unicode61 把连续汉字当作单一 token（无词边界），
//! 中文子串检索必然零命中；tokens 列写入 CJK 2-gram 后查询侧做同构
//! 展开即可恢复中文命中。旧库（无 tokens 列）用 PRAGMA user_version
//! 检测并在 open 时 DROP 重建（返回 need_reindex 由调用方回退全量
//! 文本重索引——见 lib.rs update_search_index_incremental）。

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::model::CodeNode;
use crate::search::tokenize::extract_keywords;

/// FTS5 建表 SQL（schema v2：末列 tokens 存 CJK 2-gram）
///
/// user_version 约定：0/1 = 旧 schema（无 tokens 列，需重建）；
/// 2 = 当前 schema。
const CREATE_ENTITIES_V2: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS entities USING fts5(
    name,
    kind,
    signature,
    source,
    file_path,
    node_json,
    tokens
);";

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
    /// 返回 (store, need_reindex)：need_reindex=true 表示旧 schema 已
    /// 迁移重建（表为空），调用方必须全量重索引文本（增量路径不能只
    /// 补 changed_files，否则旧实体索引丢失）。
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, bool)> {
        let conn = Connection::open(path.as_ref())
            .context("打开 SQLite 数据库失败")?;

        // WAL 模式：允许并发读，写操作排队
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("设置 WAL 模式失败")?;
        // 写锁等待 5 秒
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("设置 busy_timeout 失败")?;

        // schema 版本检测（user_version 是 SQLite 内建持久化版本槽位）
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("读取 user_version 失败")?;
        let table_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'entities')",
                [],
                |row| row.get(0),
            )
            .context("查询 entities 表存在性失败")?;

        if !table_exists {
            // 全新数据库：直接建 v2 表
            conn.execute_batch(CREATE_ENTITIES_V2)
                .context("创建 FTS5 表失败")?;
            conn.pragma_update(None, "user_version", 2)
                .context("写入 user_version 失败")?;
            return Ok((Self { conn }, false));
        }

        if version < 2 {
            // 旧 schema（v1 无 tokens 列）：FTS5 虚拟表无法 ALTER 加列，
            // DROP 重建为 v2。旧索引数据随之清空，返回 need_reindex 由
            // 调用方决定全量重索引时机。
            conn.execute_batch("DROP TABLE IF EXISTS entities;")
                .context("删除旧 FTS5 表失败")?;
            conn.execute_batch(CREATE_ENTITIES_V2)
                .context("重建 FTS5 表失败")?;
            conn.pragma_update(None, "user_version", 2)
                .context("写入 user_version 失败")?;
            return Ok((Self { conn }, true));
        }

        Ok((Self { conn }, false))
    }

    // ==================== FTS5 全文搜索 ====================

    /// 从实体文本提取 CJK 2-gram token 串（空格分隔，供 tokens 列）
    ///
    /// 英文词不进 tokens 列：name/signature/source 原列已被 unicode61
    /// 正确分词，覆盖英文检索；tokens 列专为中文子串检索服务，避免
    /// 索引体积成倍膨胀。与查询侧展开（build_match_terms）共用
    /// extract_keywords，切分逻辑单一真源（tokenize.rs）。
    fn cjk_tokens(parts: &[&str]) -> String {
        let mut out: Vec<String> = Vec::new();
        for part in parts {
            for k in extract_keywords(part) {
                // 2-gram 全部为 CJK 字才进 tokens 列
                if k.chars().all(|c| matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)) {
                    out.push(k);
                }
            }
        }
        out.join(" ")
    }

    /// 批量插入实体到 FTS5 表
    pub fn insert_entities_batch(&self, items: &[(CodeNode, String)]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO entities (name, kind, signature, source, file_path, node_json, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        ).context("准备 FTS5 插入语句失败")?;

        for (node, source) in items {
            let node_json = serde_json::to_string(node)
                .context("序列化 CodeNode 失败")?;
            let tokens = Self::cjk_tokens(&[
                &node.name,
                node.signature.as_deref().unwrap_or(""),
                source,
            ]);
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
                tokens,
            ]).context("插入 FTS5 条目失败")?;
        }
        Ok(())
    }

    /// 把用户查询展开为 FTS5 词表：CJK 段 2-gram + 英文词原样
    ///
    /// 与写入侧共用 extract_keywords，保证同构（写入 tokens 列的
    /// 2-gram 查询时必能构造出来）。extract_keywords 输出已剥离
    /// FTS5 特殊字符（引号/括号/逻辑词按非字母数字分隔），词表
    /// 不会引发 MATCH 语法错误；空串/纯标点返回空词表。
    fn build_match_terms(query: &str) -> Vec<String> {
        extract_keywords(query)
    }

    /// FTS5 全文搜索，按 BM25 相关性排序
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f64)>> {
        // 展开词表：空词表（空串/纯标点）→ 空结果（与 hybrid 引擎的
        // 退化语义一致，v35 审计：text 模式此前直接透传导致 FTS5
        // 语法错误上抛中断，三引擎行为不一致）
        let terms = Self::build_match_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // 列集合约束：英文词落在 name/signature/source 原列，CJK 2-gram
        // 落在 tokens 列，单 MATCH 表达式统一命中。
        let match_expr = format!("{{name signature source tokens}} : ({})", terms.join(" OR "));

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

        // 词表来自 extract_keywords 切分（已剥离特殊字符），正常不会
        // 语法错误；残留错误统一转为空结果 + 告警（三引擎一致，不向
        // 上层传播查询中断）。
        let rows = match stmt.query_map(rusqlite::params![match_expr], |row| {
            let node_json: String = row.get(0)?;
            let rank: f64 = row.get(1)?;
            Ok((node_json, rank))
        }) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("FTS5 查询语法错误，返回空结果: {} (query: {})", e, query);
                return Ok(Vec::new());
            }
        };

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
        let (store, need_reindex) = SearchStore::open(tmp_db_path("fts")).unwrap();
        assert!(!need_reindex, "新库无需重建");
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
        let (store, _) = SearchStore::open(tmp_db_path("fts_del")).unwrap();
        let items = vec![
            (make_node("alpha", "src/a.rs"), "alpha code".to_string()),
            (make_node("beta", "src/b.rs"), "beta code".to_string()),
        ];
        store.insert_entities_batch(&items).unwrap();

        let removed = store.delete_entities_by_file("src/a.rs").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.entity_count().unwrap(), 1);
    }

    /// CJK 检索：写入侧 tokens 列承载 2-gram，查询侧同构展开命中
    #[test]
    fn test_fts_cjk_substring_search() {
        let (store, _) = SearchStore::open(tmp_db_path("fts_cjk")).unwrap();
        let items = vec![
            (make_node("提取配置", "src/config.rs"), "fn 提取配置() 读取合并后的配置".to_string()),
            (make_node("save_session", "src/storage.rs"), "fn save_session(id: u64)".to_string()),
        ];
        store.insert_entities_batch(&items).unwrap();

        // 中文子串检索：unicode61 下「配置」是「提取配置」整串的一部分，
        // v1 schema 必零命中；v2 经 tokens 列 2-gram（提取/取配/配置）命中
        let results = store.search_fts("配置", 5).unwrap();
        assert_eq!(results.len(), 1, "中文 2-gram 应命中实体");
        assert_eq!(results[0].0.name, "提取配置");

        // 中文 + 英文混合查询：两腿词表统一进同一 MATCH
        let mixed = store.search_fts("配置 session", 5).unwrap();
        assert_eq!(mixed.len(), 2, "混合查询应同时命中中文与英文实体");
    }

    /// 旧 schema 迁移：v1 表（无 tokens 列）open 后重建为 v2 并返回
    /// need_reindex，二次 open 幂等（不再触发重建）
    #[test]
    fn test_fts_legacy_schema_migration() {
        let path = tmp_db_path("fts_migrate");
        let _ = std::fs::remove_file(&path);
        {
            // 手工构造 v1 旧库：v1 建表 + user_version=1 + 一条旧数据
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE entities USING fts5(
                    name, kind, signature, source, file_path, node_json
                );"
            ).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO entities (name, kind, signature, source, file_path, node_json)
                 VALUES ('old_fn', 'function', 'fn old_fn()', 'old code', 'src/old.rs', '{}')",
                [],
            ).unwrap();
        }

        // 第一次 open：检测旧 schema，重建为 v2，返回 need_reindex
        let (store, need_reindex) = SearchStore::open(&path).unwrap();
        assert!(need_reindex, "旧 schema 必须触发重建标记");
        assert_eq!(store.entity_count().unwrap(), 0, "旧索引数据已清空");

        // 新数据写入后中文检索可用（v2 tokens 列生效）
        let items = vec![
            (make_node("验证迁移", "src/new.rs"), "fn 验证迁移()".to_string()),
        ];
        store.insert_entities_batch(&items).unwrap();
        let results = store.search_fts("迁移", 5).unwrap();
        assert_eq!(results.len(), 1, "迁移后 v2 检索正常");
        assert_eq!(results[0].0.name, "验证迁移");

        // 第二次 open：已是 v2，幂等无重建
        let (_, need_reindex2) = SearchStore::open(&path).unwrap();
        assert!(!need_reindex2, "二次 open 不应再触发重建");
    }

    /// 空串/纯标点查询：返回空结果而非 FTS5 语法错误（三引擎一致）
    #[test]
    fn test_fts_punctuation_query_returns_empty() {
        let (store, _) = SearchStore::open(tmp_db_path("fts_punct")).unwrap();
        let items = vec![(make_node("alpha", "src/a.rs"), "alpha code".to_string())];
        store.insert_entities_batch(&items).unwrap();

        let empty = store.search_fts("", 5).unwrap();
        assert!(empty.is_empty(), "空串查询返回空");
        let punct = store.search_fts("！！！---", 5).unwrap();
        assert!(punct.is_empty(), "纯标点查询返回空（v1 会语法错误上抛）");
    }
}
