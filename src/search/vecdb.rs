//! 语义向量存储层：基于 sqlite-vec 0.1.9 的 vec0 虚表封装
//!
//! ## 技术选型（v6 决策 1 修正）
//!
//! 原定 sqlite-vector-rs（HNSW via usearch），实测其依赖链在 Windows/MSVC
//! 三重阻断不可编译：
//! 1. sqlite3_ext 0.2.1 的 `cfg!(unix)` 误用（运行时宏门控编译期 use
//!    `std::os::unix`）→ E0433（纯 Rust 层问题，gcc 工具链同样存在，
//!    windows-gnu target 也没有 std::os::unix）；
//! 2. numkong（usearch 的 C 依赖）的 C99 混合声明 + cc 传 `-std:c99`
//!    → MSVC C 编译器不支持声明后语句 → C2059；
//! 3. 切 windows-gnu 工具链可解 C 层，但 E0433 无解且全项目 C 依赖
//!    （rusqlite bundled/git2/leiden）需重编译，风险不可接受。
//!
//! 改用 **sqlite-vec 0.1.9**（纯 C 静态嵌入，cc 编译，无 usearch/numkong/
//! sqlite3_ext 依赖链）：官方 Rust 用法 = `sqlite3_vec_init` + rusqlite
//! `sqlite3_auto_extension` 注册，Windows 实测编译通过 + 全链路探针通过。
//!
//! ## 语义对齐（阈值换算依据）
//!
//! vec0 的 `distance_metric=cosine` 返回 `1 - cosine_similarity`
//! （sqlite-vec.c:479，与 usearch Cos 同语义）。原实现的相似度阈值 0.3
//! （semantic.rs 硬编码）等价换算为 **distance ≤ 0.7**（1 - 0.3）。
//! 排序方向也一致：distance 升序 = 相似度降序。
//!
//! ## 维度契约
//!
//! vec0 建表时维度固定（`float[N]`）。首次插入时探测向量维度建表；
//! 换 embedding 模型（EmbedSection.model）导致维度变化时自动重建表
//! （丢弃旧索引并告警——与旧实现"删旧索引全量重建"行为一致）。
//!
//! ## 表结构
//!
//! 单表 vec0：`embedding float[N] distance_metric=cosine` + metadata 列
//! `file_path TEXT`（删除/过滤键，与 FTS5 entities 表同归一化规则）、
//! `node_json TEXT`（CodeNode 序列化，KNN 结果直接反序列化，无 join）。

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// 相似度阈值 0.3 换算后的余弦距离上限（1 - 0.3 = 0.7）
///
/// 换算依据见模块头：vec0 cosine distance = 1 - cosine_similarity。
/// 保持与旧实现（cosine_similarity > 0.3）逐位一致的行为。
pub const MAX_COSINE_DISTANCE: f64 = 0.7;

/// KNN 超采样扩大的上限：单次查询最多取这么多候选行
///
/// 循环扩样策略：从 `limit` 起步，若返回行数等于采样数且最后一行
/// 仍 ≤ 阈值（可能有更多候选被截断）则翻倍重查，直到表尽或达到
/// 本上限。上限是防御真实路径（万级实体全相似）退化为全表扫描。
const MAX_KNN_CANDIDATES: usize = 10_000;

/// sqlite-vec 扩展的进程级注册（OnceLock 保证只注册一次）
///
/// `sqlite3_auto_extension` 是全局注册：注册后所有新建的 SQLite 连接
/// 自动加载扩展。重复注册会重复执行 init（无害但冗余），且并发注册
/// 存在竞态风险——用 OnceLock 收敛为一次。
static VEC_EXT_REGISTERED: OnceLock<()> = OnceLock::new();

/// 注册 sqlite-vec 扩展（进程级，幂等）
///
/// 必须在任何 vec0 虚表操作之前调用；所有连接共享该注册。
fn ensure_extension_registered() {
    VEC_EXT_REGISTERED.get_or_init(|| {
        // 类型注解：`transmute` 需显式标注目标类型（clippy::missing_transmute_annotations）。
        // RawAutoExtension = unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char,
        // *const sqlite3_api_routines) -> c_int（SQLite 扩展入口约定）；
        // sqlite3_vec_init 是 extern "C" fn()——ABI 兼容（C 扩展入口），
        // 官方示例即用 transmute（sqlite-vec crate 自带测试同款写法）。
        // register_auto_extension 是 rusqlite 对 sqlite3_auto_extension 的安全封装，
        // 只做条目注册不打开连接，返回值校验由封装完成。
        let init_fn: unsafe extern "C" fn() = sqlite_vec::sqlite3_vec_init;
        let entry: rusqlite::auto_extension::RawAutoExtension =
            unsafe { std::mem::transmute(init_fn as *const ()) };
        // register_auto_extension 内部调用 ffi::sqlite3_auto_extension（unsafe），
        // 封装自身也标注 unsafe：扩展注册是进程级全局状态变更
        unsafe {
            let _ = rusqlite::auto_extension::register_auto_extension(entry);
        }
    });
}

/// 虚表名（与 FTS5 entities 表同库共存的向量存储）
const VECTOR_TABLE: &str = "vectors";

/// vec0 虚表封装：向量持久化 + 带阈值过滤的 KNN 查询
///
/// 职责边界：只做 vec0 虚表的建表/增删查，不含 embedding 生成
/// （EmbeddingEngine 在 SemanticEngine 层调用）与 CodeNode 业务逻辑
/// （node_json 的序列化由调用方负责）。单库单连接（与 SearchStore 同
/// 进程契约：WAL + busy_timeout）。
pub struct VecDb {
    conn: Connection,
}

/// KNN 查询结果行：node_json + 余弦距离
#[derive(Clone)]
pub struct KnnRow {
    /// CodeNode 的 JSON 序列化（调用方反序列化）
    pub node_json: String,
    /// 余弦距离（1 - similarity，升序 = 相似度降序）
    pub distance: f64,
}

impl VecDb {
    /// 打开或创建向量数据库（注册扩展 + 建表延迟到首次插入）
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        ensure_extension_registered();
        let conn = Connection::open(path.as_ref())
            .context("打开向量数据库失败")?;
        // WAL 模式：与 FTS5 存储同并发契约（多读单写）
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("设置 WAL 模式失败")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("设置 busy_timeout 失败")?;
        Ok(Self { conn })
    }

    /// 虚表是否存在
    fn table_exists(&self) -> Result<bool> {
        let sql = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1";
        let count: i64 = self
            .conn
            .query_row(sql, [VECTOR_TABLE], |r| r.get(0))
            .context("查询虚表存在性失败")?;
        Ok(count > 0)
    }

    /// 当前虚表的向量维度（表不存在返回 None）
    pub fn table_dimension(&self) -> Result<Option<usize>> {
        if !self.table_exists()? {
            return Ok(None);
        }
        let sql = "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1";
        let ddl: String = self
            .conn
            .query_row(sql, [VECTOR_TABLE], |r| r.get(0))
            .context("读取虚表定义失败")?;
        // vec0 建表 SQL 形如 "CREATE VIRTUAL TABLE vectors USING vec0(embedding float[3] ..., ...)"
        // 提取 float[N] 中的 N
        let start = ddl.find("float[").map(|i| i + 6);
        let Some(start) = start else {
            // 定义异常（理论不可达：表由本模块创建）→ 显式报错而非猜测维度
            anyhow::bail!("vec0 虚表定义缺少 float[N] 声明: {ddl}");
        };
        let end = ddl[start..].find(']').map(|i| start + i);
        let Some(end) = end else {
            anyhow::bail!("vec0 虚表定义缺少 ] 结束符: {ddl}");
        };
        ddl[start..end]
            .parse::<usize>()
            .map(Some)
            .with_context(|| format!("解析 vec0 维度失败: {}", &ddl[start..end]))
    }

    /// 以指定维度创建 vec0 虚表
    fn create_table(&self, dim: usize) -> Result<()> {
        let sql = format!(
            "CREATE VIRTUAL TABLE {VECTOR_TABLE} USING vec0(\
                embedding float[{dim}] distance_metric=cosine,\
                file_path TEXT,\
                node_json TEXT\
            )"
        );
        self.conn
            .execute_batch(&sql)
            .with_context(|| format!("创建 vec0 虚表失败（dim={dim}）"))
    }

    /// 丢弃并重建虚表（维度变化时调用，旧索引一并丢弃）
    fn rebuild_table(&self, dim: usize) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP TABLE IF EXISTS {VECTOR_TABLE};"))
            .context("删除旧 vec0 虚表失败")?;
        self.create_table(dim)
    }

    /// 确保虚表存在且维度匹配；不匹配时重建
    ///
    /// `dim` 为本次插入向量的维度（EmbeddingEngine 首次产出后才知道）。
    /// 表不存在 → 按 dim 建表；表存在但维度不同（换模型）→ 重建并告警。
    fn ensure_table(&self, dim: usize) -> Result<()> {
        match self.table_dimension()? {
            None => self.create_table(dim),
            Some(existing) if existing != dim => {
                tracing::warn!(
                    "向量维度变化（{} → {}），重建语义索引（embedding 模型变更需全量重新索引）",
                    existing, dim
                );
                self.rebuild_table(dim)
            }
            Some(_) => Ok(()),
        }
    }

    /// 批量插入向量（node_json 由调用方序列化）
    ///
    /// 首次插入时按首个向量维度建表。file_path 归一化与 FTS5 表同规则
    /// （跨平台路径键统一，删除/过滤同基准）。
    pub fn insert_batch(&self, items: &[(String, String, Vec<f32>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let dim = items[0].2.len();
        if dim == 0 {
            anyhow::bail!("空向量无法入库（维度为 0）");
        }
        self.ensure_table(dim)?;
        // sqlite-vec 0.1.9 的 vec0Update_InsertRowidStep 支持省略 rowid 自动分配
        // （vec0 表声明 rowid INTEGER PRIMARY KEY AUTOINCREMENT）。增量路径
        // remove_by_file 只删部分行，剩余行的 rowid 可能与显式 rowid
        // （按 enumerate 序号）撞车导致主键冲突；省略 rowid 后由虚表自增
        // 分配，消除冲突。
        let mut stmt = self.conn.prepare(&format!(
            "INSERT INTO {VECTOR_TABLE}(embedding, file_path, node_json) \
             VALUES (?1, ?2, ?3)"
        ))?;
        for (file_path, node_json, vector) in items.iter() {
            // vec0 的 embedding 列接受 JSON 数组字符串（探针验证：'[1,0,0]' 直接可用）
            let json = vector_to_json(vector);
            stmt.execute(rusqlite::params![
                json,
                crate::incremental::norm_sep(file_path),
                node_json,
            ])
            .with_context(|| format!("插入向量失败: {file_path}"))?;
        }
        Ok(())
    }

    /// 带阈值过滤的 KNN 查询：返回余弦距离 ≤ `max_distance` 的全部行
    ///
    /// vec0 的 KNN 是 LIMIT 截断，无法在 SQL 侧表达距离谓词
    /// （knn_match 是唯一 distance 约束，见 sqlite-vec best_index 实现），
    /// 因此用**循环扩样**保证阈值语义正确：
    /// - 从 `limit` 起步采样，若返回行数 == 采样数且最后一行仍 ≤ 阈值，
    ///   说明可能有更多候选被截断 → 翻倍重查；
    /// - 直到返回行数 < 采样数（表尽）或最后一行 > 阈值（边界内已全）
    ///   或达到 MAX_KNN_CANDIDATES 上限。
    ///
    /// 该策略与旧实现（全量加载 + 逐条余弦 + 过滤）在阈值语义上
    /// 完全等价，且正常场景（阈值过滤后结果远小于 limit）只查一次。
    /// 表不存在时返回空（等价于空索引）。
    ///
    /// 扩样重查的去重：翻倍 LIMIT 后，前一次已取过的行会再次返回
    /// （KNN 结果按距离稳定排序），按 node_json 去重合并，保证
    /// 返回行数 = 阈值内候选数（不因扩样重复）。
    pub fn knn(&self, query_json: &str, limit: usize, max_distance: f64) -> Result<Vec<KnnRow>> {
        if !self.table_exists()? {
            return Ok(Vec::new());
        }
        let mut sample = limit.max(1);
        let mut all: Vec<KnnRow> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let sql = format!(
                "SELECT node_json, distance FROM {VECTOR_TABLE} \
                 WHERE embedding MATCH ?1 \
                 ORDER BY distance \
                 LIMIT {sample}"
            );
            let mut stmt = self.conn.prepare(&sql).context("准备 KNN 查询失败")?;
            let rows: Vec<KnnRow> = stmt
                .query_map([query_json], |r| {
                    Ok(KnnRow {
                        node_json: r.get(0)?,
                        distance: r.get(1)?,
                    })
                })
                .context("执行 KNN 查询失败")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("读取 KNN 结果失败")?;

            // 本次采样内全部 ≤ 阈值 → 并入结果（按 node_json 去重，扩样重查防重复）
            for row in rows.iter().filter(|r| r.distance <= max_distance) {
                if seen.insert(row.node_json.clone()) {
                    all.push(row.clone());
                }
            }
            // 提前终止条件：
            // 1. 表尽（返回行数 < 采样数）——已取完所有候选
            // 2. 采样内最后一行 > 阈值——阈值边界之后的（更远）行必然也 > 阈值
            //    （distance 升序），无更多可并入结果
            // 3. 达到扩样上限——防御全表相似退化
            if rows.len() < sample || rows.last().map(|r| r.distance > max_distance).unwrap_or(true) || sample >= MAX_KNN_CANDIDATES {
                break;
            }
            sample = (sample * 2).min(MAX_KNN_CANDIDATES);
        }
        Ok(all)
    }

    /// 删除指定文件路径关联的所有向量（file_path 键与插入同基准）
    pub fn remove_by_file(&self, file_path: &str) -> Result<usize> {
        if !self.table_exists()? {
            return Ok(0);
        }
        let sql = format!("DELETE FROM {VECTOR_TABLE} WHERE file_path = ?1");
        let count = self
            .conn
            .execute(&sql, rusqlite::params![crate::incremental::norm_sep(file_path)])
            .context("删除向量失败")?;
        Ok(count)
    }

    /// 清空所有向量
    pub fn clear(&self) -> Result<()> {
        if !self.table_exists()? {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!("DELETE FROM {VECTOR_TABLE};"))
            .context("清空向量失败")
    }

    /// 当前向量条数（表不存在视为 0）
    pub fn entry_count(&self) -> Result<usize> {
        if !self.table_exists()? {
            return Ok(0);
        }
        let sql = format!("SELECT COUNT(*) FROM {VECTOR_TABLE}");
        let count: i64 = self
            .conn
            .query_row(&sql, [], |r| r.get(0))
            .context("查询向量数失败")?;
        Ok(count as usize)
    }
}

/// f32 向量 → vec0 可用的 JSON 数组字符串（`[0.1,0.2,...]`）
///
/// vec0 的 embedding 列接受 JSON 数组文本（探针验证）；不使用二进制
/// blob 传输，避免引入额外序列化依赖。
fn vector_to_json(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f}")).collect();
    format!("[{}]", parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时数据库路径（进程内自增序号防并行冲突）
    fn tmp_db(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("vecdb_{}_{}_{}.db", tag, std::process::id(), id));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn make_items() -> Vec<(String, String, Vec<f32>)> {
        vec![
            ("src/a.rs".into(), r#"{"name":"alpha"}"#.into(), vec![1.0, 0.0, 0.0]),
            ("src/b.rs".into(), r#"{"name":"beta"}"#.into(), vec![0.0, 1.0, 0.0]),
            ("src/c.rs".into(), r#"{"name":"gamma"}"#.into(), vec![-1.0, 0.0, 0.0]),
        ]
    }

    #[test]
    fn test_insert_and_knn_cosine_ranking() {
        let db = VecDb::open(tmp_db("knn")).unwrap();
        db.insert_batch(&make_items()).unwrap();

        // 查询 alpha 附近：alpha 距离最小（≈1-cos），gamma 最远
        let rows = db.knn("[0.9,0.1,0]", 10, MAX_COSINE_DISTANCE).unwrap();
        // 阈值 0.7：alpha(0.006) 与 beta(0.89>0.7? 否——beta 距离 0.89 > 0.7 被过滤)
        // 实际：alpha d≈0.006 ≤0.7 ✓；beta d≈0.89 >0.7 ✗；gamma d≈1.99 >0.7 ✗
        assert_eq!(rows.len(), 1, "只有 alpha 在 0.3 相似度阈值内");
        assert!(rows[0].node_json.contains("alpha"));
        assert!(rows[0].distance < 0.01);
    }

    #[test]
    fn test_threshold_0_3_maps_to_distance_0_7() {
        // 阈值换算锚定：0.3 相似度 ↔ 0.7 距离（常量即契约）
        assert_eq!(MAX_COSINE_DISTANCE, 0.7);
    }

    #[test]
    fn test_knn_without_threshold_returns_all() {
        let db = VecDb::open(tmp_db("knn_all")).unwrap();
        db.insert_batch(&make_items()).unwrap();

        // max_distance=2.0（放行全部）：3 行按距离升序返回
        let rows = db.knn("[0.9,0.1,0]", 10, 2.0).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].node_json.contains("alpha"));
        assert!(rows[2].node_json.contains("gamma"));
        assert!(rows[0].distance < rows[1].distance);
    }

    #[test]
    fn test_knn_expands_sample_to_cover_threshold() {
        // 扩样正确性：插入 30 个与查询高度相似（distance ≤ 0.7）的向量，
        // limit=5。若不做扩样只取 5 条会漏掉；循环扩样必须返回全部 30 条。
        let db = VecDb::open(tmp_db("expand")).unwrap();
        let items: Vec<(String, String, Vec<f32>)> = (0..30)
            .map(|i| (format!("src/f{i}.rs"), format!(r#"{{"name":"f{i}"}}"#), vec![1.0, 0.0, 0.0]))
            .collect();
        db.insert_batch(&items).unwrap();

        let rows = db.knn("[1,0,0]", 5, MAX_COSINE_DISTANCE).unwrap();
        assert_eq!(rows.len(), 30, "阈值内全部候选必须返回（扩样不能截断）");
    }

    #[test]
    fn test_delete_by_file_and_count() {
        let db = VecDb::open(tmp_db("del")).unwrap();
        db.insert_batch(&make_items()).unwrap();
        assert_eq!(db.entry_count().unwrap(), 3);

        let removed = db.remove_by_file("src/b.rs").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(db.entry_count().unwrap(), 2);

        db.clear().unwrap();
        assert_eq!(db.entry_count().unwrap(), 0);
    }

    #[test]
    fn test_dimension_mismatch_rebuilds_table() {
        let db = VecDb::open(tmp_db("dim")).unwrap();
        db.insert_batch(&make_items()).unwrap(); // 3 维建表

        // 换"模型"（4 维）→ 自动重建，旧数据丢弃
        db.insert_batch(&[(
            "src/d.rs".into(),
            r#"{"name":"delta"}"#.into(),
            vec![1.0, 0.0, 0.0, 0.0],
        )])
        .unwrap();
        assert_eq!(db.entry_count().unwrap(), 1, "维度变化重建后只剩新数据");

        let rows = db.knn("[1,0,0,0]", 10, MAX_COSINE_DISTANCE).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].node_json.contains("delta"));
    }

    #[test]
    fn test_empty_batch_and_zero_dim() {
        let db = VecDb::open(tmp_db("empty")).unwrap();
        db.insert_batch(&[]).unwrap(); // 空批次静默成功
        assert!(db.insert_batch(&[("a".into(), "b".into(), vec![])]).is_err(), "零维向量应报错");
    }

    #[test]
    fn test_empty_db_operations_are_noop() {
        let db = VecDb::open(tmp_db("no_table")).unwrap();
        assert_eq!(db.entry_count().unwrap(), 0);
        assert!(db.knn("[1,0,0]", 10, MAX_COSINE_DISTANCE).unwrap().is_empty());
        assert_eq!(db.remove_by_file("src/a.rs").unwrap(), 0);
        db.clear().unwrap(); // 表不存在时静默成功
    }

    #[test]
    fn test_insert_batch_after_partial_removal() {
        // 「第二次运行」范式回归锚（P0-1）：增量路径必须测非空表场景。
        // 增量流程（lib.rs 的 remove_by_file → index_batch）在表非空时
        // 删旧行再插新行，剩余行的 rowid 是不连续的残值。旧实现按
        // enumerate 序号显式写 rowid（1..N 每批重启），残留 rowid 与
        // 新批显式 rowid 撞车 → SQLITE_CONSTRAINT_PRIMARYKEY；现实现
        // 省略 rowid 由 vec0 自增分配（见 insert_batch 注释），本场景
        // 必须通过——任何回归到显式 rowid 的实现都会被此测试拦下。
        let db = VecDb::open(tmp_db("incr")).unwrap();

        // 首批 5 条：其中 2 条共用同一 file_path（模拟单文件多实体，
        // remove_by_file 一次删整组，与增量路径的删旧行语义一致）
        let mut items = make_items();
        items.push(("src/shared.rs".into(), r#"{"name":"delta"}"#.into(), vec![0.0, 0.0, 1.0]));
        items.push(("src/shared.rs".into(), r#"{"name":"epsilon"}"#.into(), vec![0.5, 0.5, 0.0]));
        db.insert_batch(&items).unwrap();
        assert_eq!(db.entry_count().unwrap(), 5);

        // 增量第一步：文件级变化删除旧行（2 条同路径一并删除）
        let removed = db.remove_by_file("src/shared.rs").unwrap();
        assert_eq!(removed, 2);
        assert_eq!(db.entry_count().unwrap(), 3);

        // 增量第二步：再次 insert_batch 3 条新路径（表非空 + rowid 残值）
        let second: Vec<(String, String, Vec<f32>)> = (0..3)
            .map(|i| (format!("src/next{i}.rs"), format!(r#"{{"name":"next{i}"}}"#), vec![0.2, 0.8, 0.0]))
            .collect();
        db.insert_batch(&second).unwrap();

        // 全表查询（max_distance=2.0 放行全部，同既有测试范式）：6 条且含新增路径
        let rows = db.knn("[1,0,0]", 10, 2.0).unwrap();
        assert_eq!(rows.len(), 6, "非空表 remove 后 insert_batch 必须成功且行数正确");
        assert!(rows.iter().any(|r| r.node_json.contains("next0")));
        assert!(rows.iter().any(|r| r.node_json.contains("next2")));
        assert_eq!(db.entry_count().unwrap(), 6);
    }
}
