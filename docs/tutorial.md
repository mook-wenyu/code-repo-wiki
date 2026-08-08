# 教程：第一次生成 Wiki

本教程带你从零开始，为一个代码仓库生成持续更新的 Wiki 文档。全程约 10 分钟（不含首次 LLM 生成时间）。

## 前置条件

- **Rust 工具链**（含 `cargo`，用于源码安装；发布 crates.io 后可直接 `cargo install code-repo-wiki`）
- 目标仓库可以是**任何语言**的项目（解析器覆盖见[限制项](reference/limitations.md)），不要求是 Rust 仓库，也不要求是 git 仓库

## 第 1 步：安装

```bash
cargo install --path . --locked
```

> 从本仓库源码构建。装完验证：`code-repo-wiki --version`。

## 第 2 步：配置 LLM API key（可选但推荐）

**不配置也能跑**：无 key 时 LLM 自动降级为本地模拟内容（模板化页面），语义搜索降级为纯文本，全流程不中断。但真实 Wiki 内容需要 LLM key：

```bash
export OPENCODEGO2_API_KEY="sk-..."
```

`code-repo-wiki key` 命令可交互式写入用户级配置（或 `--env` 用环境变量引用）；完整配置说明见[配置参考](reference/config.md)。

> ⚠️ 2026-08 实测：默认端点 `https://opencode.ai/zen/go/v1` 当前不可用（网关生成端点返回 400/500）。需要按[配置参考的已知问题段](reference/config.md#已知问题)改用百炼等兼容端点。

## 第 3 步：生成 Wiki

```bash
cd /path/to/your/repo
code-repo-wiki generate
```

零参数全自动：扫描源码 → 知识图谱 + 社区检测划分模块 → LLM 生成知识卡片与文档页。产物在 `.code-repo-wiki/wiki/zh/`（模块页 + `api.md` + `architecture.md` + `overview.md`）+ `.code-repo-wiki/cards/` + `.code-repo-wiki/llms.txt`。

生成后立刻能看的：

```bash
code-repo-wiki status     # Wiki 状态总览
code-repo-wiki search --query "你的函数名" --engine hybrid
code-repo-wiki lint       # 产物健康检查（断链/过时/引用错位）
```

## 第 4 步（推荐）：一键全自动

```bash
code-repo-wiki install
```

注册 git `post-commit` / `post-merge` hook——之后**每次 commit 后 Wiki 自动增量更新**，无需再手动执行任何命令。install 还同时：写用户级默认配置、注册 OpenCode 全局 MCP / 插件、注入 AGENTS.md 引导块。卸载用 `code-repo-wiki uninstall --force`。

常驻实时模式（代码保存即更新）用 `code-repo-wiki watch`，托管方案见[watch 托管](how-to/watch.md)。

## 日常使用

| 场景 | 命令 |
|---|---|
| 改代码后更新文档 | 什么都不用做（commit 自动触发）；手动跑 `code-repo-wiki update` |
| 新增/删除了文件 | `code-repo-wiki update`（实体级变化分类驱动语义传播） |
| 想看文档状态 | `code-repo-wiki status` |
| 搜索实体 | `code-repo-wiki search --query "k" --engine hybrid` |
| 导出 HTML 站点 | `code-repo-wiki export` |
| 环境体检 | `code-repo-wiki doctor` |

## 下一步

- [配置参考](reference/config.md) —— 覆盖语言/模型/引导
- [命令参考](reference/cli.md) —— 全部子命令
- [FAQ](reference/faq.md) —— 常见问题
