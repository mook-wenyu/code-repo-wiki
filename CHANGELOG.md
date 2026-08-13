# Changelog

本文件记录 code-repo-wiki 的重要变更，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

## [0.8.0] - 2026-08-14

### Added
- **search 检索质量四件套**：真混合检索（向量+全文）、RRF 融合 k=20、结构感知分块、纯远程嵌入（删除本地 fastembed 路径）——远程嵌入默认收敛，本地嵌入不再维护
- **search 索引加固**：块去重 + vecdb/query cache 加固 + Java 语言支持 + FTS5 UNINDEXED/bm25 排名
- **generate 项目级上下文**：注入依赖（Imports∪Calls）与调用方摘要、token 预算、依赖防幻觉校验（fence）、`design_rationale` 意图段、调用方边归属
- **lint severity 化**：`severity` 字段 + entity-ownership 归属校验 + citation_density + dependency-fabricated 磁盘级接线 + 结构性/结构化误报修复 + stale 指纹化与反向定位（warning 级不阻断 CI）

### Changed
- **incremental 变更检出**：BodyChanged 沿 Calls 深度 1 传播、`should_fallback_full` 回退保护、doc_comment 变更检出、diff 死字段清理
- **测试适配**：integration 适配新签名/字段、body 传播断言更新（http 页刷新）、orphan 降 warning 行为断言

### CI
- **产物 lint 门禁**：artifacts.yml 产物 lint 门禁 + 真实产物生成工作流；EOL 统一（历史 CRLF 归一为 LF）
- **生产文档**：生产部署/bench 基线重录 runbook + manifest 模板

## [0.7.1] - 2026-08-13

### Fixed
- **增量 update 回填未改动模块文档集**：增量生成路径对未重生成模块执行文档回填（`backfill_unchanged_modules`），`llms/_toc` / `export_snapshot` 不再丢失未重生成模块
- **entity-coverage 用源码现实校验**：lint 改用真实子目录名与路径引用做实体覆盖校验——真实子目录名/路径引用不误报，真编造仍报

### Changed
- **CI 加固**：ci.yml 加权限最小化 / fmt 门禁 / NO_PROXY / local-embed；release.yml 修 release-id 死配置、权限按 job 最小化、NO_PROXY、publish-dry-run 前置；actions SHA pin
- **chore(fmt)**：cargo fmt 全仓格式化（CI fmt 门禁前置）

## [0.7.0] - 2026-08-13

### Changed
- **Phase A7 全面审计与实现**：路径安全单点守卫（`..` / 根相对 / 盘符相对 / 绝对越 root）+ source-missing 缺失源文件检查
- **libgit2 Windows 竞态 flaky 修复**：`test_git::commit_all` 公共 git 提交 helper 统一有界重试
- **配置**：插件 configPath 指向项目配置 / validate_config 落地 / 密钥权限收紧 / key `--env` 对齐 / fixture 清理
- **生成引擎**：describe_modules 不静默吞错 / MockProvider 分流 / 并发语义统一 / scanner 大小上限 / 预算接线 / 死代码清理 / run_generation_filtered 拆分
- **降级路径**：骨架降级 fail-fast / Mermaid 显式标注 / Responses 404 显式报错（移除协议回退）/ doctor 协议探测
- **输出/MCP**：单点解析器收敛 / `_log` 孤儿豁免 / MCP 工具 isError / primary_language 改名
- **CLI**：update `--output` 复核修正 / ast-search language 校验 / card root global / sync 纳入锁 / dry-run 组合显式 / bench `-c`
- **搜索锁**：索引重建清空不吞错 / need_reindex 语义去重 / watch `./` 剥离 / 锁池复用 / LockError 结构化 / integrity_check / watch fail-fast

## [0.6.0] - 2026-08-12

### Added
- **本地嵌入（v51/T04）**：`[embed] provider = "local"` + `local_model`
  （bge-small-zh-v1.5/bge-small-en-v1.5/bge-m3/multilingual-e5-small）——
  fastembed ONNX 本地推理，零网络依赖；默认 `provider = "remote"`
  （qwen3-text-embedding 不变）；`cargo build --features local-embed` 启用
- **评测参考注入（v51/T05）**：`bench`/`rubrics` 命令新增 `--reference <path>`
  ——judge 三态（满意/不满意/不确定）prompt 注入人工参考材料对照，缓解
  「无标签基准」偏差（arXiv 2606.00093 实践）
- **评测体系修复（T05）**：TQS 三态 uncertain 正式启用（option_variant 独立 attempts 计数，不再 ignore）；`bench --root` 校验存在性；Update Recall 强制 mock 回放 + 产物过滤（`.code-repo-wiki/` 变更不计入 recall）；manifest 克隆目录名 URL 派生 + 已存在时 open+fetch 更新；TQS 报告输出 2×2 混淆矩阵
- **运行锁残留自愈（v51/13.1）**：锁冲突时读取 PID 做进程活性检测
  （Unix kill(pid,0) / Windows OpenProcess）——死进程残留锁自动清理重试
  一次，活进程报真并发（含 PID），空/半写锁自愈；TOCTOU 窗口经重读校验收窄
- **Unity 工程支持（v51/13.4）**：根级 `Packages/`（UPM 第三方包）、`Temp/`、
  `Logs/` 目录默认排除（嵌套同名目录保留）
- **generate/update --wait / --skip-if-locked（15.2）**：锁冲突策略可配——
  `--wait <秒>` 轮询重试（`--wait 0` = 不等待）、`--skip-if-locked` 冲突跳过
  （退出码 0，非阻塞），hook/CI 拿锁不再误报并发
- **CLI help 分组（16.3）**：顶层 help 静态分组——查询命令 / 生成命令 /
  维护命令 / 评测命令 四组 18 命令（clap 4.6.4 无原生子命令分组，用
  override_help 静态文本 + 契约测试防漂移）
- **search --engine 解析期校验（16.3）**：`search --engine` 改 clap ValueEnum
  解析期校验——非法值报错退出码 2，help 列出 possible values
- **card 子命令 -c 短参（16.3）**：card 各动作的 `--config` 补 `-c` 短参
- **AGENTS.md 注入块五要素重写（16.5）**：install 注入模板与仓库根 AGENTS.md
  按 agents.md 标准五要素重写——命令优先（核心命令速查）/ MCP 工具清单
  （wiki_ 前缀工具与使用时机）/ 完成定义（update no-op、lint 通过判据）/
  渐进式披露（llms.txt 站点地图 → overview/architecture → 模块页 → api.md）；
  README/cli.md MCP 清单同步、config.toml 模板命名统一

### Changed
- **LLM 调用层统一契约重构（v51/T01）**：SSE 流式截断检测（chat
  finish_reason / Anthropic stop_reason / Responses incomplete）；full jitter
  指数退避重试 + Retry-After 头尊重（429/503）；chat/embed 并发信号量
  `max_concurrency`（默认 16/16/4）；Anthropic max_tokens 默认 4096→8192；
  embedding 批次内并发（4 路保序）；`[embed] max_concurrency = 0` 显式报错
  （不再死锁挂起）
- **增量更新正确性修复（v51/T02）**：语义索引 rowid 不再显式自增（修复增量
  路径主键冲突——此前每次增量更新必失败）；watch 中途存盘移除（崩溃后产物
  与指纹失配隐患）；变更路径精确匹配（a.ts 不再误命中 a.tsx）
- **检索质量（v51/T03）**：RRF 融合 k 可配置 `[search] rrf_k`（默认 40）；
  FTS5 保留词（OR/AND/NOT）转义；同名实体去重键含行号；callgraph 符号级聚合
- **图分析层（v51/T11）**：embedding 提取 4 线程并发；单例特征过滤；
  确定性排序（NodeId tie-break）；import 边单边建模（自克隆边删除）
- **生成管线（v51/T07）**：卡片失败不再错位（失败卡占位对齐）；Mermaid
  降级空块感知；卡片 JSON 解析失败自动重试一次；chunk 依赖单遍处理
- **解析器（v51/T06）**：.tsx 文件用 TSX 语法解析（JSX 实体不再丢失）；
  Python docstring 归属修复；`dist/build/out/bin/obj` 仅根级排除；Java
  注解/C# delegate+event 支持
- **安全（v51/T08）**：HTML 输出事件级 XSS 转义（原始 HTML 不再透传）；
  bench 模板配置脱敏（api_key 不落盘）；OpenCode/Claude MCP 配置读失败中止
  写回（防覆盖 OAuth 会话）；LLM prompt 注入边界声明（代码为数据非指令）；
  模板占位符缺失显式报错、{{ 残留检测、引导注入分级（tier）、.gitignore 模板四项
- **progress_json 净化（v51/T09a）**：generate/update 完成摘要与 no-op 早退
  在 --progress-json 下输出 JSON 行（不再污染 JSONL 流）
- **崩溃自愈有界化（v51/13.2）**：watch 非锁错误指数退避重试封顶 10 次
  （5s→60s）；锁冲突立即退出（不再无限重试）
- **git hook 增强（v51/13.3）**：post-commit/post-merge 模板锁感知（另一
  实例运行中跳过本次）；update-error.log 超 1MiB 轮转保留尾部 100 行
- **图构建进度补点（13.5）**：`analyzing 27%` 移至 build_graph 完成后 + 28/29 推进点（25%→30% 大窗口不再黑屏）
- **panic 链加固（13.6）**：子线程 panic 降级为阶段告警 + 模块检测阶段定位（不再整体中断生成）
- **架构概览 prompt 消费模块职责描述（14.3）**：architecture 页 user prompt 补入
  `describe_modules` 生成的模块职责描述（缺失时退化回纯统计行）——消除每模块
  1 次白费的 LLM 调用，架构页模块职责不再依赖 LLM 猜测
- **运行锁协议 fd-lock 内核锁重写（15.1）**：锁实现由 create_new+PID 活性
  检测改为 fd-lock 内核锁（常驻锁文件 + 持锁者身份写入），同进程二次获取
  也按 WouldBlock 报真并发——消除 PID 活性判定失真与 hook 内 check-then-act
  TOCTOU
- **git hook 模板改 --skip-if-locked（15.3）**：post-commit/post-merge 模板
  由探测 + 活性判定改为命令内原子拿锁自行跳过（--skip-if-locked），并发由
  update 命令内处理；watch 监听过滤产物目录（自触发不再递归更新）
- **MCP 工具改名 wiki_ 前缀（16.1）**：MCP server 五个工具统一 `wiki_` 前缀
  （wiki_search/wiki_ast_search/wiki_status/wiki_read_page/wiki_read_card），
  与 OpenCode 插件工具命名对齐；删除 wiki_init 死接线（无消费者残留工具）
- **MCP 工具描述英文四要素重写（16.2）**：五个工具描述按
  Purpose / When to use / When NOT to use / Parameters & return example 四要素
  重写（Agent 命中点判定素材），search/ast_search 互指使用边界
- **search 结构化输出（16.2）**：MCP `wiki_search` 命中行结构化——签名 +
  文件:行号区间 + score（RRF 融合分）+ callers/callees（hybrid 专属）；
  `wiki_ast_search` 结果尾部附扫描耗时提示（成本透明）
- **prompt 体系修复（16.4）**：overview 拆 system+user（指令入 system、数据
  入 user、防御声明列出数据类别）；module_description/index_guide 补防御声明 +
  user 数据段 === 分隔标记；edit_card/schema_doc 语言映射对齐 output_lang；
  6 个裁判 prompt 补 few-shot 示例；v45 契约测试扩展注入防御/分隔标记断言

### Fixed
- **搜索/ast-search 输出目录 root 化（14.1，P0）**：`--root` 场景下
  `execute_search`/`execute_ast_search` 改走 `load_config_rooted`——输出目录统一
  解析到 `root/.code-repo-wiki`，修复跨 cwd 调用时索引目录按进程 cwd 相对解析
  导致的「读错索引目录」（索引不存在或空结果，cli-vs-mcp-02）
- **MCP 工具产物路径 root 化（14.2，P0）**：search/read_wiki_page/read_card/status
  四个工具配置加载改 `load_config_rooted`，`status` 改用注入的 `--root`
  （不再 `from_cwd()` 重建）——修复跨 cwd 调用时 MCP 读错产物目录、`status`
  误报「未生成」（cli-vs-mcp-03）；删除 resolve_mcp_config 死代码
- **MCP 语义降级提示对 Agent 可见（14.2，FR-501）**：`search` 结果尾部与
  `status` 报告显式输出「语义索引已降级（原因: …）」（此前仅进 tracing 日志，
  MCP 调用方不可见；与 CLI 行为对齐，cli-vs-mcp-07）
- **card 命令纳入写锁（15.4）**：card 写卡片同样经运行锁串行化，防并发双写
  （与 generate/update 同锁语义）；watch 撞锁测试改真实持锁者场景（15.5，
  适配 fd-lock 内核锁语义，不再依赖 PID 活性判定）

### 依赖/CI
- **依赖升级（v51/T10）**：rusqlite 0.32→0.38（bundled SQLite 3.51.1）、
  tree-sitter 语言 crate 按版本矩阵升级
- **CI 测试矩阵加 macOS（v51/T09b）**：测试矩阵扩为 ubuntu/windows/macos
  三平台
- **release 测试前置**：tag 推送产物经三平台测试全绿才发布
- **版本号 0.5.1 → 0.6.0（16.5）**：Cargo.toml/Cargo.lock 版本同步，[Unreleased]
  归档为本段

## [0.5.1] - 2026-08-09

### Added
- **LLM 思考模式可配置（v50）**：`[llm] thinking`（`true`/`false`）+ `[llm]
  reasoning_effort`（`"low"/"high"/"max"`）——DeepSeek 系 thinking 模式开关，
  仅 openai-compatible（chat/completions）路径生效。deepseek-v4 官方默认
  启用思考且 effort=high，批量卡片/文档生成实测慢约 5×、输出 token 多约
  3.7×——低推理任务在配置中 `thinking = false` 可获约 5× 提速（参数与官方
  Thinking Mode 文档 2026-08-10 抓取核证：`thinking: {"type":"disabled"}`
  + `reasoning_effort`）。

### Changed
- **LLM 并发上限 16 → 128（v50）**：依据权威查证——DeepSeek 官方限流为纯
  并发数（v4-flash 账户级 2500），128 远低于上限；服务端连续批处理（Orca/
  vLLM）吞吐拐点约 128 并发（NVIDIA NIM 官方基准：并发 100→250 吞吐持平而
  TTFT 8s→88s），128 为拐点内最大化收益取值；超限由 429 退避兜底。
- **上层重试语义统一（v50）**：`wiki.rs complete_with_retry` 原对**一切**
  Err 无条件重试 3 次（黑洞首字节超时 90s 被放大到约 270s/调用，且 mock
  注入失败也被白重试）——改为直接透传 `llm.rs retry_with_backoff` 的重试
  结论（429/5xx/reqwest 连接失败已在该层重试；黑洞/业务 4xx 立即失败走
  降级/补偿）。

### Added
- **LLM 逐项进度（v46）**：`generate`/`update` 的卡片生成与 Wiki 页生成两个 LLM
  密集阶段改为逐项进度——`进度 [生成知识卡片] 3/12（62%）`（任务单位 N/M，
  对齐 Ubuntu CLI 规范；stderr 输出，TTY 下行内刷新、非 TTY 按阶段/10% 档/
  每 5 项节流）；`--progress-json` 同步新增 `current`/`total` 字段（机器消费）；
  no-op 早退补齐 `done` 事件（进度流终态完整）。

### Fixed
- **进度事件流单调性**：cards/wiki 阶段点原在生成后发射（与项级事件、output
  95 冲突导致百分比回退）——阶段点移至生成前发射，wiki 项级区间收敛到
  90..95（与 output 95 相接）。
- **更新卡死（v47）**：`update` 中途无进度输出的四个根因全部修复——①非 TTY
  进度行缺换行（与 tracing 日志粘行）；②`analyzing 25%` 在 54s 特征聚类后
  才发射（长黑屏误判卡死）→ 移至图构建前；③HTTP 请求 `send()` 无首字节
  超时（端点黑洞可无限挂起——实测一进程卡 16 小时）→ 新增 90s 首字节超时，
  超时视为不可重试的网络故障；④v30 的 FileWatch 状态哨兵 `"file-watch"` 被
  当 git SHA 解析（`unable to parse OID`）导致每次 `update` 回退全量且 no-op
  判定失效 → 哨兵显式识别、git 仓库改记真实 HEAD 提交。
- **真实进度可观测**：真实 LLM 全链路实测（426s，107 文件/16 页/13 模块）
  进度 10→25→30→60→90→95→98% 全程可见、无卡死。
- **大仓告警刷屏与黑屏期（v48）**：Unity 项目 3143 文件首次 generate 实测反馈——
  ①解析器合法 kind `"var"`（Go 包级变量）漏映射，90+ 条「未知实体类型」warn
  同毫秒刷屏 → 补 `var → Variable` 映射；②循环依赖检测全量 Debug 打印无截断
  （跨模块同名字段/方法按名互连的巨型伪 SCC 可达数百节点、30K+ 字符刷屏并
  淹没真实进度）→ 新增 `format_cycles` 紧凑格式化（每链最多 8 个名称、总链数/
  总节点数摘要，超长以「…」省略）；③`analyzing 25%` 与 `chunking 30%` 之间
  的图构建+模块检测在大仓可达分钟级且无任何输出 → 补 `analyzing 27%` 进度
   事件，25→27→30 单调推进，长黑屏期可见「构建知识图谱」仍在运行。
- **大仓模块检测卡死（v49）**：Unity 大仓在 `analyzing 27%` 后分钟级无输出——
  `module.rs` 旧实现对每个社区调用 `count_edges` 三次（cohesion/coupling/
  expanded 各一次）、每次遍历全图边两遍，复杂度 O(社区数 × 边数)（数百目录
  × 约 20 万边 × 5 遍 ≈ 数亿次迭代）→ 重构为单遍边聚合：一次遍历建立
  File→社区/实体→File 归位索引，再单遍非 Contains 边累加各社区 internal/
  external，复杂度降为 O(边 + 社区)（3000 文件合成图 0.02s 完成）；新增
  聚合与暴力参照逐社区数值一致测试 + 大图冒烟测试防回归（语义严格等价，
  内聚/耦合/扩展集合规则不变）。

## [0.5.0] - 2026-08-09

### Added
- **命令进度提示（v44）**：`generate`/`update` 文本模式接入进度事件流——分阶段
  进度行输出到 stderr（`进度 [扫描源码] 10%`…，阶段与 lib.rs 事件一一对应），
  完成摘要输出到 stdout（`✓ 生成完成: 扫描 N 个文件 / M 个实体 / K 页文档（Ts）`、
  `✓ 增量更新完成: …`；no-op 早退保持「无文件变更，跳过更新（no-op）」契约行，
  不打印摘要）——长任务不再静默，用户明确知道完成与否；
- **提示词工程优化（v45）**：`generate/prompt.rs` 四个 system prompt
  （模块摘要/架构概览/知识卡片/Wiki 页）重构——指令前置 + `### 角色/任务/
  输出格式/约束` 分节（OpenAI 官方最佳实践 + Lost in the Middle 位置效应）；
  知识卡片 prompt 新增「输出原始 JSON，不要 Markdown 代码块包裹」约束；
  Wiki 页 prompt 新增「信息不足显式标注（信息不足）而非编造」防幻觉写法
  （Anthropic reduce-hallucinations）；输出语言显式化（zh → 简体中文）；
  既有真实性/引用契约字面全部保留（anti-fabrication 测试不破）；
- **发布工作流修复（v45）**：`release.yml` 重写——新增 `create-release` job
  （Draft Release 先行创建 + `--generate-notes`），`build-binaries` 矩阵经
  `release-id` 上传到同一 Draft（消除并发各自创建 Release 的竞态——此前
  矩阵 job 反复 "release not found"）；macos 构建修复（brew openssl@3 +
  PKG_CONFIG_PATH——自带 LibreSSL 与 openssl-sys 不兼容）；新增
  `publish-release` job（全部二进制上传完成后 Draft → 正式发布）；
  仓库 Actions 写权限要求写入工作流注释（需 Read and write）

### Changed
- **版本号 0.4.0 → 0.5.0（v45）**：Cargo.toml/Cargo.lock 版本同步；
- **README 结构重排（v44）**：新增「常用命令」速查表（快速开始之后），
  「面向 AI 助手」移至核心功能之后，统一表格/代码块排版，search 示例
  改省略写法（默认 hybrid + top-k 10 均可省略——cli.md 命令表同步注明默认值）

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
- **Apache License 2.0（v43）**：仓库根 LICENSE 官方全文（apache.org 权威文本原样）；
  `Cargo.toml` 补发布元数据（`license = "Apache-2.0"`、`repository`、`readme`、
  `keywords` ≤5、`categories` ≤5——crates.io 硬性必填 description+license，缺失即 400）
- **发布工作流（v43）**：`.github/workflows/release.yml`——tag `v*` 触发：crates.io
  发布（Trusted Publishing OIDC 免长期 token，首次发布需手动）+ GitHub Releases
  二进制矩阵（linux/macos/windows 四目标，taiki-e upload-rust-binary-action）
- **文档质量门禁（v43）**：`.markdownlint-cli2.jsonc` 中文文档规则配置 + CI 新增
  `lint-docs` job（markdownlint-cli2 结构检查 + lychee 链接可达性；CHANGELOG/STATUS
  历史记录豁免，已知不可用网关端点与脱敏占位 URL 入 `.lycheeignore`）

### Changed
- **项目改名 Code Repo Wiki（v37）**：crate/二进制/命令名 `repo-wiki` → `code-repo-wiki`（代码/测试/文档/MCP 注册键/git hooks 字面量/AGENTS 注入块全链路同步）；产物目录 `.repo-wiki` → `.code-repo-wiki`；用户级配置目录 `%APPDATA%\repo-wiki` → `%APPDATA%\code-repo-wiki`（改名不迁移，删除重装并重新配置 key）；GitHub 仓库改名 `mook-wenyu/code-repo-wiki`
- **install 注入块渲染接入完整配置链（v42）**：`install_wiki` 改用 `load_default_config(root)`（项目级字段级合并覆盖用户级），不再只读项目级单文件——项目无 config.toml 时不再误报「未找到有效配置」，且按用户级配置（如 `language`）渲染注入块；两处配置皆无时 install 自动创建用户级默认模板（install 语义即确保用户级配置）；畸形配置降级为「配置解析失败」提示并继续注入
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
- **用户级配置目录迁移 home 点目录（v41）**：`%APPDATA%\code-repo-wiki`
  → `%USERPROFILE%\.code-repo-wiki`（Unix `~/.code-repo-wiki`，对齐
  Codex/Claude Code 点目录惯例）；`CODE_REPO_WIKI_HOME` 环境变量显式重定位；
  首次运行从旧目录一次性迁移（复制内容，旧目录保留不删，显式设置
  `CODE_REPO_WIKI_HOME` 时不迁移）；key 测试夹具清理脚本同步双路径
- **git hooks 共存追加（v41）**：install 对已存在且非 code-repo-wiki 内容的
  hook 改为**尾部追加标记块**（`# code-repo-wiki: append-begin/append-end`，
  与 LFS 包装/memorix 等既有 hook 共存，再次 install 幂等只更新块，uninstall
  剥离块并还原用户内容）；安装前检查 `core.hooksPath` 并提示指向其他目录时
  hook 不生效
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
