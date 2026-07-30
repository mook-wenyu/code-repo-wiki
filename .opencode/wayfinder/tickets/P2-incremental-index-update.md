## 问题

文件变更触发增量更新时，搜索索引如何只做增量更新而不全量重建？

### 子问题

1. 如何在 `incremental::run_incremental_update` 中获取变更影响到的 entity 列表？
2. TextEngine 需要 `delete(node_id)` / `update(node, source_code)` 方法
3. SemanticEngine 也需要类似的增量 API（但重新 embedding 成本高）
4. 全量重建 vs 增量更新的取舍标准（变更文件超过 N% 时全量重建）

### 影响范围

- `src/incremental/mod.rs` — 增量流程
- `src/search/text.rs` — 新增 delete/update 方法
- `src/search/semantic.rs` — 新增 delete/update 方法
- `src/lib.rs` — 增量 pipeline 集成

### 验证标准

修改一个文件后增量更新，只有该文件的 entity 索引发生变化（而不是全量重建）。
