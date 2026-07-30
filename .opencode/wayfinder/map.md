# 地图：repo-wiki 演进为 OpenCode Agent 共享知识系统

## 目的地

将 repo-wiki 从 "CLI 工具 + 死代码" 变为 "持久化代码知识平台"：**所有 OpenCode Agent 实例共享的符号搜索 + 语义检索 + 调用追踪系统**。

验收标准：
- `repo-wiki search --query "auth"` 返回结构化结果
- `repo-wiki generate` 后自动构建搜索索引
- `repo-watch` 模式下文件变更自动更新索引
- `install-to-opencode` 将 repo-wiki 注册为 OpenCode MCP 工具

## Notes

- 已确认：允许引入需网络下载的新依赖（rusqlite bundled）
- 已确认：改为 SQLite FTS5 持久化（替换 bincode）
- 搜索引擎模块 (8 files, 892 lines) 已完成但未接入 pipeline — 核心工作
- 代码库已全面打开分析过，关键文件路径已知

## 决策记录

<!-- 本次 session 不关闭 ticket，创建后将记录在此 -->

## 未明确 (Fog of War)

1. **搜索配置**：`config.toml` 需要新增 `[search]` section 控制索引开关、路径和引擎选择
2. **OpenCode MCP 集成协议**：repo-wiki 以什么协议暴露搜索给 OpenCode？CLI subcommand + JSON 输出还是 HTTP server？
3. **SQLite 迁移成本**：bincode → SQLite FTS5 的迁移路径和兼容性策略
4. **共享知识系统多实例**：多个 Agent 实例如何共享同一个索引（文件锁 vs SQLite 并发）？

## 不包含

- 定义跳转/go-to-definition — LSP Server 已覆盖
- PDF 导出 — 非知识系统必需
- GitHub/GitLab 自动同步 — 独立功能
- 上游文档索引 — 效用/成本比低，需依赖爬虫
