# Ticket: 数据模型增强

## Question

两个数据流问题需要解决：

**P0.4 — FileInsight 携带 source 字段**:
- 问题: `build_source_map()` 在搜索索引构建时重新 `read_to_string()` 每个文件。但解析阶段已经在 `parser/mod.rs` 中读过一次
- 方案: `FileInsight` 增加 `source: String` 字段（`src/ingest/parser/mod.rs:13-19`），`parse_file()` 在解析后填充。`build_source_map()` 直接从 `insight.source` 取，跳过磁盘 I/O
- 影响文件: `parser/mod.rs`(FileInsight结构体), 6个parser(parse返回), `lib.rs`(build_source_map使用), `incremental/state.rs`(compute_file_fingerprint使用)
- 估算: ~25 行

**Spec #1 — 图序列化**:
- 问题: 增量更新需要加载上一轮的知识图谱进行影响传播，但 `KnowledgeGraph` 无持久化。每次增量更新都重头 build graph
- 方案: 利用 `petgraph` 已启用 `serde-1` feature，给 `KnowledgeGraph` 加 `save(path)` 和 `load(path)` 方法，写入 `output.dir/graph/` 目录
- 估算: ~80 行

## Tickets this blocks

- [Ticket: Spec合规修复](ticket-04-spec-gaps.md) — 图序列化是增量更新的前置

## Assets

`docs/full-audit-report.md:93-94` — P0.4 + Spec#1 描述
