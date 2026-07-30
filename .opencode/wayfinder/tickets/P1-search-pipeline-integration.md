## 问题

搜索索引（TextEngine + SemanticEngine）如何集成到 `lib.rs::run_pipeline` 和 `lib.rs::run_incremental_pipeline` 中？

### 需要回答的子问题

1. 索引数据存放在哪里？（`output.dir/.search/` 还是 `output.dir/.state/`？）
2. 什么时候构建索引？（build_graph 后遍历所有 entity 索引）
3. 增量更新时如何更新索引？（删除旧 entity + 添加新 entity）
4. 是否需要 `[search]` 配置项控制行为？
5. 错误处理：embedding 引擎不可用时（无 API key）降级为 text-only

### 影响范围

- `src/lib.rs` — pipeline 集成点
- `src/config/schema.rs` — 搜索配置
- `src/search/text.rs` — 当前 API 是否满足需求？
- `src/search/semantic.rs` — 是否需添加 batch_index 方法？

### 验证标准

全量 pipeline 运行后，`.repo-wiki/.search/` 中存在非空索引文件；
增量更新后搜索索引反映最新内容。
