# Ticket: Spec合规修复

## Question

以下 4 个 Spec 要求的特性尚未实现：

**Spec #2 — complete_stream**（P0，完全未实现）
- 问题: `LlmProvider::complete_stream()` 的 trait 默认实现返回 `unimplemented!("streaming not supported")`，OpenAiProvider 和 AnthropicProvider 均未覆盖
- 方案: 利用 `reqwest` 已启用 `stream` feature，在 `OpenAiProvider.complete_stream()` 中发起 streaming POST 请求，逐 chunk 读取 SSE 事件，收集为 `Vec<String>` 返回
- 估算: ~60 行

**Spec #3 — Level 0 实体级摘要**（P0，完全未实现）
- 问题: `prompt.rs:221` 已定义 `entity_summary_prompt()` 模板，但 `generate/mod.rs` 的 pipeline 从未调用它
- 方案: 在 `run_generation()` 中可选触发实体级摘要（配置控制或自动判断），结果注入 KnowledgeCard
- 估算: ~40 行

**Spec #4 — `<cite>` 代码引用渲染**（P0，标记为 TODO）
- 问题: `src/output/crossref.rs:55` 的 `<cite>` tag 渲染标记为 TODO，代码引用不可导航
- 方案: 实现跨文件引用链接：`<cite path="src/lib.rs:42">Lib::run</cite>` → `[Lib::run](wiki/path.md#run)`
- 估算: ~30 行

**Spec #9 — 循环依赖 Wiki 标注**（P1，部分实现）
- 问题: `graph.rs:124` 已通过 `detect_cycles()` (tarjan SCC) 检测到循环，但输出完全不渲染循环信息
- 方案: 在 `mermaid.rs` 中为循环依赖节点添加高亮样式（红色虚线框），在 `markdown.rs` 的架构概览中增加循环依赖警告段落
- 估算: ~20 行

## Tickets this blocks

无（4 项互不依赖，可并行实现）

## Dependencies

- [Ticket: 数据模型增强](ticket-03-data-model.md) — 图序列化前置已完成为前提（Spec #4 crossref 需要加载图）
