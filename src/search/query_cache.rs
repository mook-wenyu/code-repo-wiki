//! 查询 embedding LRU 缓存
//!
//! 语义搜索每次查询都要把用户 query 发给远程 embedding API（一次网络 RTT +
//! API 计费）。重复查询（MCP server / watch 常驻进程内同义近义查询高频出现）
//! 若每次都重新嵌入则浪费。本缓存按 query 原样缓存向量，LRU 淘汰上限 256 条。
//!
//! ## 线程安全
//!
//! map/order 分两个 Mutex（设计定案），所有路径都**不嵌套持锁**——
//! 命中路径先取 map 再释放、再取 order 刷新；未命中路径先取 order 淘汰、
//! 释放后再取 map 插入。避免两锁逆序获取的经典死锁。
//!
//! ## 缓存键命名空间（P1）
//!
//! 向量与模型强相关。调用方（semantic.rs）在缓存键中并入 embedding 模型名
//! （`{model}\u{0}{query}`）作为命名空间：MCP 长驻进程 + config.toml 热改
//! `[embed].model` 后，旧模型的 query 向量不会命中（维度变化报错、同维静默
//! 劣化两条路径都被消除）；本结构自身不感知模型，只按键存取。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;

/// 默认 LRU 上限：256 条查询向量（每条约 1024 维 f32 ≈ 4KB，总 ~1MB 内存，
/// 覆盖 MCP server 会话内的查询复用）
const DEFAULT_CAPACITY: usize = 256;

/// 进程级共享缓存（execute_search 每次调用新建 SemanticEngine 时注入同一
/// 实例，跨查询复用；MCP server 常驻场景收益最大）
static SHARED: OnceLock<Arc<QueryEmbedCache>> = OnceLock::new();

/// 查询嵌入 LRU 缓存（thread-safe，`Arc<Vec<f32>>` 零拷贝共享）
pub struct QueryEmbedCache {
    map: Mutex<HashMap<String, Arc<Vec<f32>>>>,
    /// LRU 访问顺序：队尾最新（push_back），队首最旧（淘汰 pop_front）
    order: Mutex<VecDeque<String>>,
    cap: usize,
}

impl QueryEmbedCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
            cap,
        }
    }

    /// 返回进程级共享缓存实例（OnceLock 单例）
    pub fn shared() -> Arc<QueryEmbedCache> {
        SHARED
            .get_or_init(|| Arc::new(QueryEmbedCache::new()))
            .clone()
    }

    /// 当前缓存条目数（测试/诊断用）
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 缓存是否为空（len 配套，clippy len_without_is_empty 门禁）
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取查询向量：命中返回缓存值（并刷新 LRU 位），未命中调用 `embed`
    /// 生成后插入（超上限淘汰队首）。
    ///
    /// 网络 IO 在锁外执行（不持锁等待 API），避免长阻塞串行化其他查询；
    /// 两线程并发未命中同一 query 时各自嵌入一次、后者覆盖（幂等无害）。
    pub fn get_or_embed<F>(&self, query: &str, embed: F) -> Result<Arc<Vec<f32>>>
    where
        F: FnOnce(&str) -> Result<Vec<f32>>,
    {
        // 命中：克隆 Arc（O(1)），随后单独刷新 LRU 位（不嵌套持锁）
        if let Some(v) = self
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(query)
            .cloned()
        {
            self.refresh_order(query);
            return Ok(v);
        }
        // 未命中：锁外调用 embed（网络 RTT 不占锁）
        let vector = Arc::new(embed(query)?);
        // 先更新 order（淘汰队首），释放后再写 map——两锁不嵌套
        let evicted = {
            let mut order = self.order.lock().unwrap_or_else(|e| e.into_inner());
            order.push_back(query.to_string());
            if order.len() > self.cap {
                order.pop_front()
            } else {
                None
            }
        };
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(evicted) = evicted {
            map.remove(&evicted);
        }
        map.insert(query.to_string(), vector.clone());
        Ok(vector)
    }

    /// 命中后把 key 移到队尾（MRU），O(n) 但 n ≤ 256
    fn refresh_order(&self, query: &str) {
        let mut order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = order.iter().position(|k| k == query) {
            order.remove(pos);
            order.push_back(query.to_string());
        }
    }
}

impl Default for QueryEmbedCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_miss_embeds_and_caches() {
        let cache = QueryEmbedCache::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let embed = |q: &str| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![q.len() as f32, 0.0])
        };
        let v1 = cache.get_or_embed("alpha", embed).unwrap();
        assert_eq!(v1.as_ref(), &vec![5.0, 0.0]);
        let v2 = cache.get_or_embed("alpha", embed).unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第二次命中不应再嵌入"
        );
        assert_eq!(v1, v2);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_lru_evicts_oldest() {
        let cache = QueryEmbedCache::with_capacity(2);
        let embed = |q: &str| Ok(vec![q.len() as f32]);
        cache.get_or_embed("a", embed).unwrap();
        cache.get_or_embed("b", embed).unwrap();
        cache.get_or_embed("c", embed).unwrap(); // 淘汰 "a"
        assert_eq!(cache.len(), 2);
        // "a" 已淘汰 → 重新嵌入（计数不可见，但条目回来）
        cache.get_or_embed("a", embed).unwrap();
        assert_eq!(cache.len(), 2, "淘汰后再插入仍受 cap 限制");
    }

    #[test]
    fn test_refresh_moves_to_mru() {
        let cache = QueryEmbedCache::with_capacity(2);
        let embed = |q: &str| Ok(vec![q.len() as f32]);
        cache.get_or_embed("a", embed).unwrap();
        cache.get_or_embed("b", embed).unwrap();
        // 命中 "a" → 它变最新；再插入 "c" 应淘汰 "b"
        cache.get_or_embed("a", embed).unwrap();
        cache.get_or_embed("c", embed).unwrap();
        // "a" 仍在（被刷新为 MRU），"b" 被淘汰
        assert_eq!(cache.len(), 2);
        let embed2 = |q: &str| Ok(vec![q.len() as f32]);
        let va = cache.get_or_embed("a", embed2).unwrap();
        assert_eq!(va.as_ref(), &vec![1.0]);
    }
}
