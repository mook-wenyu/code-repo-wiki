# repo-wiki 全面业务分析报告（v2）

> 基于 4 个并行子代理对全部 42 个 Rust 源文件、Cargo.toml、配置和插件的深度审查。
> 代码总量：~6,000 行 Rust + ~100 行 TypeScript
> 审查时间：2026-07-30

---

## 一、关键业务影响分级

| 等级 | 业务影响 | 数量 |
|------|---------|------|
| **P0 — 灾难性** | 数据损坏、静默错误、核心逻辑被绕过 | 12 |
| **P1 — 严重** | 功能损坏、输出错误、性能不可用 | 24 |
| **P2 — 中等** | 可用性差、测试漏洞、可维护性债务 | 35 |
| **P3 — 改进** | 架构优化、代码异味、死代码、文档缺口 | 40+ |

---

## 二、P0 — 灾难性问题（必须立即修复）

### P0-1 `chunk.rs:47-61` — NodeId→file_path 映射完全缺失（已知）
**src/generate/chunk.rs:47-61** — `module.node_ids.iter().any(|_| file_path_str.contains(&module.name))` 谓词 `|_|` 表示入参未使用。完全忽略 node_id，每次迭代执行相同的检查。

### P0-2 `graph.rs:207-228` — 导入边全局名称匹配（已知）
**src/analysis/graph.rs:207-228** — `name_map: HashMap<String, NodeId>` 中相同名字的不同实体相互覆盖。两个不同模块的 `Config` 结构体会互相冲突。

### P0-3 `dependency.rs:69-122` — 环检测借用 petgraph 实现（已知，已修复）
**src/analysis/dependency.rs:69-122** — `visited.insert(current)` 在全局 DFS 中阻止路径重用，错过以非起始节点为根的环。**建议：使用 petgraph::algo::tarjan_scc**。

### P0-4 `card.rs:56` — max_concurrent 完全被忽略（已知）
**src/generate/card.rs:53-62** — 所有 LLM 请求通过 `join_all` 一次性全部发出，`_max_concurrent` 参数被忽略。

### P0-5 `generate/mod.rs:90` — cards 和 chunks 索引对齐断裂（新发现）
**src/generate/mod.rs:90** — `generate_all_cards` 跳过失败的 LLM 调用后，`cards` 长度 < `chunks` 长度。`cards.get(i)` 导致 `chunks[i]` 配对 `cards[i]` 对应的是**不同**的 chunk。**wiki 页面使用了错误的卡片内容**。

### P0-6 `impact.rs:29` — 文件路径子串匹配（新发现）
**src/incremental/impact.rs:29** — `fp.contains(cfp.as_str())` 使用子串匹配而非精确路径比较。修改 `src/parser.rs` 也会匹配 `src/parser_utils.rs`。

### P0-7 `impact.rs:81` — module_path[0] 无边界检查 panic（新发现）
**src/incremental/impact.rs:50,81** — L50 有 `!start_node.module_path.is_empty()` 检查但 L81 对邻居节点**没有**检查。空 module_path 的邻居节点触发 index out of bounds panic。

### P0-8 `watch.rs:121` — 空 include 列表 panic（新发现）
**src/incremental/watch.rs:121** — `config.scope.include[0]` — 如果 `scope.include` 为空，索引越界 panic。

### P0-9 `lib.rs:94-107` — 增量更新无变更时清空输出（新发现）
**src/lib.rs:94-107** — `inc_result.affected_modules.is_empty()` 时返回空 document/card 列表，导致 outputs 中的所有旧文档被**完全清除**。

### P0-10 `main.rs:73` — Generate --output 参数完全被忽略（新发现）
**src/main.rs:73** — `Commands::Generate { config, output: _ }` — `_` 表示参数被声明但完全不用。

### P0-11 `graph.rs:325` — build_call_edges 子串匹配误报（新发现）
**src/analysis/graph.rs:325** — `format!("{}(", callee_name)` 使 `add(` 匹配 `addToQueue(`、`additional_setup(`。所有函数名的子串都成为调用边。

### P0-12 `generate/mod.rs:44-46` — Custom provider 错误映射（新发现）
**src/generate/mod.rs:44-46** — `LlmProviderType::Custom => Ok(Provider::OpenAi(...))` — Custom 类型被当作 OpenAI 处理，应使用 base_url 构造。

---

## 三、P1 — 严重问题

### 3.1 功能损坏

| # | 位置 | 问题 | 影响 |
|---|------|------|------|
| 3.1.1 | `python.rs:59` | 签名在 `':'` 分割 | `def greet(name: str) -> str:` 给出错误签名 |
| 3.1.2 | `python.rs:100-119` | 多行文档字符串被忽略 | Python 类/函数丢失文档 |
| 3.1.3 | `prompt.rs:38-41` | 文档字符串截断为第一行 | LLM 仅接收第一行文档 |
| 3.1.4 | `prompt.rs` | **源代码完全缺失** | LLM 看不到函数体，生成的内容只是关于元数据的元数据 |
| 3.1.5 | `chunk.rs:68-72` | dependencies = 除自身外的所有模块 | 依赖数据完全无效 |
| 3.1.6 | `html.rs:183` | 硬编码 `../style.css`，index.html 需要 `./style.css` | 索引页 CSS 加载失败 |
| 3.1.7 | `mermaid.rs:140-142` | `module_name` 只返回路径第一段 | 所有模块在图中都显示为 "src" |
| 3.1.8 | `diff.rs:99-105` | 重命名文件被跟踪但从未被消费 | 重命名文件永远不会被增量更新处理 |
| 3.1.9 | `unity.rs:96-101` | 生命周期方法不检查 MonoBehaviour 继承 | 非 Unity 项目产生误报 |
| 3.1.10 | `html.rs:89` | Mermaid 模块图外部节点但无边 | 模块间关系完全缺失 |
| 3.1.11 | `diff.rs:28` | git2::Repository::open 同步阻塞 | 异步运行时中阻塞整个线程 |
| 3.1.12 | `parser/rust.rs:160` | `Self::fallback(source)` 被调用两次 | 同一源文件被两次 O(n) 解析 |
| 3.1.13 | `git diff` 第一次运行退化 | `diff.rs:52-54` 无 last_commit_hash 使用 HEAD^ | 多次提交间的变更被丢失 |
| 3.1.14 | `llm.rs:161` | 退避无抖动 | 并行请求同一瞬间重试（雷鸣羊群） |
| 3.1.15 | `llm.rs:174-183` | 4xx 与 5xx 同样重试 | 400/401 消耗所有重试不可能成功 |
| 3.1.16 | `card.rs:124-128` | module_type 始终返回 "module" | 文档丢失类型区分 |

### 3.2 性能不可用

| # | 位置 | 复杂度 | 问题 |
|---|------|--------|------|
| 3.2.1 | `graph.rs:321-347` | O(N²) + triple loop | build_call_edges 每个函数遍历整个 name_map |
| 3.2.2 | `graph.rs:234-251` | O(imports × targets × entities) | build_import_edges 三重嵌套 |
| 3.2.3 | `graph.rs:207,259,310` | 3× 遍历 | collect_node_names 被重复调用 3 次 |
| 3.2.4 | `graph.rs:122` | 完整图克隆 | kg.graph = g.clone() — O(N) + 内存分配 |
| 3.2.5 | `graph.rs:94` | 不必要完整 Entity clone | 只用了 name/doc_comment/signature |
| 3.2.6 | `dependency.rs:106-124` | 只有单层 BFS | 影响集不是传递闭包，完全无用的分析 |
| 3.2.7 | `chunk.rs:91` | O(n²) | Vec 去重应为 HashSet |
| 3.2.8 | `chunk.rs:104-118` | O(modules² × nodes × edges) | 依赖计算三重嵌套 |
| 3.2.9 | `module.rs:66` | 不必要全局 nodes.clone() | 大部分节点已分配 |
| 3.2.10 | `module.rs:112,123` | 双倍 count_edges | cohesion 和 coupling 独立调用 count_edges |
| 3.2.11 | `impact.rs:61-62` | 密集图 O(n²) | BFS 同时遍历出边和入边 |
| 3.2.12 | `lib.rs:51,110` | 每次创建新 tokio Runtime | 昂贵操作，应复用 |
| 3.2.13 | `chunk.rs:134-138` | 死计算 | `_file_stem` 赋值后从未使用 |

### 3.3 设计/架构违规

| # | 位置 | 问题 |
|---|------|------|
| 3.3.1 | `llm.rs:56-85` | Provider 枚举违反开放封闭原则（每个新 provider 改 4 处） |
| 3.3.2 | `llm.rs:104-127 vs 239-256` | OpenAiProvider 和 AnthropicProvider 构造函数逐行重复 |
| 3.3.3 | `generate/mod.rs:56-129 vs 135-209` | run_generation 和 run_generation_filtered 80% 重复 |
| 3.3.4 | `incremental/mod.rs:59-111 vs 114-147` | GitDiff 和 FileWatch 策略 80% 代码重复 |
| 3.3.5 | `llm.rs:105,240` + `embed.rs:37` | API Key 解析逻辑 3 处重复 |
| 3.3.6 | `config/opencode.rs:46-124` | 3 处独立的 JSON 读写逻辑 |
| 3.3.7 | `main.rs:100` | CLI 直接调用 output::html::export_html 跳过 lib.rs 抽象 |
| 3.3.8 | `lib.rs:51,110` | 手动创建 tokio Runtime，应迁移到 `#[tokio::main]` |
| 3.3.9 | `model/mod.rs:15` | pub graph 字段破坏封装 |
| 3.3.10 | `model/node.rs:25` | module_path: Vec\<String\> 与 ModuleCluster.name 类型不一致 |

---

## 四、P2 — 中等问题

### 4.1 错误处理缺口

| # | 位置 | 问题 |
|---|------|------|
| 4.1.1 | `parser/mod.rs:64-69` | 6 个 `.unwrap()` 在 tree-sitter 初始化失败时 panic（含 TypeScript 真风险） |
| 4.1.2 | `scanner.rs:21-22` | 无效 glob 模式被静默忽略 |
| 4.1.3 | `state.rs:41` | save() 不是原子操作（写入时崩溃产生损坏文件） |
| 4.1.4 | `state.rs:80` | compute_file_fingerprint 将整个文件读入内存 |
| 4.1.5 | `lib.rs:62` | 增量状态保存错误被静默吞掉 |
| 4.1.6 | `lib.rs:59` | git 命令失败时静默退化 |
| 4.1.7 | `card.rs:99-113` | LLM 响应中缺失字段静默变为空字符串 |
| 4.1.8 | `card.rs:67-74` | 卡片生成失败静默跳过，无失败计数 |
| 4.1.9 | `embed.rs:98-99` | filter_map 静默丢弃非 f64 → 产生更短嵌入向量 |
| 4.1.10 | `embed.rs:67-103` | 嵌入 API 无重试逻辑 |
| 4.1.11 | `watch.rs:144` | Channel 断开错误被 `.ok()` 吞掉 |
| 4.1.12 | `watch.rs:92-109` | 混合事件类型覆盖逻辑不正确 |
| 4.1.13 | `scanner.rs:42` | file_type() 返回 None 时静默跳过 |
| 4.1.14 | `parser/rust.rs:96` | strip_prefix/strip_suffix 静默降级到原始文本 |
| 4.1.15 | `incremental/mod.rs:68-71` | Git diff 失败 → 静默无变更 |
| 4.1.16 | 所有 6 parser | set_language/parse 失败 → fallback() 无 tracing::warn |

### 4.2 测试黑洞

| # | 模块 | 测试数 | 覆盖特征 |
|---|------|--------|---------|
| 4.2.1 | `parser` fallback 逻辑（全部 6 个） | 0 | 所有 parser 降级路径完全无测试 |
| 4.2.2 | `prompt.rs` | 0 | 无任何测试 |
| 4.2.3 | `generate/mod.rs` | 0 | 无流水线编排测试 |
| 4.2.4 | `incremental/diff.rs` | 0 | 无 git diff 集成测试 |
| 4.2.5 | `mermaid.rs:66-104` | 0 | `render_entity_graph` 完全未测试（且是死代码） |
| 4.2.6 | `html.rs` | 4（可达性） | CSS 路径、Mermaid ID 编码无验证 |
| 4.2.7 | `watch.rs` | 0 | 无文件监听测试 |
| 4.2.8 | `impact.rs` | 0 | BFS 传播无独立测试 |
| 4.2.9 | `graph.rs:277-292` | 0 | `parse_impl_target` 无单元测试 |
| 4.2.10 | `graph.rs:321-347` | 0 | build_call_edges 完全无测试 |
| 4.2.11 | `module.rs:find_nearest_module` | 0 | 核心函数无测试 |
| 4.2.12 | `chunk.rs:chunk_by_module` 依赖计算 | 0 | 三嵌套循环无回归测试 |
| 4.2.13 | `embed.rs` | 6 | 余弦相似度和空输入基本覆盖 |
| 4.2.14 | `unity.rs` 第二阶段 | 0 | 图节点更新逻辑完全未验证 |

**总计**: ~3,200 行逻辑代码，36 个单元测试，覆盖率约 10%。

### 4.3 解析器功能空白

| # | 语言 | 缺失功能 |
|---|------|---------|
| 4.3.1 | **Rust** | union_item、macro_invocation、macro_definition、associated_item、foreign_item |
| 4.3.2 | **TypeScript** | export_statement（JS 有但 TS 没有）、abstract_class、namespace、constructor、decorator、getter/setter、import alias |
| 4.3.3 | **JavaScript** | CommonJS require()/module.exports、dynamic import()、export default expression、getter/setter、generator_function |
| 4.3.4 | **Python** | decorator、async_function_definition（AST 无但 fallback 有）、`__all__`、type_alias(695) |
| 4.3.5 | **Go** | const_declaration、var_declaration、type alias、import alias |
| 4.3.6 | **C#** | record_struct、event、delegate、indexer、operator、using_static、using_alias、enum_member |
| 4.3.7 | **全部** | doc_comments: vec![] 始终为空（收集了但未传递给 FileInsight） |
| 4.3.8 | **全部** | 5/6 parser 不支持 import alias 提取（只有 Rust 做了） |
| 4.3.9 | **全部** | 签名提取方式不统一（Rust/Python 用 `{` / `:` 正确，其他语言脆弱） |

### 4.4 代码重复

| # | 模式 | 重复量 | 文件 |
|---|------|--------|------|
| 4.4.1 | Walk 函数骨架 | ~30×6=180 行 | 全部 6 parser |
| 4.4.2 | FileInsight 构造 | ~5×6=30 行 | 全部 6 parser |
| 4.4.3 | Entity push 样板 | ~7×6×6≈252 行 | 全部 6 parser |
| 4.4.4 | Fallback 框架 | ~3×6=18 行 | 全部 6 parser |
| 4.4.5 | 生成函数 | ~150 行 | generate/mod.rs |
| 4.4.6 | API Key 解析 | 3 份 | llm.rs×2 + embed.rs |
| 4.4.7 | JSON 文件读写 | 3 份 | config/opencode.rs |

### 4.5 安全隐患

| # | 位置 | 问题 | 严重性 |
|---|------|------|--------|
| 4.5.1 | `markdown.rs:122,140` | 模块路径未 sanitize | 路径遍历（低风险但真实） |
| 4.5.2 | `config/schema.rs:86,170` | API Key 明文存储在 TOML | 凭据泄露 |
| 4.5.3 | `config/opencode.rs:128` | Windows 用 HOME/USERPROFILE 拼接 `.config/opencode` | 配置路径错误 |
| 4.5.4 | `model/node.rs:11` | CodeNode.id 为 petgraph NodeIndex | 序列化后 ID 失效 |

---

## 五、P3 — 改进项

### 5.1 死代码

| # | 位置 | 内容 | 说明 |
|---|------|------|------|
| 5.1.1 | `mermaid.rs:66-104` | `render_entity_graph` | 从未被任何代码调用 |
| 5.1.2 | `mermaid.rs:107-138` | `render_cycle_diagram` | 从未被任何代码调用 |
| 5.1.3 | `unity.rs:24,64` | `UnityEnricher` | 从未被分析层调用 |
| 5.1.4 | `model/node.rs:82-96` | `NodeKind::priority()` | 未被任何代码引用 |
| 5.1.5 | `generate/mod.rs:28-33` | `GenerationStats.total_tokens_used` | 定义但始终为 0 |
| 5.1.6 | `config/schema.rs` | `tree-sitter-java` Cargo.toml 依赖 | 声明但从未使用（已删除？） |
| 5.1.7 | `card.rs:124-128` | `module_type` 始终返回 `"module"` | 分支逻辑全部死代码 |
| 5.1.8 | `typescript.rs:13,23` | `js_lang` 字段 | 设置为 ts_lang.clone() 从未使用 |
| 5.1.9 | `wiki.rs:126` | `#[allow(dead_code)]` on `make_test_chunk` | 应在 tests 块内 |
| 5.1.10 | `chunk.rs:134-138` | `_file_stem` 计算 | 赋值后从未读取 |

### 5.2 架构优化

| # | 建议 | 当前 | 改进 |
|---|------|------|------|
| 5.2.1 | 共享 tokio 运行时 | 每次 `run_pipeline()` 新建 | 应用级全局运行时 |
| 5.2.2 | 共享 parser walk 基础设施 | 6 次重复 | 共享的树遍历宏或 trait 默认方法 |
| 5.2.3 | 并行扫描 + 解析 | 顺序执行 | rayon::par_iter() |
| 5.2.4 | 模块检测算法 | 硬编码聚类阈值 | 可配置/自适应阈值 |
| 5.2.5 | 自定义 Provider trait | Provider 枚举 + match | `dyn LlmProvider` 注册表 |
| 5.2.6 | 错误类型 | anyhow 全部使用 | 自定义错误枚举 |
| 5.2.7 | Cargo features | 无 | `json-output`、`watch`、`no-llm` |
| 5.2.8 | 持久 ID | CodeNode.id = petgraph 索引 | 独立 UUID |

### 5.3 可观测性缺口

| # | 缺口 | 影响 |
|---|------|------|
| 5.3.1 | 无 LLM 调用日志（提示/响应/延迟/token 用量） | 无法调试生成质量问题 |
| 5.3.2 | 无进度报告回调 | 大型仓库用户看不到进展 |
| 5.3.3 | 无取消机制 | 扫描/生成期间无法优雅退出 |
| 5.3.4 | 无干运行模式 | 无法在不调用 LLM 的情况下预览生成计划 |
| 5.3.5 | `total_tokens_used` 始终为 0 | 无法按成本监控 LLM 使用 |
| 5.3.6 | watch 模式无事件日志 | 用户看不到文件监听的实时反馈 |

### 5.4 配置可改进点

| # | 建议 | 原因 |
|---|------|------|
| 5.4.1 | 添加配置版本字段 | 支持未来迁移 |
| 5.4.2 | 支持 Ollama/LocalAI provider | 本地部署场景 |
| 5.4.3 | 支持多 API Key 回退链 | 提高可靠性 |
| 5.4.4 | EmbedSection 字段与 LlmSection 严重重复 | 提取公共 ProviderSection |
| 5.4.5 | `llm.temperature` 无范围校验 | 应为 0.0~2.0 |
| 5.4.6 | `scope.include`/`exclude` 格式验证 | 配置加载时报错非静默丢弃 |
| 5.4.7 | `.opencode/plugins/repo-wiki.ts` 硬编码 `.repo-wiki/cards` | 应与配置的 output.dir 同步 |
| 5.4.8 | OpenCode 插件无事件钩子 | 缺少 onFileChange 等 |

### 5.5 跨平台

| # | 问题 | 位置 |
|---|------|------|
| 5.5.1 | config_dir() 用 HOME/USERPROFILE 而非 dirs crate | opencode.rs:127-132 |
| 5.5.2 | LLM 错误响应可能泄露 API Key | llm.rs:179-187 |
| 5.5.3 | TS 插件使用同步 execSync 阻塞事件循环 | repo-wiki.ts:23-29 |
| 5.5.4 | Anthropic 无 max_tokens 时默认 4096 | llm.rs:286 |
| 5.5.5 | Anthropic base_url 硬编码无法自定义 | llm.rs:306 |
| 5.5.6 | reqwest client 无超时设置 | llm.rs:110 |

---

## 六、修复优先级（按依赖图排序）

### 第一优先（零依赖，可并行修复）

1. **P0-5**: `generate/mod.rs:90` — 用 `HashMap<usize>` 替代 Vec 索引对齐
2. **P0-6**: `impact.rs:29` — `fp.contains` → `fp == cfp` 精确路径匹配
3. **P0-7**: `impact.rs:81` — 添加 `if !neighbor_node.module_path.is_empty()` 保护
4. **P0-8**: `watch.rs:121` — 空 include 列表保护
5. **P0-9**: `lib.rs:94-107` — 无变更时保留现有输出
6. **P0-10**: `main.rs:73` — `output: _` → 使用 `output` 参数
7. **P0-11**: `graph.rs:325` — 添加单词边界检查（`\b` 或字符类检查）
8. **P0-12**: `generate/mod.rs:44-46` — Custom provider 使用 base_url
9. **P0-3**: `dependency.rs` — 替换为 `petgraph::algo::tarjan_scc`

### 第二优先（依赖较少）

1. **P0-1**: `chunk.rs:47-61` — 构建 NodeId→file_path 映射
2. **P0-2**: `graph.rs:207-228` — `name_map` 改为 `HashMap<String, Vec<NodeId>>`
3. **P0-4**: `card.rs:53-62` — `Semaphore` 限制并发
4. **P3.1.3-4**: `prompt.rs` — 将源代码传递给 LLM
5. **P3.1.5**: `chunk.rs:68-72` — 基于图边计算依赖

### 第三优先（需较大重构）

1. 提取共享 parser walk 基础设施
2. 消除 run_generation / run_generation_filtered 代码重复
3. 消除 GitDiff / FileWatch 策略代码重复
4. 消除 API Key 解析 3 处重复
5. 迁移 Provider 枚举到 `dyn LlmProvider` + 注册表模式

---

## 七、最终评级

```
代码质量分布（按严重性）：
  P0（灾难性） ██████░░░░░░  12（15%）
  P1（严重）   ██████████░░  24（30%）
  P2（中等）   ████████████  35（44%）
  P3（改进）   ████████████  40+（50%）

注：一个问题可能影响多个级别，代码行约为 6,000 行
```

### 核心结论

**架构愿景优秀，实现质量存在显著差距**：

- **做对了**：LanguageProcessor trait、petgraph 知识图谱抽象、LLM provider trait、prompt 模板系统、tree-sitter 多语言支持（6 种语言）、Embedding 引擎、文件监听框架
- **做错了**：12 个 P0 bug 使核心功能（模块关联、增量匹配、调用边、并发控制、增量更新状态）在生产中**完全不可用或静默损坏**
- **最大的单一问题**：源代码完全不传递给 LLM（`prompt.rs`），生成的文档只是关于元数据的元数据

### 新发现（相对 v1 报告新增）

本次审查新增发现的关键问题包括：
- P0-5: cards/chunks 索引对齐断裂 — 最隐蔽的 bug
- P0-6/7: impact.rs 两个致命 bug（子串路径匹配 + 无边界 panic）
- P0-8: watch.rs 空列表 panic
- P0-9: 增量更新清空输出（数据丢失）
- P0-10: --output 参数完全无效（用户界面 bug）
- P0-11: build_call_edges 子串误报（全局错误数据）
- P0-12: Custom provider 错误映射（配置无效）
- P1: 多处性能问题（3 倍 collect_node_names、双倍 count_edges、完整图克隆）
- P2: 大量错误处理缺口（10+ 处静默错误吞掉）
- P3: 9 处死代码（含 3 处从未被调用的功能函数）
