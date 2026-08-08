# Changelog

本文件记录 code-repo-wiki 的重要变更，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

## [Unreleased]

### Added
- 生产可用性审计改进（v33）：embedding 模型版本化——`.search/embed_model.json`
  持久化构建时模型名，增量路径检测到模型变化（含同维度模型升级与旧版索引
  标记缺失）回退全量重建语义索引，消除新旧向量混存导致的检索静默劣化
  （`embed_model_marker`/`read_embed_model`/`write_embed_model`/`embed_model_mismatch`，
  含单元测试）；
- 测试残留清理脚本 `scripts/cleanup-test-residue.ps1`（预览/确认两段式，
  清理 `%APPDATA%\code-repo-wiki\key-test-*` 与 `%TEMP%\repo_wiki*` /
  `code_repo_wiki*` 历史测试残留；v37 起新旧命名双清）；
- README 新增 watch 常驻进程托管模板（systemd 用户服务 / launchd
  LaunchAgent / Windows 任务计划程序，均含崩溃自愈配置）。

### Changed
- **项目改名 Code Repo Wiki（v37）**：crate/二进制/命令名 `repo-wiki` → `code-repo-wiki`（代码/测试/文档/MCP 注册键/git hooks 字面量/AGENTS 注入块全链路同步）；产物目录 `.repo-wiki` → `.code-repo-wiki`；用户级配置目录 `%APPDATA%\repo-wiki` → `%APPDATA%\code-repo-wiki`（改名不迁移，删除重装并重新配置 key）；GitHub 仓库改名 `mook-wenyu/code-repo-wiki`
- **CI 增强（v37）**：三 job 重构——check（clippy `-D warnings` + `cargo doc --no-deps` 门禁）、test（ubuntu + windows 矩阵，fail-fast:false）、lint-workflow（actionlint 校验 workflow）；新增 `rust-toolchain.toml`（stable + clippy）；concurrency cancel-in-progress
- **跨平台测试修复（v37）**：bench manifest 本地路径名提取改双分隔符（ubuntu 上 Windows 盘符路径不再被当相对路径）；key 测试全局配置目录注入临时路径 + opencode config_dir 测试纯函数化（消除并行 env 竞态——ubuntu 无 APPDATA 兜底必现）
- **文档重构（v37）**：docs/ 目录按 Diátaxis 组织（教程/CLI 参考/配置参考/FAQ/限制项/lint 检查项/架构说明/watch 托管/维护/术语表），README 瘦身为着陆页（原 22 节内容下沉 docs/）
- 原子写补齐 fsync：`write_file_atomic` 在 rename 前显式
  flush + `sync_all`（写句柄，兼容 Windows FlushFileBuffers），
  消除「已 rename 但内容截断」的断电窗口（salt 9c18c27 实证）。
- 检索改进（v36）：CJK 2-gram token 列（独立 `tokens` 列 + `PRAGMA user_version`
  表重建迁移，增量路径检测旧 schema 自动回退全量文本重索引）；默认引擎改
  `hybrid`（text/semantic/hybrid 三引擎共存，`--engine` 仍可指定）；调用链补全
  检索侧只扩展 callees（CodeRAG 实证 callers 方向 -17% MRR，展示侧仍双向）；
  图谱索引磁盘缓存（`.code-repo-wiki/.cache/call_index.json` + git HEAD/文件
  统计指纹）；rerank 端点按 dashscope compatible-api 格式实现后因用户拍板整体移除
- 运维改进（v36）：单实例运行锁（`.state/run.lock` 原子创建 + PID，并发启动
  显式报错）；watch 崩溃自愈循环（5s 起倍增上限 60s 退避重试）；git hooks
  失败可见性（stderr 落 `.code-repo-wiki/update-error.log` + 失败时提交输出
  提示一行）；`--also-claude` 合并进 `--claude`；status 新增 LLM 状态行；
  MCP 工具描述补「需先运行 generate」前置条件
- 搜索完备性审计（v35）：代码库与 llms.txt 自身两份 Agent 索引的完整性/新鲜度
  比对分析与差距清单（CJK 分词、rerank、错误路径一致性、评测覆盖等）
- 安装集成合并（v33）：`install-wiki`/`uninstall-wiki`/`install-to-opencode`/
  `uninstall-from-opencode` 四子命令删除，全部并入 `install`/`uninstall`；
  MCP 注册默认写用户级全局 `opencode.json`（OpenCode 原生格式，多仓库共享），
  `--claude` 可选写项目根 `.mcp.json`（Claude Code/Cursor），`--codex` 可选写
  `~/.codex/config.toml`（Codex）；已存在集成产物带 code-repo-wiki 标记则升级覆盖
  （插件文件内容比对、hooks 标记判定），无标记的人工产物保留并提示；
  `install` 默认注入 AGENTS.md 引导块（`--also-claude` 同步 CLAUDE.md）；
  `uninstall` 无 flag 幂等清理全部 code-repo-wiki 集成痕迹
- 傻瓜式自动化配置（v30）：`output.dir`（恒 `.code-repo-wiki`）、`embed.enabled`
  （恒 true）、`search.enabled`（恒 true）、`incremental.enabled`/
  `incremental.strategy`（恒 FileWatch 监听模式）全部硬编码为代码常量，
  配置文件不再需要也不接受这些键；`expand_languages` 扩展语言删除（只输出
  主语言）
- plan 功能整体删除（v30）：`wiki_plan.yaml`、`plan.path` 配置、plan 驱动的
  页面白名单/模块规划全部移除——生成范围完全由扫描结果自动决定，无需人工干预
- 语义索引失败降级（v30）：embedding 运行期失败（Key 缺失/网络不可达）不再
  `?` 中断主流程，改为告警+保留旧索引（与初始化失败同语义）
- 非 Git 仓库 FileWatch 增量修复（v30）：实体变化分类空集（非 Git/无上次
  提交）时保守保留全部变更起点，不再把 changed_files 全部剔除导致 0 模块
- 扫描零配置（v30）：`scope.include/exclude` 删除，源码扫描恒为全量遍历 +
  四层内置边界（.gitignore/噪音目录/支持语言扩展名自动识别/二进制与上限）；
  非 Rust 仓库（JS/Go/Python 等）开箱即用
- 配置净化机制整体删除（v30）：项目级 `config.toml` 任意键原样生效（无注入
  无剥除），缺失键由 schema 内置默认兜底——傻瓜式即写即用

### Performance
- 实体摘要 LLM 调用删除（v31 C-01）：`Entity.summary` 字段与逐实体摘要
  生成整体移除（零消费者），生成 Token 显著下降
- 模块职责描述缓存（v31 C-02）：同一模块描述仅调用一次 LLM（架构页/概览页
  共享），并按模块文件指纹落盘缓存（`.code-repo-wiki/.state/module_descriptions.json`），
  增量轮零 Token 复用
- watch 冷却窗口（v31 C-07）：连续编辑（IDE 自动保存/批量重构）期间合并事件，
  安静 2s 或首个事件后 5s 触发一次合并增量，避免 N 次保存触发 N 次全量管线

### Fixed
- 增量空 chunk 误记失败模块（v31 C-03）：空 chunk（无实体可生成）不再写入
  `failed_modules`——此前污染会令 no-op 快速跳过永久失效，并触发无关模块
  的补偿重试

### Added
- RepoDocBench 对齐五维评测（v32 6.4）：`bench --repodoc` 输出五维聚合报告
  （Coverage/Doc Info/Completeness@K/TQS/Update Recall，对齐 RepoDoc 论文
  评测维度）；各维 LLM 不可用/索引缺失等场景显式降级标注（FR-101 不静默）
- Completeness@K 可检索性维度（v32 6.3）：实体名检索 text 索引 top-10，命中
  判定按「实体模块 == 索引条目模块（均按文件父目录派生）且模块页存在」，
  修正评审发现的实体侧与索引侧模块派生不一致（生产路径恒 0 命中）
- judge 三态协议（v32 6.1）：rubric 判定新增 `uncertain`（证据不足弃权），
  重试一次仍不确定记 abstain；TQS 平局率（tie_rate）超过 0.30 触发升级为
  更多裁判轮次
- Doc Info LLM 判定（v32 6.2）：按页 0-10 信息性评分（LLM 可用时），
  证据不足 uncertain 弃权，LLM 不可用降级为纯文本统计

## [0.3.0] - 2026-08-05

### Changed
- 配置链三合一（v25）：init/install-to-opencode 合并为 install（确保用户级默认配置 + 插件/MCP/hooks 注册）；项目级配置统一为 `config.toml`，字段级合并覆盖用户级 `config.toml`（数组整体覆盖）；旧文件名 `.code-repo-wiki.toml` 与旧全局 `config.toml` 停用
- 净化边界收窄（v25）：provider/model 允许项目级覆盖（协议/模型无凭据泄露面，mock 是 CI 常态），base_url/api_key_env 仍净化（端点劫持/凭据泄露防护）
- 默认配置模板协议统一（v25）：provider openai-compatible → openai（Responses 协议，DeepSeek 官方推荐）



### Changed
- 配置分层重构（v24）：项目级配置迁移为独立文件 `.code-repo-wiki.toml`（与产物目录 `.code-repo-wiki/` 物理分离）；install 命令不再在项目级自动创建配置文件（自动创建只发生在用户级目录）；项目级配置执行敏感键净化（Codex DENYLIST 模式：`llm.provider/model/base_url/api_key_env`、`embed.model/base_url/api_key_env` 被忽略并告警，凭据/提供商/模型归属用户级配置或 `--config` 显式指定）

### Added
- llms-full.txt 完整内容索引（v19 t05）：内联全部实体摘要，32K token 预算内四档裁剪（完整 → 去常量 → 去无定位 → 精简），头部含生成版本行
- 版本自检（v19 t01）：doctor 新增「版本」检查（状态记录 vs 当前二进制版本漂移提示）；llms.txt / llms-full.txt 头部注入生成版本
- update no-op 快速判定（v19 t06）：git HEAD + 工作树状态 + 产物存在三重判据，无变更时秒级跳过（含 watch 免费空转）
- CI 模板（v19 t02）：.github/workflows/ci.yml（build + test + clippy -D warnings）

### Changed
- 社区命名稳定重排序（v19 t07）：社区按大小降序编号（Graphify 模式），新增小社区不扰动既有大社区编号
- lint 实体噪声过滤（v19 t03）：单字符/纯数字实体名（LLM 编造噪声）两侧同口径忽略；graph 新增 mod 实体类型（NodeKind::Module）
- 文档与配置模板统一（v19 t02）：init 模板 embed 段统一 text-embedding-3-small + OPENAI_API_KEY（消除三方失实）；README 补齐 init 全局链/lint 三态/dry-run/doctor/子命令表

### Fixed
- 测试产物泄漏隐患根治（v19 t04）：测试配置统一绝对路径（helper 强制临时目录）
- 删除文件场景修复（v22）：多文件模块删除部分文件时，幸存文件并入正常 LLM 路径重生成（此前回填旧文档残留被删实体）；IncrementalResult 新增 has_deleted_files 信号，索引/全局文档在删除场景正确重建
- lint 实体提取修复（v22）：剥离继承段/泛型参数/属性宏段后再取签名（此前 `internal class Foo : ScriptableObject` 误提取 ScriptableObject 等，C# 仓库 stale-entity 虚高）；lint 扫描根目录 root 化（--root 指向其他仓库时不再扫当前目录，Unity 实测 stale-entity 1000→13）

### Changed
- 配置项硬编码简化（v22）：10 项低频配置移入代码常量（src/config/schema.rs 顶部单一真源）——llm.max_concurrent=16 / llm.max_tokens / llm.temperature（模型默认）/ embed.batch_size=20 / search.index_dir=.search / search.default_engine=text / search.default_top_k=10 / search.rrf_k=60.0 / incremental.max_depth=3 / plan.path=wiki_plan.yaml；schema 与 install 模板同步删键，旧配置残留键被 serde 忽略可安全删除
- 卡片写盘与页面生成解耦（v22）：Knowledge Card 独立全量落盘，不再绑定页面成功（Unity 实测 10 个模块页 LLM 失败时卡片一并丢失，产出「快照/_index 有、磁盘无」的不一致）；卡片只按主语言写盘
- 失败补偿重试（v22）：生成失败的模块（failed_modules）写入状态，下次 update 并入变更集补生成，no-op 快速判定不再跳过含失败模块的仓库
- LLM 调用级重试（v22）：Wiki 页面调用瞬时网络错误（连接重置/超时/5xx）自动退避重试 3 次（此前直接失败丢页，长任务中静默丢 10+ 页面）

## [0.2.0] - 2026-08-04

### Added
- LLM Provider 协议拆分（v17 t02）：provider = openai（Responses API，base_url 可配，DeepSeek 归此）/ openai-compatible（chat/completions，custom 并入）；Responses 端点不支持（404/400）自动回退 chat/completions；旧配置 provider = custom 需改 openai-compatible
- doctor 诊断命令（v17 t04）：六查（配置可解析/产物目录可写/输出目录状态/LLM Key 引导/网络探活/版本漂移），mock provider 跳过网络查
- update --dry-run 预览（v17 t03）：列出变更文件与受影响模块，零副作用
- init 缺省链保护（v17 t01）：默认配置链已存在时跳过不覆盖，--force 强制重写，显式路径保持覆盖
- mock 占位页页脚（v17 t03）：mock provider 生成的页面注入「非真实内容」标记
- lint 三态退出码（v17 t03）：通过 0 / 检出问题 1 / 配置失败 2（CI 可直接消费）
- 生产就绪 P0-P2（v16）：README 安装段与 Linux/macOS 前置依赖声明、插件 PATH 绝对路径注入（install 时绑定 current_exe，不再依赖 PATH）
- 评测闭环（v14 C 组）：Rubric 层级完整性维度 7（CodeWikiBench 协议：docs_tree → 3 次独立生成 + 1 次合并 → 叶子 0/1 判定 → 加权聚合 S±σ+coverage）；TQS 补机会校正 Cohen's κ、位置偏差 |P(A胜)−0.5|、低置信模块清单
- 语义 lint（v14 D 组）：LLM 跨页矛盾检查（变更驱动、单次调用合并多页、失败只告警）
- llms.txt 导出（v14 E 组）：Agent 站点地图（确定性生成，render_all 同步写）
- watch Ctrl-C 优雅退出（v14 F 组）：stop_flag + 500ms 轮询，等当前增量生成完成后退出
- 引用机械校验（v14 B 组）：区间重叠两级校验（文件级 + 实体区间覆盖），lint 新检查 bad-citation-overlap
- 全局配置链（v13 E 组）：默认配置搜索链 项目级 → 全局（Windows %APPDATA%/code-repo-wiki，其他 ~/code-repo-wiki）→ 创建全局
- 符号漂移检查（v13 D 组）：api.md 清单实体与当前源码 AST 对比（stale-entity）
- LLM 生产路径统一流式（v13 A 组）：complete 默认实现委托 complete_stream，移除客户端总超时（SSE 60s 空闲超时保护）

### Changed
- 解析失败统计（v13 B 组）：ScanOutput{insights, files_failed}，AnalysisStats 新增 files_failed
- Mermaid 依赖图/调用图确定性排序（v13 A 组）
- 首次 update 无 git 基线时回退全量生成（v13 A 组）
- lint 检查数 3 类 → 7 类（孤儿/断链/过时/引用/实体覆盖/Mermaid/符号漂移）

### Fixed
- 增量语义索引入口静默失败（v14 A 组）：EmbeddingEngine/数据库打开失败显式告警
- 状态文件读取/保存失败静默吞错（v13 A/B 组）
- 文档指纹读取失败保守计入保护集（v13 A 组）
- test_cli mock LLM server 未按 SSE 格式响应导致卡片内容为空（v13 A4 连带回归）

## [0.1.0] - 2026-08-02

初始版本：AI 驱动的代码仓库 Wiki 自动生成工具（全量/增量生成、多引擎搜索、
文件监听、HTML 导出、OpenCode 插件、MCP server、评测基准）。
