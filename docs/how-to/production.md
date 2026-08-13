# 运维指南：生产部署与验证清单

> 适用场景：把 code-repo-wiki 以「有网络 + LLM key 的常驻环境」投入日常使用的完整验收清单。
> 本文命令与常量均基于当前二进制核证（src/main.rs / src/bench / src/config）；需在有网络 + key 的环境实际执行（见第 10 节沙箱说明）。

## 0. 约定

- 产物目录默认 `.code-repo-wiki/`（`config.toml` 可覆盖 output 位置；本清单以默认值为例）。
- 二进制名统一 `code-repo-wiki`；以下命令默认在项目根执行。
- 相关文档：[CLI 命令参考](../reference/cli.md)、[配置参考](../reference/config.md)、[watch 托管](watch.md)、[lint 检查项](../reference/lint.md)、[限制项](../reference/limitations.md)。

## 1. 环境准备

### 1.1 doctor 六查全绿

```bash
code-repo-wiki doctor
```

六查项（src/main.rs:140 核证）：配置 / 产物可写 / 输出状态 / Key / 网络 / 版本漂移。全过退出码 `0`；任一失败退出码 `1`（输出失败项清单，逐项修复后重跑）。

### 1.2 配置 LLM key

| 变量 | 用途 | 必需 |
|---|---|---|
| `OPENCODEGO2_API_KEY` | 默认 LLM key（opencode.ai 网关） | 必需（无 key 生成 fail-fast） |
| `BAILIAN_API_KEY` | 默认 embed key（阿里百炼） | 可选（缺失语义搜索降级纯文本） |

写入方式：

- `code-repo-wiki key` 交互式配置（`--env` 用环境变量引用）；或
- 在 `config.toml` 写 `api_key_env = "OPENCODEGO2_API_KEY"`，key 放环境变量，仓库不写明文。

### 1.3 config.toml 的 model/base_url/api_key_env

以项目默认模板（`config.toml`）为例：

```toml
[llm]
provider = "openai"
model = "deepseek-v4-flash"
base_url = "https://opencode.ai/zen/go/v1"
api_key_env = "OPENCODEGO2_API_KEY"

[embed]
model = "qwen3.7-text-embedding"
api_key_env = "BAILIAN_API_KEY"
```

- `model`：LLM 模型名；`base_url`：API 端点，换服务商时两者配套改（默认模板显式声明 DeepSeek 端点，不写会把 DeepSeek 模型打到 OpenAI 端点）。
- `api_key_env`：引用环境变量名，key 不落盘明文；也可写 `api_key = "明文"`（仅建议用户级配置用）。
- `[llm] thinking`（可选）：DeepSeek 系默认启用 thinking，批量生成慢约 5×；对速度敏感可显式 `thinking = false`（bench 裁判调用同样适用，见 [bench 基线重录](benchmark.md)）。
- 已知问题：默认端点曾上游拒绝生成请求，已于 2026-08-10 实测恢复；再遇 400/500 的切换方案见 [配置参考的已知问题](../reference/config.md#已知问题)。

## 2. 首次生成

1. 选目标仓库：建议先用小仓库（千级源文件内）探成本，成本估算见第 8 节。
2. 执行 `code-repo-wiki generate`。
3. 确认退出码 `0`，且完成摘要无 `failed_modules`（失败模块非 0 时查 `.code-repo-wiki/.state/generation_state.json` 与网络连通性）。
4. 产物落 `.code-repo-wiki/`：`wiki/` 页面、`.search/text_index.db`（语义索引）、`.state/export_snapshot.json`（导出快照）。

## 3. 产物质量：lint

```bash
code-repo-wiki lint
```

- 期望退出码 `0` 干净；`1` 有问题 / `2` 配置或环境错误（CI 可直接用退出码）。
- **先 generate 再 lint**：lint 检查的是磁盘产物，产物过期会误报 `stale` / `bad-citation-overlap`（源码已变但文档未更新）。
- 已知噪声：`entity-coverage` 会把模块名引用判为不在实体清单（合成页按模块名引用属已知模式），人工复核按 [lint.md](../reference/lint.md) 排除。

## 4. 检索验证

```bash
code-repo-wiki search --query <真实符号> --engine hybrid --top-k 10
```

- 用目标仓库的真实符号（函数/结构体名）验证命中且带 score（text/semantic/hybrid 三引擎，默认 hybrid、默认 top-k 10）。
- 语义索引正常：`code-repo-wiki status` 的语义索引行无「已降级」提示（embed key 缺失会提示降级纯文本，属已知降级路径）。

## 5. MCP 验证

```bash
code-repo-wiki mcp
```

- 启动 MCP stdio server（Claude Code/Cline 接入；`install --claude` / `--codex` 可自动注册）。
- 五个只读 `wiki_` 工具：`wiki_status` / `wiki_search` / `wiki_ast_search` / `wiki_read_page` / `wiki_read_card`。
- 验收路径：`wiki_status` 确认已生成 → `wiki_search` 定位实体 → `wiki_read_page` 读对应模块页。

## 6. 导出 HTML

```bash
code-repo-wiki export
```

- 从产物渲染 HTML；已有产物时 `export --skip-generate` 从快照直接导出。

## 7. 增量维护与 CI 集成

- 增量：`code-repo-wiki update`（git diff / 文件指纹；`--dry-run` 预览不执行）。
- 常驻：`code-repo-wiki watch`（内置崩溃自愈循环）；三平台托管（Linux systemd / macOS launchd / Windows 任务计划程序）见 [watch 托管](watch.md)。
- CI 集成建议：
  - `lint` 作产物门禁（退出码 0/1/2 可直接用；生成后、提交产物前跑）。
  - `doctor` 作环境门禁（防 key 未配 / 版本漂移）。
  - 同一输出目录串行调用：状态/快照/缓存无锁，最后写入者胜（v36 起单实例运行锁防并发互踩）。
  - 与 watch / post-commit hook 并存无害（单实例锁保证并发不互踩），一般二选一。

## 8. 成本试算（从常量估算）

LLM 调用量量级（常量核证自 src/bench/mod.rs）：

| 维度 | 单模块/单页调用量 | 依据 |
|---|---|---|
| Doc Info | ≈1 调用/页 | LLM 判定每页一次 |
| TQS | ≈10–22 调用/模块 | AB/BA 各 5 轮（TQS_REPEATS=5），低置信升级 11 |
| Rubric | 3 生成 + 叶子×3–5 | RUBRIC_GENERATIONS=3，叶子判定 3 次（争议升级 5） |
| Update Recall | 最多 20 次全量生成 | MAX_RECALL_COMMITS=20，强制 mock provider（无 token 成本但耗墙钟） |

- 单次调用输出上限 `BENCH_MAX_OUTPUT_TOKENS=16384`。
- 小仓库探成本：先跑 `bench --judge --rubrics-only` 分离裁判层，再按需跑含回放的完整模式（大仓库回放是墙钟主导成本）。

## 9. 已知限制

- **单仓库本地定位**：无团队协作/权限/评审流程，产物随本地仓库演进。
- **依赖远程 API**：无 key 时 generate fail-fast；embed 缺失降级纯文本（`search`/`status` 显式提示）。
- **产物需纪律性更新**：漂移检测靠 doctor（版本漂移）/ lint（stale）+ 基于提交标记（`based_on_commit`）。
- **并发串行**（见第 7 节）；bench 的 judge 判定受 LLM-as-judge 稳定性影响（[限制项](../reference/limitations.md)）。

## 10. 沙箱说明

本清单涉及真实外部依赖：doctor 的网络检查、generate 的 LLM 调用、search 的语义索引、mcp 的接入。**必须在有网络 + LLM key 的环境逐项执行**；沙箱/离线环境无法代跑关键步骤。
