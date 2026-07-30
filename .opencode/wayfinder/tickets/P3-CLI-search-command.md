## 问题

`repo-wiki search` 子命令的 CLI 接口如何设计？

### 需要回答的子问题

1. 查询参数：（`--query`, `--top-k`, `--engine text|semantic|hybrid|ast`）
2. 输出格式：（表格 / JSON / 仅路径列表）
3. 是否需要全局 `--config` 参数？
4. 未指定 engine 时默认使用 hybrid？

### 影响范围

- `src/main.rs` — 新 subcommand
- `src/search/hybrid.rs` — SearchHit 输出格式

### 验证标准

`cargo run -- search -q "auth" -k 5` 返回非空结果列表（表格形式）；
`cargo run -- search -q "auth" --json` 返回 JSON 格式。
