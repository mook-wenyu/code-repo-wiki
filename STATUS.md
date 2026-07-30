# 项目状态简报

## 一、架构健康度

- 当前模块总数：9（config, ingest, analysis, generate, output, incremental, model, search, lib）
- 违规跨模块调用：无
- 搜索层 8 个子模块全部集成入 pipeline
- 测试总数：126（118 单元 + 5 集成 + 3 快照），0 失败

## 二、本次变更范围

### 1. Spec 差距补全（最终轮——4 并行子代理）

| 模块 | 修改 | 状态 |
|------|------|------|
| scanner.rs | 二进制扩展名过滤（32 种扩展名） | ✅ |
| graph.rs | `detect_cycles()` tarjan SCC + 2 测试 | ✅ |
| lib.rs | 空仓库 `bail!("未找到任何源文件")` | ✅ |
| llm.rs | `complete_stream` 默认 + Provider 委托 | ✅ |
| prompt.rs | `entity_summary_prompt` 实体级提示 | ✅ |
| markdown.rs | architecture.md 固定输出路径 | ✅ |
| output/mod.rs | overview.md + _index.json + diagrams/ 子目录 | ✅ |
| .opencode/plugins/ | Slash 命令 + module_info + execa 异步 + 进度 | ✅ |
| impact.rs | `EdgeKind::Calls` 加入影响传播 | ✅ |
| lib.rs | 已删除文件 wiki/card 清理 | ✅ |
| tests/snapshot_test.rs | 3 知识卡片 JSON 模式快照测试 | ✅ |

### 2. 前期已完成（代码智能增强 + 死代码清理 + 流水线修复）

- 代码智能：AstQuery / CallGraph / FTS5 / Semantic / Hybrid / AstChunk / SearchAgent / Store 全部 8 模块
- 死代码清理：dependency.rs + unity.rs + map.rs + CLI 死命令 + 配置死变体 + 无用依赖
- CLI 修复：Generate --output / Custom provider / search 子命令
- 流水线修复：name_map→Vec / NodeId→file_path / Semaphore / SCC / BFS / ModuleDetector

## 三、已知风险点

- **持久化原子性**: state.rs save() 非原子写入
- **并发性**: 每次 run_pipeline 新建 tokio Runtime
- **Store.rs**: SQLite FTS5+vec0 实现完整但依赖 rusqlite/sqlite-vec（仓库已加，当前编译可达、测试通过）
- **增量清理**: 当前按 `!exists()` 判断，未利用 GitDiff deleted 列表

## 四、下次最该做的事

1. SearchAgent 暴露为 MCP 工具（`.opencode/plugins/repo-wiki.ts` 已有框架，需补 tool 注册）
2. 系统级集成测试（多语言混合仓库 + 全流水线 + 搜索验证 + 增量清理）
3. 提升测试覆盖率（目标 200+）
4. 增量清理改用 GitDiff deleted 列表替换 `!exists()` 判断
5. 性能基准测试（bench/ 目录）
