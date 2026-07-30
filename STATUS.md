# 项目状态简报

## 一、架构健康度
- 当前模块总数：8（config, ingest, analysis, generate, output, incremental, model, search）
- 违规跨模块调用：无
- 死代码清理后模块更紧凑

## 二、本次变更影响范围
- 修改的功能：深度审计 + 死代码清理 + CLI 精简 + 配置精简 + 已知 Bug 修复
- 删除的文件：
  - `src/analysis/dependency.rs`（351 行，全部 pub 函数无外部调用）
  - `src/analysis/unity.rs`（336 行，UnityEnricher 从未实例化）
  - `src/output/map.rs`（177 行，render_codebase_map 从未被调用）
- 删除的 CLI 命令：`InstallToOpencode`、`UninstallFromOpencode`、`Export --format pdf`
- 精简的配置：`OutputFormat::Html`、`EmbedProviderType::Ollama/Custom`、`WikiTemplate::ProductRequirement`
- 修复的 Bug：
  - `card.rs: _max_concurrent` 死参数
  - `chunk.rs: _file_stem` 未使用变量
  - `incremental/mod.rs: _state` 已加载未使用
  - `graph.rs: serialize_graph/deserialize_graph` 死函数（仅测试引用）
- 摸到的文件：Cargo.toml, src/main.rs, src/analysis/mod.rs, src/analysis/graph.rs, src/output/mod.rs, src/config/schema.rs, src/generate/card.rs, src/generate/chunk.rs, src/generate/mod.rs, src/incremental/mod.rs, src/output/html.rs, src/search/semantic.rs
- 是否改变了接口/契约：是——generate_all_cards 去除 max_concurrent 参数；OutputFormat/EmbedProviderType/WikiTemplate 枚举精简

## 三、已知风险点
- 搜索引擎部分（search/）仍未接入主 pipeline，纯死代码
- `tree-sitter-c`、`walkdir`、`uuid`、`rayon`、`thiserror` 依赖实际未使用但保留在 Cargo.toml（限于网络不可达无法下载新版依赖树）
- `tokio features = ["full"]` 未降级（同上原因）

## 四、下次最该做的事
1. 将 search/ 模块集成到 pipeline（P1，lib.rs 在 build_graph 后自动构建搜索索引）
2. 实现 CLI `search` 子命令（P3）
3. 增量搜索索引更新（P2）
4. OpenCode 集成 install-to-opencode 命令（P5）
5. 网络恢复后：清理未用依赖、切 SQLite FTS5、降 tokio features

## 五、Wayfinder 地图
- 地图文件：`.opencode/wayfinder/map.md`
- Ticket：P1 搜索接入 pipeline、P3 CLI search、P2 增量索引、P5 install-to-opencode