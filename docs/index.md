# Code Repo Wiki 文档

自动为代码仓库生成**持续更新**的 Wiki 文档：模块页、API 参考、知识卡片，供人和 AI 助手阅读。

零配置文件 · 单二进制 · 支持 11 种语言 · 增量更新 · 可注册为 OpenCode 插件 / MCP server

## 从这里开始

| 你想做什么 | 去这里 |
|---|---|
| 第一次上手（安装 → 配置 → 生成 Wiki） | [教程](tutorial.md) |
| 查某个命令怎么用 | [CLI 命令参考](reference/cli.md) |
| 改配置（LLM/embed/语言/引导） | [配置参考](reference/config.md) |
| 了解内部如何工作（解析/图谱/增量/搜索） | [架构说明](explanation/architecture.md) |
| 让 watch 常驻、托管开机自启 | [运维指南：watch 托管](how-to/watch.md) |
| 清理测试残留、发布新版本 | [运维指南：维护](how-to/maintenance.md) |
| 排查「文档过时/断链/编造」 | [lint 检查项](reference/lint.md) |
| 边界条件与已知限制 | [限制项](reference/limitations.md) |
| 快速问答 | [FAQ](reference/faq.md) |
| 术语速查（模块/卡片/快照/保护集…） | [术语表](glossary.md) |

## 项目根

- [README](../README.md) —— 30 秒了解 + 快速开始
- [CHANGELOG](../CHANGELOG.md) —— 版本变更记录
- [GitHub 仓库](https://github.com/mook-wenyu/code-repo-wiki) —— 问题与 PR

## 文档约定

- 所有命令示例的二进制名统一为 `code-repo-wiki`（v37 改名前的 `repo-wiki` 仅存在于历史 CHANGELOG 条目中）。
- 产物目录示例统一为 `.code-repo-wiki/`（v37 起），历史文档中的 `.repo-wiki/` 不再有效。
