# 配置参考

v30 起绝大多数选项已硬编码为合理默认（输出目录恒 `.code-repo-wiki`、增量恒监听模式、embed/search 恒开启、无 Key 自动降级），**空配置文件即可运行**。

## 配置文件层级与合并

项目级 `{root}/config.toml` 按**字段级合并**覆盖用户级配置（Windows `%USERPROFILE%\.code-repo-wiki\config.toml`，其他 `~/.code-repo-wiki/config.toml`），未写出的键继承默认。字段级合并语义与 uv/Claude Code/cargo 官方 merge 一致：表递归合并，未命中的键取默认，数组整体覆盖。

用户级目录解析优先级（v41 拍板，对齐 Codex/Claude Code 的 home 点目录惯例）：`CODE_REPO_WIKI_HOME` 环境变量（显式重定位）→ `%USERPROFILE%\.code-repo-wiki`（Windows）→ `~/.code-repo-wiki`。首次运行时自动从旧目录（`%APPDATA%\code-repo-wiki` / `~/code-repo-wiki`）**一次性迁移**（复制内容，旧目录保留不删）；`CODE_REPO_WIKI_HOME` 显式设置时不迁移。

## 全部可配键（注释式完整示例，与默认值一致，按需取消注释修改）

```toml
[wiki]
language = "zh"          # 文档语言（默认 zh）

[wiki.guide]             # 生成引导：只对 pages 列的模块生成独立页面，其余并入 overview（空 = 默认行为）
pages = []               # 模块路径前缀白名单，如 ["src/core"]；未匹配的模块不生成独立页但保留 overview 汇总
priority = []            # 模块生成顺序（确定性优先），如 ["src/core", "src/analysis"]
notes = []               # 注入模块页 prompt 的全局注意事项，如 ["本项目的 xx 约定…"]

[llm]
provider = "openai-compatible"  # openai / openai-compatible / anthropic / mock
model = "deepseek-v4-flash"     # 默认 opencode 网关 + deepseek-v4-flash
# base_url = "https://opencode.ai/zen/go/v1"
api_key = ""                    # 明文 key（仅建议用户级配置用；项目级请用下面 api_key_env 引用环境变量）
api_key_env = "OPENCODEGO2_API_KEY"  # key 放环境变量，仓库只写变量名

[embed]
model = "qwen3.7-text-embedding"      # 默认阿里百炼 + qwen3.7-text-embedding
# base_url = "https://llm-…maas.aliyuncs.com/compatible-mode/v1"
api_key = ""
api_key_env = "BAILIAN_API_KEY"       # embed key 缺失时语义搜索自动降级为纯文本
```

## 已知问题（2026-08 期间）

默认 LLM 端点 `https://opencode.ai/zen/go/v1` 曾出现上游临时拒绝——网关的 `chat/completions` 与 `responses` 生成端点返回 400/500（`/models` 列表正常但 Console Go 上游拒绝生成请求），首次 `generate` 会出现「Wiki 页面生成失败（API 应答错误）」且 `generation_state.json` 的 `failed_modules` 全模块失败。**已于 2026-08-10 实测恢复**（真实 `generate` 端到端成功，17 页产物）。若再遇 400/500：先重试一次（上游波动）；持续失败时切换兼容端点（阿里百炼，与上方 `[embed]` 同栈同 Key）：

```toml
[llm]
provider = "openai-compatible"
model = "qwen3.7-plus"   # 或 qwen-max / deepseek-v3 等百炼兼容模型
base_url = "https://llm-…maas.aliyuncs.com/compatible-mode/v1"
api_key_env = "BAILIAN_API_KEY"
```

替换 `…` 为你百炼控制台的应用专属 base_url。

## 源码扫描

无需配置：恒为全量遍历 + 四层内置边界自动过滤（`.gitignore`/内置噪音目录清单/支持语言自动识别/二进制与上限），非 Rust 仓库开箱即用。

## 迁移说明

旧版本配置项（`scope`/`output`/`plan` 段、`embed.enabled`、`incremental.strategy` 等）已删除或硬编码，残留键会被**静默忽略**，可安全删除。用户级配置目录沿革：v37 起 `%APPDATA%\code-repo-wiki\`；v41 起 `~/.code-repo-wiki`（home 点目录惯例），首次运行自动迁移旧目录内容（旧目录保留）；v37 改名（`%APPDATA%\repo-wiki` → `%APPDATA%\code-repo-wiki`）时不迁移，删除重装并重新配置 key。
