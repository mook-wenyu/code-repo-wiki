# Ticket: 依赖清理

## Question

Cargo.toml 中 `sqlite-vec = "0.1.9"` 和 `async-openai = "0.27"` 两个依赖在代码中完全未使用，是否应该删除？

**证据**:
- `sqlite-vec`: grep 全仓库 0 引用。向量搜索使用 `rusqlite` 的 BLOB 自定义存储（`src/search/store.rs` 的 `vectors` 表），非 sqlite-vec 扩展。保留导致 ~5 分钟额外编译时间（含 LLVM IR 编译 sqlite3 绑定）
- `async-openai`: grep 全仓库 0 引用。实际 LLM 调用通过 `reqwest::Client` 直接构造 HTTP 请求到 OpenAI/Anthropic REST API（`src/generate/llm.rs`）

**决策**: **删除两者**。代价：极低（无代码改动）。收益：编译时间缩短 ~5 分钟 + Cargo.lock 更小。

## Tickets this blocks

无

## Assets

`docs/full-audit-report.md:91` — P0.1 问题描述
