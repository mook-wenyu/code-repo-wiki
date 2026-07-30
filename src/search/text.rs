use std::collections::{HashMap, HashSet};
use std::path::Path;
use anyhow::{Context, Result};

use crate::model::CodeNode;

/// BM25 全文搜索引擎——bincode 持久化
///
/// 索引数据通过 bincode 序列化到磁盘文件，进程重启后自动加载。
/// 在内存中执行 BM25 计算（repo 规模下零延迟），避免外部数据库依赖。
pub struct TextEngine {
    docs: Vec<DocEntry>,
    df: HashMap<String, usize>,
    avg_doc_len: f64,
    total_docs: usize,
    /// 持久化文件路径（None 表示纯内存模式，不持久化）
    persist_path: Option<std::path::PathBuf>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DocEntry {
    node: CodeNode,
    source_code: String,
    tokens: Vec<String>,
}

impl TextEngine {
    /// 创建或加载持久化搜索引擎。
    ///
    /// 指定 path 时，如果文件已存在则从其中恢复索引数据。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let persist_path = path.as_ref().to_path_buf();
        if persist_path.exists() {
            let data = std::fs::read(&persist_path)
                .context("读取持久化索引文件失败")?;
            let persisted: PersistedIndex = bincode::deserialize(&data)
                .context("反序列化持久化索引失败")?;
            return Ok(Self {
                docs: persisted.docs,
                df: persisted.df,
                avg_doc_len: persisted.avg_doc_len,
                total_docs: persisted.total_docs,
                persist_path: Some(persist_path),
            });
        }
        Ok(Self {
            docs: Vec::new(), df: HashMap::new(),
            avg_doc_len: 0.0, total_docs: 0,
            persist_path: Some(persist_path),
        })
    }

    /// 纯内存模式（不持久化）
    pub fn new_in_memory() -> Self {
        Self {
            docs: Vec::new(), df: HashMap::new(),
            avg_doc_len: 0.0, total_docs: 0,
            persist_path: None,
        }
    }

    /// 持久化当前索引到文件
    fn save(&self) -> Result<()> {
        if let Some(ref path) = self.persist_path {
            let data = PersistedIndex {
                docs: self.docs.clone(),
                df: self.df.clone(),
                avg_doc_len: self.avg_doc_len,
                total_docs: self.total_docs,
            };
            let bytes = bincode::serialize(&data)
                .context("序列化持久化索引失败")?;
            std::fs::write(path, &bytes)
                .context("写入持久化索引文件失败")?;
        }
        Ok(())
    }

    /// 索引一个 CodeNode，自动持久化。
    pub fn index(&mut self, node: &CodeNode, source_code: &str) -> Result<()> {
        let text = format!(
            "{} {:?} {} {}",
            node.name, node.kind, node.signature.as_deref().unwrap_or(""), source_code
        );
        let tokens = tokenize(&text);
        let unique: HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
        for tok in &unique {
            *self.df.entry(tok.to_string()).or_insert(0) += 1;
        }
        self.docs.push(DocEntry {
            node: node.clone(),
            source_code: source_code.to_string(),
            tokens,
        });
        self.total_docs = self.docs.len();
        self.avg_doc_len = self.docs.iter().map(|d| d.tokens.len() as f64).sum::<f64>()
            / self.total_docs.max(1) as f64;
        self.save()?;
        Ok(())
    }

    /// BM25 搜索，返回 (CodeNode, score) 按相关性降序。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(CodeNode, f64)>> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() || self.total_docs == 0 {
            return Ok(Vec::new());
        }
        let k1 = 1.5;
        let b = 0.75;

        let mut scores: Vec<(usize, f64)> = (0..self.docs.len()).map(|i| (i, 0.0)).collect();
        for (i, entry) in self.docs.iter().enumerate() {
            let dl = entry.tokens.len() as f64;
            let mut score = 0.0;
            for qt in &q_tokens {
                let tf = entry.tokens.iter().filter(|t| *t == qt).count() as f64;
                let df = self.df.get(qt).copied().unwrap_or(1);
                let idf = ((self.total_docs as f64 - df as f64 + 0.5)
                    / (df as f64 + 0.5) + 1.0).ln();
                score += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * dl / self.avg_doc_len));
            }
            scores[i].1 = score;
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let results = scores.into_iter()
            .filter(|(_, s)| *s > 0.0)
            .take(limit)
            .map(|(i, s)| (self.docs[i].node.clone(), s))
            .collect();
        Ok(results)
    }

    /// 清空索引（同时删除持久化文件）
    pub fn clear(&mut self) -> Result<()> {
        self.docs.clear();
        self.df.clear();
        self.avg_doc_len = 0.0;
        self.total_docs = 0;
        if let Some(ref path) = self.persist_path {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    pub fn doc_count(&self) -> usize { self.total_docs }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedIndex {
    docs: Vec<DocEntry>,
    df: HashMap<String, usize>,
    avg_doc_len: f64,
    total_docs: usize,
}

/// 分词：转小写，按非字母数字分割
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() >= 2)
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeId, NodeKind};

    fn make_node(name: &str, kind: NodeKind, _id: u64) -> CodeNode {
        CodeNode {
            id: NodeId::new(0), kind, name: name.into(),
            file_path: None, line_range: None, doc_comment: None,
            signature: None, module_path: vec![],
        }
    }

    fn tmp_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!("text_{}_{}.bin", label, COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_index_and_search() -> Result<()> {
        let mut engine = TextEngine::open(tmp_path("index_search"))?;
        engine.index(&make_node("add_user", NodeKind::Function, 0), "fn add_user(name: &str)")?;
        engine.index(&make_node("delete_user", NodeKind::Function, 1), "fn delete_user(id: u64)")?;
        let results = engine.search("add", 10)?;
        assert!(!results.is_empty());
        assert!(results[0].0.name.contains("add"));
        Ok(())
    }

    #[test]
    fn test_empty_engine() -> Result<()> {
        let engine = TextEngine::open(tmp_path("empty"))?;
        assert!(engine.search("anything", 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn test_scoring_order() -> Result<()> {
        let mut engine = TextEngine::open(tmp_path("scoring"))?;
        engine.index(&make_node("auth_login", NodeKind::Function, 0), "handle user login")?;
        engine.index(&make_node("user_profile", NodeKind::Function, 1), "user profile page")?;
        let results = engine.search("user", 10)?;
        assert!(results.len() >= 1);
        Ok(())
    }

    #[test]
    fn test_persistence() -> Result<()> {
        let path = tmp_path("persist");
        {
            let mut engine = TextEngine::open(&path)?;
            engine.index(&make_node("persist_test", NodeKind::Function, 0), "fn test()")?;
        }
        let engine = TextEngine::open(&path)?;
        assert_eq!(engine.doc_count(), 1);
        let results = engine.search("persist_test", 10)?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[test]
    fn test_clear() -> Result<()> {
        let mut engine = TextEngine::open(tmp_path("clear"))?;
        engine.index(&make_node("x", NodeKind::Function, 0), "")?;
        assert_eq!(engine.doc_count(), 1);
        engine.clear()?;
        assert_eq!(engine.doc_count(), 0);
        Ok(())
    }
}