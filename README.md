# repo-wiki

自动为代码仓库生成**持续更新**的 Wiki 文档：模块页、API 参考、知识卡片，供人和 AI 助手阅读。

零配置文件 · 单二进制 · 支持 11 种语言 · 增量更新 · 可注册为 OpenCode 插件 / MCP server

---

## 它能做什么（30 秒了解）

输入一个代码仓库，`repo-wiki generate` 产出 `.repo-wiki/` 目录：

- 分析源码结构（tree-sitter AST + 知识图谱 + 社区检测自动划分模块）
- 让 LLM 为每个模块生成知识卡片与文档页（API 参考含**真实文件与行号**）
- 之后每次 `git commit` 自动增量更新（只重写受影响的模块页）

真实产物示例（本项目自己生成的 `.repo-wiki/wiki/zh/api.md`）：

```markdown
## analysis

- `pub fn detect_communities(graph: &KnowledgeGraph) -> Vec<Vec<NodeId>>`（模块）· 文件级社区检测：
  构建 File 节点弱连通分量，每簇一个 Vec<NodeId> · src\analysis\community.rs:54
- `MIN_DIRS_FOR_SUPERNODE`（常量） · src\analysis\community.rs:90
```

## 快速开始

> 前置条件：需要 Rust 工具链（含 `cargo`）；目标仓库可以是**任何语言**的项目（解析器见[限制项](#限制项)），不要求是 Rust 仓库，也不要求是 git 仓库。

```bash
# 1. 安装（源码构建；发布 crates.io 后可直接 cargo install repo-wiki）
cargo install --path . --locked

# 2. 配置 LLM API key（不配也能跑：自动降级为本地模拟内容）
export OPENCODEGO2_API_KEY="sk-..."

# 3. 在目标仓库里生成 Wiki（零参数全自动，产物在 .repo-wiki/wiki/zh/）
cd /path/to/your/repo
repo-wiki generate
```

**一键全自动（推荐）**：`repo-wiki install` 注册 git `post-commit`/`post-merge` hook——之后**每次 commit 后 Wiki 自动增量更新**，无需再手动执行任何命令。常驻实时模式用 `repo-wiki watch`（代码保存即更新）。卸载用 `repo-wiki uninstall --force`。

## 核心功能

| 功能 | 说明 |
|---|---|
| 代码解析 | tree-sitter：Rust/TypeScript/TSX/Python/Go/JS/JSX/MJS/CJS/C#/Java 共 11 种 |
| 模块划分 | petgraph 知识图谱 + leiden-rs 社区检测，自动发现模块边界 |
| Wiki 生成 | LLM 生成知识卡片 + 模块页 + API 参考，引用真实文件/行号 |
| 增量更新 | 实体级变化分类（新增/删除/签名变更/正文修改）驱动语义传播，只重生成受影响模块；基于内容指纹，**非 Git 仓库同样支持** |
| 搜索 | BM25 全文 + 向量语义 + RRF 混合排序；另有 `ast-search` 精确符号查找 |
| 文件监听 | `watch` 常驻监听，保存即更新 |
| HTML 导出 | `export` 一键导出静态 HTML 站点 |
| AI Agent 友好 | 自动生成 `llms.txt`/`llms-full.txt`（Agent 索引）、AGENTS.md 引导块；可注册为 OpenCode 插件 / MCP server |

## 配置：默认零配置，需要时只写要覆盖的键

v30 起绝大多数选项已硬编码为合理默认（输出目录恒 `.repo-wiki`、增量恒监听模式、embed/search 恒开启、无 Key 自动降级），**空配置文件即可运行**。项目级 `{root}/config.toml` 按字段级合并覆盖用户级配置（Windows `%APPDATA%\repo-wiki\config.toml`），未写出的键继承默认。下面是全部可配键的注释式完整示例（与默认值一致，按需取消注释修改）：

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

> ⚠️ **已知问题（2026-08 实测）**：默认 LLM 端点 `https://opencode.ai/zen/go/v1` 当前不可用——网关的 `chat/completions` 与 `responses` 生成端点返回 400/500（`/models` 列表正常但 Console Go 上游拒绝生成请求），首次 `generate` 会出现「Wiki 页面生成失败（API 应答错误）」且 `generation_state.json` 的 `failed_modules` 全模块失败。**临时方案**：把 `[llm]` 段改为实测可用的兼容端点（阿里百炼，与上方 `[embed]` 同栈同 Key）：

```toml
[llm]
provider = "openai-compatible"
model = "qwen3.7-plus"   # 或 qwen-max / deepseek-v3 等百炼兼容模型
base_url = "https://llm-…maas.aliyuncs.com/compatible-mode/v1"
api_key_env = "BAILIAN_API_KEY"
```

替换 `…` 为你百炼控制台的应用专属 base_url。默认端点修复后此说明会移除。

源码扫描**无需配置**：恒为全量遍历 + 四层内置边界自动过滤（`.gitignore`/内置噪音目录清单/支持语言自动识别/二进制与上限），非 Rust 仓库开箱即用。

> 旧版本配置项（`scope`/`output`/`plan` 段、`embed.enabled`、`incremental.strategy` 等）已删除或硬编码，残留键会被静默忽略，可安全删除。

## 命令参考

| 命令 | 用途 |
|---|---|
| `generate` | 全量生成 Wiki（`-o` 输出覆盖、`--force` 强制重写、`--progress-json`） |
| `update` | 增量更新（git diff / 文件指纹；`--dry-run` 预览不执行） |
| `watch` | 监听文件变更自动更新 |
| `sync` | 以 Git 工作区内容同步指纹库（不触发 LLM） |
| `status` | 查看 Wiki 状态 |
| `lint` | 产物健康检查（断链/过时/引用错位/坏 mermaid）；退出码 0/1/2 供 CI |
| `export` | 导出 HTML（`--skip-generate` 从快照直接导出） |
| `doctor` | 环境诊断（配置/产物/输出/Key/网络/版本六查） |
| `key` | 交互式配置 LLM API key（用户级，`--env` 用环境变量引用） |
| `install` | 一键集成：用户级默认配置 + OpenCode 全局 MCP/插件 + AGENTS.md 引导 + git hooks（`--claude` 加 .mcp.json、`--codex` 加 Codex 配置、`--also-claude` 同步 CLAUDE.md） |
| `uninstall` | 移除全部集成（MCP 条目/插件/引导块/hooks；需 `--force`） |
| `search` | 搜索代码实体（text/semantic/hybrid 三引擎；`--query/--engine/--top-k`） |
| `ast-search` | AST 精确符号查找（文件+行号+签名） |
| `card` | 知识卡片操作（generate/modify/supplement/rewrite） |
| `note` | 知识沉淀记录（追加到 `_log.md`） |
| `mcp` | 启动 MCP stdio server（Claude Code/Cline 接入） |
| `bench` / `bench-manifest` | 文档质量评测 / 清单批量跑分 |

除 `bench` 外全部子命令支持 `--root <路径>` 指定项目根（默认当前目录）；每个命令 `--help` 有详细用法。

## 常见问题（FAQ）

| 问题 | 回答 |
|---|---|
| 没有 API key 能跑吗？ | 能。无 key 时 LLM 降级为本地模拟、语义搜索降级为纯文本，全流程不中断 |
| 产物在哪里？ | `.repo-wiki/`：`wiki/{lang}/` 文档页、`cards/` 知识卡片、`llms.txt` Agent 索引 |
| 手动改过文档会被覆盖吗？ | 不会。人工修改过的页面自动加入保护集（SHA256 指纹），后续更新跳过；`generate --force` 清空保护集 |
| 文档过时了？ | `repo-wiki lint` 检查断链/过时/引用错位；`doctor` 检测二进制与产物版本漂移 |
| 大仓库跑得动吗？ | 单项目上限 10 万个源文件；单次变更超 1 万行自动回退全量生成；16 路并发 LLM 调用（详见[限制项](#限制项)） |
| 会泄露我的代码/密钥吗？ | 只把模块内实体清单/文件路径发给 LLM（不发全文件）；API key 只从环境变量或用户级配置读取，项目级配置写 `api_key_env` 变量名而非明文 |

## 架构

```
Source Files → ingest (tree-sitter AST 解析) → analysis (知识图谱 + 社区检测)
            → generate (LLM 卡片 + 文档) → output (Markdown/HTML) → Wiki/
            → search (FTS5 + embedding 索引) → text/semantic/hybrid 搜索
```

## 限制项

- 同一输出目录并发运行不被支持（状态/快照/缓存无锁，最后写入者胜；CI/编辑器集成请串行调用）
- 每种语言都独立生成会线性放大 LLM 成本——当前只输出主语言（`wiki.language`，默认 `zh`）
- **大仓边界**：单项目上限 10 万个源文件（超出显式报错）；单次变更超过 1 万行自动回退全量生成。实测（mock LLM，v30）：cal.com（5048 文件/5.9 万实体）全量约 6.2 分钟；图构建在万级实体仓库是主要耗时（v32 增量索引优化后 287s→20s）；真实 LLM 成本随仓库规模线性放大
- **评测边界**：`bench --judge` 是参考型口径（文档生成质量自评），判定受 LLM-as-judge 稳定性影响（judge 三态 + abstain + tie 升级阈值已缓解，flip 率已知项）；rubrics 全量打分仅作趋势参考，不承诺与人工评审一致
- **语义搜索降级**：embed Key 缺失或运行期失败时自动降级为纯文本搜索，`search`/`status` 显式提示「语义索引已降级」；降级状态持久化到 `semantic_degraded` 标记，下次成功生成自动清除
- **语言覆盖**：tree-sitter 解析支持 Rust/TypeScript/TSX/Python/Go/JS/JSX/MJS/CJS/C#/Java 11 种；**无 Ruby/PHP 解析器**（rails 等 Ruby 仓库只能解析其 JS/TS 资产；纯非支持语言仓库零源文件会显式报错「未找到任何源文件」——扫描是全量自动识别，无 include 白名单配置）

## 进阶与运维

### watch 常驻进程托管（可选）

`watch` 进程崩溃后不会自动重启——需要「常驻 + 崩溃自愈」时按平台托管（watch 启动时先全量生成再监听，重启后自动收敛，不会损坏产物）：

- **Linux（systemd 用户服务）**：`~/.config/systemd/user/repo-wiki-watch.service`
  ```ini
  [Unit]
  Description=repo-wiki watch daemon
  [Service]
  ExecStart=/absolute/path/to/repo-wiki watch
  WorkingDirectory=/absolute/path/to/project
  Restart=on-failure
  RestartSec=5
  [Install]
  WantedBy=default.target
  ```
  `systemctl --user enable --now repo-wiki-watch`
- **macOS（launchd LaunchAgent）**：`~/Library/LaunchAgents/com.repo-wiki.watch.plist`
  ```xml
  <plist version="1.0"><dict>
    <key>Label</key><string>com.repo-wiki.watch</string>
    <key>ProgramArguments</key>
    <array><string>/absolute/path/to/repo-wiki</string><string>watch</string></array>
    <key>WorkingDirectory</key><string>/absolute/path/to/project</string>
    <key>KeepAlive</key><true/>
  </dict></plist>
  ```
  `launchctl load ~/Library/LaunchAgents/com.repo-wiki.watch.plist`
- **Windows（任务计划程序）**：登录触发 + 失败重启（`RestartCount` 与 `RestartInterval` 需同时设置）
  ```powershell
  $action  = New-ScheduledTaskAction -Execute 'D:\path\to\repo-wiki.exe' -Argument 'watch' -WorkingDirectory 'D:\path\to\project'
  $trigger = New-ScheduledTaskTrigger -AtLogOn
  $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
  Register-ScheduledTask -TaskName 'repo-wiki-watch' -Action $action -Trigger $trigger -Settings $settings
  ```

### 测试残留清理

历史测试可能遗留无意义的临时目录（`%APPDATA%\repo-wiki\key-test-*` 与 `%TEMP%\repo_wiki*`，≈0B 占用、无敏感数据）。需要清理时：

```powershell
powershell -File scripts/cleanup-test-residue.ps1          # 先预览
powershell -File scripts/cleanup-test-residue.ps1 -Apply   # 确认后删除
```

## lint 检查项

`repo-wiki lint` 对磁盘上的产物做静态健康检查（对齐 LLM Wiki 最佳实践：Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）。退出码：`0` 干净 / `1` 有问题 / `2` 配置或环境错误（CI 可用）。

| kind | 含义 | 发射点 |
|---|---|---|
| `orphan` | 孤儿页：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复） | `src/output/lint.rs` |
| `broken` | 断链：页面内链接指向不存在的产物文件 | 同上 |
| `stale` | 过时：页面时间戳早于源文件修改时间（源码已变文档未更新） | 同上 |
| `bad-citation` | 正文 `path:line` 引用指向不存在的文件或行号越界（引用契约的静态复核，3 个发射点） | 同上 |
| `bad-citation-overlap` | 行号对但内容错：引用行区间与实体表行区间不重叠 | 同上 |
| `bad-vctx` | 正文 `[[vctx:path#L-a-L-b@hash8]]` 手工标记 5 步哈希只读校验失败（vericontext 协议，5 个发射点） | 同上 |
| `entity-coverage` | 页面声称的实体不在 api.md 权威清单（LLM 编造的第二道闸） | 同上 |
| `stale-entity` | api.md 权威清单的实体在当前源码中不存在（文档引用了已删除/重命名的符号） | 同上 |
| `bad-mermaid` | 产物中的 mermaid fence 无法被 merman 解析（历史产物/人工编辑/增量遗留） | 同上 |

已知噪声：`entity-coverage` 会把**模块名引用**（api.md 的 `##` 节标题）判为不在实体清单——合成页（architecture/overview）按模块名引用属已知模式，不是 LLM 编造；人工复核时按此排除即可。检查对象是磁盘产物（真实用户看到的东西），而非内存中的文档对象。

## 维护者

- **搜索**：`repo-wiki search --query "k" --engine hybrid --top-k 10`；语义索引无 Key 自动降级
- **AI Agent 入口**：`llms.txt`（站点地图）+ `llms-full.txt`（含实体签名的内联索引），随生成确定性重写
- **发布**：SemVer 更新 `Cargo.toml` → `cargo test`/`clippy`/`machete` 全绿 → `cargo publish` + `git tag vX` → 干净环境 `cargo install repo-wiki` + `doctor` 六查
- **评测**：`repo-wiki bench --root <repo> [--judge]` 自动评测（Coverage/文档信息/Completeness@K/lint/Update Recall/耗时 + LLM-as-judge 打分）；`bench --repodoc` 输出 RepoDocBench 对齐五维聚合报告（Coverage/Doc Info/Completeness@K/TQS/Update Recall，LLM 不可用等降级显式标注）；`bench-manifest` 清单批量跑分
