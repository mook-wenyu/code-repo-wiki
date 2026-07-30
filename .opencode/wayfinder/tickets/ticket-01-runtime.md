# Ticket: Runtime架构

## Question

如何消除 5 个隔离 tokio Runtime 导致的线程池浪费和冗余创建？

**问题现状**:
- `src/lib.rs:59` — `run_pipeline` 中 `Runtime::new()`
- `src/lib.rs:123` — `run_incremental_pipeline` 中 `Runtime::new()`
- `src/search/semantic.rs:37,62,82` — `index()`, `index_batch()`, `search()` 中各创建一次 `Runtime::new()`

**约束**:
- `run_pipeline` 和 `run_incremental_pipeline` 是同步函数（接受 Path，返回 Result）
- `SemanticEngine` 需要 async 上下文调用 `EmbeddingEngine.embed_batch()`
- 不想让 main.rs 变成 async main（过度设计，大量同步代码）

**候选方案**:
1. **Arc<Runtime> 全局缓存**: 在 `lib.rs` 中用 `OnceLock<Arc<Runtime>>` 持有，`run_pipeline`/`run_incremental_pipeline` 获取引用，传给 `SemanticEngine`
2. **lazy_static / OnceCell**: 类似方案
3. **main() 创建 RT 作为参数传下去**: 侵入性大，interface 污染

推荐方案 1。

## Tickets this blocks

无（独立修改）

## Assets

`docs/full-audit-report.md:92` — P0.2 问题描述
