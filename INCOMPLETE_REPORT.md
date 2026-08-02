# repo-wiki 未完成项深度分析报告

> 生成日期：2026-08-02
> 方法：全量代码检索（54 src 文件 + 17 测试文件 + 插件/配置/文档），信号扫描（TODO/FIXME/stub/已知限制/降级路径/硬编码），逐项以文件:行号证据核验
> 结论速览：**显式 TODO 为 0**，但存在 **3 个确凿功能缺陷**（git hook 静默失效 / install 不装插件 / export 触发重生成）、**12 项已知限制**、**3 处文档失实**、**5 类工程债务**

---

## 一、发现总览

| 类别 | 数量 | 严重度 | 一句话 |
|------|------|--------|--------|
| A. 功能性缺陷（确凿 bug，修复成本低） | 4 | 高 | install 装的 git hook 从第一天起就静默失败 |
| B. 已知限制（代码/文档自曝） | 12 | 中 | 多数有降级路径，属"已知且接受" |
| C. 文档失实（代码与文档不符） | 3 | 低-中 | CONTEXT.md 边界 ×2、STATUS.md 过时条目 ×1 |
| D. 工程债务（阻塞演进） | 5 | 中 | 复制粘贴三处 + 反向依赖 + 乱码文件 |
| E. 战略性未完成 | 1 | 高 | 无 AI agent 消费通道（详见 ANALYSIS_REPORT.md） |

---

## 二、A 类：功能性缺陷（确凿，逐一证据）

### A1【高】git hook 静默失效——install 的核心卖点从第一天起不工作

- 证据链：
  - `src/commands.rs:198`：安装的 hook 内容为 `repo-wiki update --quiet 2>/dev/null || true`
  - `src/main.rs:30-40`：`Update` 子命令只有 `config/output/force` 三个参数，**不存在 `--quiet`**
  - clap 4 对未知参数默认报错退出（exit code 2），`2>/dev/null || true` 把错误静默吞掉
- 影响：`install-to-opencode` 安装的 post-commit / post-merge 钩子**从不触发 wiki 更新**，且无任何可见错误。用户以为自动更新在工作，实际从未工作。
- 修复（两选一，成本 1 行）：
  - hook 去掉 `--quiet`（改 `repo-wiki update 2>/dev/null || true`）；或
  - CLI `Update` 加 `--quiet` 参数（抑制 tracing 输出到 stderr——注意 tracing 输出走 stderr，`2>/dev/null` 已能抑制，所以去掉 `--quiet` 即修复）
- 备注：hook 内 `repo-wiki` 依赖 PATH，`cd "$(git rev-parse --show-toplevel)"` 后若二进制不在 PATH 同样静默失败——建议 hook 用绝对路径或 `command -v repo-wiki` 探测。

### A2【高】`install-to-opencode` 不安装插件文件——输出"已安装"但插件不存在

- 证据链：
  - `src/commands.rs:176-185`：`install("opencode")` 仅调用 `oc.install_plugin()` 后打印 "✓ OpenCode 插件已安装"
  - `src/config/opencode.rs:56-87`：`install_plugin()` 只做一件事——幂等清理 opencode.json 中历史遗留的无效 `plugins` 键；**全程没有写 `.opencode/plugins/repo-wiki.ts`**
  - `src/config/opencode.rs:91-93` 注释自认："卸载插件的实际动作是删除 .opencode/plugins/repo-wiki.ts 文件（由用户决定，不在此处执行）"
  - `src/config/opencode.rs:119-129`：`is_installed()` 以插件文件存在性为准——install 后 is_installed() 恒为 false
- 影响：用户运行 install 后，插件并未安装（opencode 目录自动加载机制下没有文件可加载）。安装流程是半成品。
- 修复：`include_str!("../../.opencode/plugins/repo-wiki.ts")` 嵌入模板，install 时写入 `.opencode/plugins/repo-wiki.ts`（不存在才写）；uninstall 删除该文件。或至少在 install 输出中如实提示"插件文件需手工放置"。
- 相关发现：`.opencode/plugins/repo-wiki.ts`（273 行）本身实现完整且质量高（DRY 工厂、execa 包装、错误透传）——只差安装闭环。

### A3【高】`export` 先全量重新生成——导出不是"导出"，是"重新生成+导出"

- 证据链：`src/main.rs:297-302`：
  ```rust
  Commands::Export { config } => {
      let cfg = repo_wiki::config::load_config(&config)?;
      let result = repo_wiki::run_pipeline(&config, None, false)?;  // ← 全量重新生成
      repo_wiki::output::html::export_html(&result.documents, &result.cards, &result.graph, &cfg)?;
  ```
- 影响：
  - 导出触发全部 LLM 调用（耗时、耗钱）；无 API key 时导出直接失败
  - 重新生成可能与磁盘已有产物不一致（新生成 ≠ 已 commit 的产物）
  - `export` 无 `-o` 输出覆盖参数（generate/update 都有），CLI 不一致
- 修复：export 增加 `--skip-generate`（读取 `.repo-wiki/_index.json` 等已有产物）或将生成改为可选；至少补 `-o` 参数对齐 generate。
- 备注：`output/html.rs:17-216` 渲染逻辑本身完整（index.html 按模块分组、style.css、Mermaid module-deps、cards 渲染），问题只在入口。

### A4【低】install 在非 git 仓库静默跳过 hook 安装

- 证据链：`src/commands.rs:199-215`：`if hooks_dir.exists()` 才安装 hook，else 分支**无任何提示**；非 git 仓库用户会以为 hook 已装。
- 修复：else 分支打印提示。

---

## 三、B 类：已知限制（代码/文档自曝，多为设计内降级）

| # | 限制 | 证据 | 现状评估 |
|---|------|------|---------|
| B1 | 2 处 Leiden expect 无降级，外部库失败直接 panic 整条流水线 | `src/analysis/community.rs:124`、`src/analysis/feature.rs:171` | **风险高**，应改 Result 传播（P0） |
| B2 | module_{n} 档（无目录社区）删除清理漏删 | `src/lib.rs:397-399` 注释自认"已知限制" | 可接受（残留孤儿文件，lint 可发现） |
| B3 | MAX_DIFF_LINES=10_000 硬编码回退全量 | `src/incremental/mod.rs:22,116-125` | 设计内安全阀，可配置化 |
| B4 | watch 防抖 300ms 硬编码 | `src/incremental/watch.rs:51` | 可配置化 |
| B5 | CPM γ=0.5 / seed=42 硬编码，小仓库调参结果 | `src/analysis/community.rs`（STATUS.md:20） | 大仓库需实测（已知） |
| B6 | tests/fixtures/sample-repo/config.toml 真实 provider，复用即触网 | `tests/fixtures/sample-repo/config.toml`（STATUS.md:24） | **操作风险**，应改 mock（P0） |
| B7 | 特征聚类 embedding 注入路径（0.5 语义权重）从未真实验证（无 key） | `src/analysis/feature.rs:103-126`、STATUS.md:21 | 语义权重是推测值 |
| B8 | 真实 LLM 大仓库全量生成 e2e、watch e2e 未验证 | STATUS.md:25 | 验证缺口 |
| B9 | cwd 全局依赖：scan_and_parse 扫描根、plan 路径、opencode 配置、watch 根均取 current_dir | `src/ingest/mod.rs:13`、`src/config/plan.rs:86`、`src/config/opencode.rs:39`、`src/lib.rs:512` | 并发/服务化扩展瓶颈（bench 需 CWD_LOCK，benches/bench_search.rs:14-21） |
| B10 | SemanticEngine 向量全量载入内存做余弦 | `src/search/semantic.rs` | 大索引内存放大 |
| B11 | 实体变化分类依赖旧 commit 可解析；文件级删除/重命名不分类实体 | `src/incremental/change.rs:13,215` | 有降级（回退保守传播） |
| B12 | Mock provider 流式已覆盖（llm.rs:520-524），无缺口 | — | 澄清：非限制（此前疑似项已排除） |

---

## 四、C 类：文档失实（3 处）

| # | 文档声称 | 实际 | 证据 |
|---|---------|------|------|
| C1 | CONTEXT.md："generate 层只依赖 analysis 层结果（不直接访问 parser）" | generate 直接 `use crate::ingest::parser::FileInsight` | `src/generate/mod.rs:16` |
| C2 | CONTEXT.md："output 层是纯渲染" | output 反向依赖 generate 的 `collect_languages` | `src/output/mod.rs:17-19` |
| C3 | STATUS.md:24："status 命令是桩实现（只打印就绪+配置路径，不检查产物）" | status 已是完整实现（ready/wiki_pages/cards/lint 健康检查 + 非 0 退出码），且有测试 | `src/commands.rs:9-34,254-294`（前一轮"桩"描述已过时） |

---

## 五、D 类：工程债务（不阻断，但阻塞演进节奏）

1. **7 语言 parser walk/fallback 骨架逐字复制 ~800 行**：csharp.rs:38、java.rs:14、go.rs:14、javascript.rs:31 等前 15 行完全一致，仅 LANGUAGE 常量与 kind 表不同——最大重复面。建议宏/生成器统一。
2. **双 LLM provider retry+SSE 双写**：llm.rs:135-270（OpenAI）vs :326-460（Anthropic）结构高度相似。
3. **全量/增量双流水线骨架重复 ~150 行**：lib.rs:136-217 vs :269-376。
4. **output → generate 反向依赖**（C2 的代码侧）：`collect_languages` 应上移共享。
5. **.gitignore 注释为 GBK 乱码**（Windows 编码写入）：`/target` 上方两行注释不可读，极轻微。

---

## 六、E 类：战略性未完成（前报告已详述，此处登记）

- **无 AI agent 消费通道**（MCP server / skill 文件缺失）：本仓库已有全部底层能力（三引擎、call graph、增量状态、9 个插件工具），业界 4 家独立押注 MCP。详见 ANALYSIS_REPORT.md 差距 1。

---

## 七、修复优先级建议

### P0（用户立即踩 / 数据风险，共 1-2 小时）
1. **A1** git hook 去掉 `--quiet`（或 CLI 补参数）+ hook 内 PATH 探测——1 行修复，恢复自动更新
2. **A3** export 加 `--skip-generate`（读 _index.json 已有产物）或至少加 `-o`
3. **B1** community.rs:124 / feature.rs:171 的 expect 改 Result 传播 + 降级（与 T 阶段"失败隔离"精神一致）
4. **B6** fixture config.toml 改 mock provider（防真实触网）

### P1（安装体验闭环，2-4 小时）
5. **A2** install 嵌入插件模板并写入 `.opencode/plugins/repo-wiki.ts`；uninstall 删除文件（opencode.rs:91-93 注释已预留语义）
6. **A4** 非 git 仓库 install 提示

### P2（一致性，1 小时）
7. **C1/C2** CONTEXT.md 修正边界描述
8. **C3** STATUS.md 移除过时"桩实现"条目（本文档生效后 STATUS.md 同步）

### P3（演进，按需）
9. **B3/B4** MAX_DIFF_LINES、防抖窗口配置化（schema 加字段，默认值不变）
10. **B9** 扫描根/plan 路径/配置路径改为参数注入，移除 CWD_LOCK
11. **D1-D4** 三处去重 + 反向依赖收敛
12. **E** MCP server（见 ANALYSIS_REPORT.md 路线图 P1）

---

## 八、验证建议（修复后）

- A1：临时仓库 `git init` → install → 修改源文件 → commit → 断言 wiki 产物 mtime 更新（复用 tests/test_cli_smoke.rs 的 spawn+轮询模式）
- A2：install 后断言 `.opencode/plugins/repo-wiki.ts` 存在；uninstall 后消失
- A3：mock provider 下 export 两次，断言第二次无 LLM 调用（call_count 不变）
- B1：构造 Leiden 失败场景（空图/畸形图）断言不 panic、有告警
- 回归：cargo test 全绿 + clippy -D warnings 0 + machete 零未使用

---

## 附：排除项澄清（扫描命中但确认非问题）

- llm.rs:48 "streaming not supported"：trait 默认实现，Mock 已覆盖（llm.rs:520-524），真实 provider 均实现流式——非缺口
- main.rs:326 "不支持的搜索引擎"：合法参数校验，非未完成
- 各种"回退全量/降级"路径（非 git 仓库、diff 超限、embedding 失败、LLM 失败）：设计内失败隔离，有测试覆盖——非缺陷
- export_html 内 `todo_notes: vec![]`（html.rs 测试辅助）：仅测试数据，非业务代码
