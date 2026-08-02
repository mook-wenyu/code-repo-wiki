# repo-wiki 深度评估报告

> 生成日期：2026-08-02
> 方法：本地全量代码检索（54 个 src 文件 / 17 个测试文件 / benches）+ 网络检索（Karpathy LLM Wiki 范式、ACL 2026 CodeWiki 论文、10+ 同类工具一手资料）+ 差距对比分析
> 依据：本地真源为准；网络资料仅作参考与对标

---

## 一、需求梳理：Repo Wiki / LLM Wiki 技术全景（2026 年 8 月现状）

### 1.1 范式源头：Karpathy LLM Wiki（2026-04，本领域公认范式）

Karpathy 的 gist 定义了"持久化 wiki"范式，核心主张：**知识编译一次，保持更新，而非每次查询重新推导**（对比 RAG 的 retrieve-and-forget）。

- 三层架构：`raw sources`（人类只读不改）/ `wiki`（LLM 完全拥有的 markdown 页面层）/ `schema`（CLAUDE.md/AGENTS.md 中约束 LLM 行为的工作流规则，被称为"最关键的配置文件"）。
- 三个操作：**ingest**（新来源 → 总结页 + 更新相关实体/概念页 + 更新 index + 追加 log，单来源可触碰 10-15 个页面）、**query**（先读 index 再深挖页面，好答案回填为页面）、**lint**（定期健康检查：矛盾、过期声明、孤儿页、缺失概念、交叉引用缺口）。
- 两个索引文件：`index.md`（内容目录，问答入口）+ `log.md`（追加式操作记录，前缀一致即可用 `grep` 解析）。
- 关键论断：人类放弃知识库是因为**簿记负担增长速度超过价值**；LLM 消除了维护成本，这是 wiki 能存续的原因。

**社区演进（LLM Wiki v2，benmillerat）**：在原范式上补充了知识生命周期机制——置信度评分（来源数 × 最近确认 × 矛盾惩罚，随时间衰减）、supersession（新声明显式替代旧声明并保留旧版）、遗忘曲线（未访问事实降权而非删除）、实体提取与类型化关系（"A caused B，3 个来源，置信度 0.9"）、图遍历查询、RRF 融合（BM25 + 向量 + 图结构三路融合）、自愈 lint、矛盾自动解决（按来源新旧/权威/支持数排序）、敏感数据过滤、完整审计轨迹。

### 1.2 代码 Wiki 工具谱系（2026 年竞争格局）

| 工具 | 技术路线 | 增量策略 | 消费方式 | 差异化卖点 |
|------|---------|---------|---------|-----------|
| **DeepWiki**（闭源 SaaS） | LLM 全仓提示 | 无 | Web | 生态位标杆 |
| **CodeWiki**（ACL 2026） | 分层分解 + 递归 agent + 多模态合成 | 无 | Markdown/HTML | 论文级基准 CodeWikiBench，68.79% vs DeepWiki 64.06% |
| **RepoWiki**（he-yufeng，同名竞品） | 6 语言 import 解析 + PageRank | SQLite 内容哈希缓存 | CLI/Web/JSON | `repowiki chat` 终端问答（TF-IDF，无嵌入服务） |
| **Secrin** | Neo4j 知识图谱 + LLM 摘要 | **git post-commit hook 自动增量** | CLI/chat | 图谱 + 向量混合检索 + 领域提取 |
| **deepforge** | tree-sitter → SQLite 图 → LLM | Resync 按钮 | K8s pod/Docsify | prompt caching 省 ~60% 成本，成本 $0.5-1.5/40 页 |
| **kiwiskil** | AST 确定性提取 + 每符号一行描述 | **pre-commit hook，只重索引变更文件** | markdown + skill 文件 | **只写结构性事实，不写散文**——客户端 LLM 自己下结论 |
| **repositories-wiki** | 三 LLM 层级（Planner/Explorer/Builder）+ tree-sitter 签名 | agent skill 增量维护 | **MCP server + AGENTS.md** | 面向 AI agent 消费，上下文层 |
| **GraphRAG-MCP** | tree-sitter + Leiden + sqlite-vec + FTS5 | - | **MCP** | 零 LLM 成本本地语义 |
| **code-graph-rag** | Memgraph 图谱 + UniXcoder 本地嵌入 | - | MCP/CLI | 意图搜索 + ast-grep 结构搜索 + Cypher |

### 1.3 关键行业结论（直接影响本项目决策）

1. **纯全仓提示（whole-repo prompting）不可扩展**：OpenDeepWiki 47.13% / deepwiki-open 50.05%，大仓库显著退化（CodeWiki 论文 Table 1）——必须依赖分层分解或结构化中间表示。
2. **图谱/结构中间层是共识**：除 kiwiskil 外几乎所有工具都构建 AST 级知识图谱；差异只在存储（Neo4j/Memgraph vs SQLite）。
3. **增量是硬需求**：所有成熟工具都有增量机制（git hook、内容哈希缓存、agent skill），全量重建被明确视为缺陷。
4. **消费端在向 AI agent 迁移**：MCP server / AGENTS.md / skill 文件成为新生态位（repositories-wiki、kiwiskil、code-graph-rag、GraphRAG-MCP 四家独立押注）。
5. **本地嵌入免 key 可行**：UniXcoder（code-graph-rag）、ONNX ort 模型（GraphRAG-MCP）均可完全本地生成代码嵌入，不依赖 OpenAI API。
6. **质量评测开始标准化**：CodeWikiBench 提供了分层 rubric + agent 判分的基准协议，68.79% 是当前公开最高分。
7. **成本控制有成熟手法**：Anthropic prompt caching 省 ~60%（deepforge）；"结构事实先行 + 少量 LLM" 是低成本路线（kiwiskil）。

---

## 二、当前项目现状评估（本地真源证据）

### 2.1 架构评估

六层架构（ingest → analysis → generate → output → incremental → search）依赖方向总体干净，lib.rs 承担编排（1,030 行，全项目最大文件）。与 CONTEXT.md 声明的边界对比，有 **2 处实现偏差**：

| 声明 | 实际 | 位置 | 评估 |
|------|------|------|------|
| generate 层只依赖 analysis 结果，不直接访问 parser | generate 直接使用 `FileInsight` | generate/mod.rs:16 | 合理需要（分块需源码文本），但文档失实 |
| output 是纯渲染 | output 反向依赖 generate 的 `collect_languages` | output/mod.rs:17-19 | 轻微违规，可共享常量消除 |

### 2.2 代码质量评估（综合：良好，债务集中在复制粘贴）

- **强项**：0 unsafe；0 TODO/FIXME；panic 宏 0；错误处理主流 Result（anyhow 82 处 + bail! 21 处）；失败隔离意识强（LLM 失败进 failed_modules、索引失败仅告警、特征聚类降级纯结构）；路径规则单一来源防漂移；关键 bug（Windows 路径分隔符、tokio Semaphore panic、模块检测未接线）均系统性修复并留痕。
- **债务**：
  - **复制粘贴 3 处**：7 个语言 parser 的 walk/fallback 骨架逐字复制（每文件 ~100-130 行，csharp.rs:38 / java.rs:14 / go.rs:14 前 15 行完全一致）；双 LLM provider（OpenAI/Anthropic）的 retry+SSE 逻辑双写（llm.rs:135-270 vs :326-460）；全量/增量双流水线骨架重复 ~150 行（lib.rs:136-217 vs :269-376）。
  - **3 处 panic 期望值**：`community.rs:124` 与 `feature.rs:171` 的 Leiden expect——外部库失败直接 panic 整条流水线，无降级；`lib.rs:40` 全局 Runtime expect（几乎不可能失败，风险低）。
  - **进程级全局状态**：`scan_and_parse` 以 `std::env::current_dir()` 为扫描根（ingest/mod.rs:13），导致 bench 需 CWD_LOCK Mutex 串行化；未来任何并发调用方（服务化/MCP）都会踩坑。
  - **性能**：Hybrid 搜索每次调用全量重建图（lib.rs:778-783 自述 ~1.2s）；SemanticEngine 向量全量载入内存做余弦，索引大时内存放大（search/semantic.rs）。

### 2.3 测试评估（强项）

17 个集成测试文件 + 194 个内嵌单测 ≈ 260 个测试；E2E 用真实 git2 仓库双提交构造；确定性测试锁死产物集合（已修复 deps 排序非确定性 bug）；watch 测试用真监听 + 轮询 + 超时 kill。clippy -D warnings 0 警告、cargo machete 零未使用（STATUS.md 自述，代码证据一致）。

**唯一弱点**：`tests/fixtures/sample-repo/config.toml` 仍是真实 provider（openai + mock key 无 base_url），直接复用会真实触网——测试隔离隐患。

### 2.4 已知限制（项目自曝 + 分析发现）

1. 删除清理对无目录社区 `module_{n}` 档漏删（lib.rs:397-399 注释）。
2. 特征聚类 embedding 路径（0.5 语义权重）无 key 未真实验证，只验证了纯结构降级。
3. 真实 LLM 大仓库全量生成端到端产物未验证；watch 端到端未验证。
4. CPM γ=0.5 / seed=42 是小仓库调参结果，万级文件仓库粒度未实测。
5. `MAX_DIFF_LINES=10_000` 硬编码回退阈值。
6. 实体变化分类依赖旧 commit 可解析（git2 依赖）。

---

## 三、差距分析：对照最佳实践的五大核心差距

### 差距 1（最大）：没有 AI agent 消费通道——错失当前最大生态位

业界 4 家独立押注 MCP server / AGENTS.md / skill 文件（repositories-wiki、kiwiskil、code-graph-rag、GraphRAG-MCP）。kiwiskil 甚至为每个仓库生成 agent skill 文件（find_module/get_symbol/trace_callers/what_changed/entry_points 五个工具）。**本项目已有全部底层数据**（图谱、三引擎、call graph、增量状态），却只能通过 CLI/本地 markdown 消费。

> 影响：工具与 AI agent 工作流隔离；图谱的调用链/社区信息无法被 agent 直接利用；无法成为"代码库上下文层"（repositories-wiki 的定位：agent 会话开始时读 wiki 而非重扫代码，省 ~90% 上下文 token）。

### 差距 2：没有质量评测基准——无法宣称/验证文档质量

CodeWiki 论文给出了可复用的基准协议（分层 rubric + agent 判分，7 语言，68.79% 公开最高分）。本项目 wiki_lint 只做结构健康（孤儿页/断链/过时），**没有对生成文档的语义质量评测**。没有评测，就无法回答"我们的 wiki 比 DeepWiki/CodeWiki 好还是差"，也无法指导 prompt/分块/聚类调参。

> 影响：T0-T5 的所有质量相关优化（分块、聚类、并行）都缺乏量化验收手段。

### 差距 3：缺知识生命周期机制（LLM Wiki v2 的核心演进）

项目有 supersession 的雏形（人工修改保护、增量状态持久化），但缺：置信度评分、矛盾标记与检测（现有 lint 是结构性的，不查"两页对同一主题说法矛盾"）、来源引用追踪到页面级、过期声明自动降权。Karpathy 原范式的 lint 操作在本项目中只有结构版，**语义 lint 完全缺失**。

### 差距 4：消费侧体验缺失——无阅读路径、无 Web/交互式消费

RepoWiki 的 PageRank 阅读路径、kiwiskil 的 entry_points（无调用者的符号 = 架构根）、deepforge 的 Docsify 站点。本项目有 _toc.md 和 mermaid 图，但没有"从哪开始读"的引导，没有 Web UI。

### 差距 5：嵌入/语义搜索依赖外部 API，且未验证

业界已证明本地嵌入完全可行（UniXcoder 768 维本地推理、ONNX ort + sqlite-vec）。本项目 SemanticEngine 需要 API key，导致：CI 不可测、feature 聚类语义路径从未真实验证（STATUS.md:21 自曝）、用户必须额外配 key 才能用核心搜索能力。

### 其他可对标项（差距小或已覆盖）

| 维度 | 业界最佳 | 本项目 | 状态 |
|------|---------|--------|------|
| AST 确定性提取 | tree-sitter | tree-sitter 7 语言 | ✅ 对齐 |
| 图结构中间层 | Neo4j/Memgraph/SQLite | petgraph 内存 + SQLite 索引 | ✅（持久化可选） |
| 增量更新 | git hook 自动 | git diff 实体级 + watch | ✅ 对齐（CLI 手动触发，可加 hook） |
| 三引擎融合 | BM25+向量+图 RRF | FTS5 + 语义 + AST + CallGraph | ✅ 对齐 |
| 多模态产物 | 架构/数据流/时序图 | mermaid 图 | ✅ 基本对齐 |
| prompt caching 省成本 | deepforge ~60% | 无 | ⚠️ 可加 |
| LLM provider 生态 | litellm 100+ | OpenAI/Anthropic/Mock/Custom | ⚠️ 够用 |
| 删除清理 | - | module_n 档漏删 | ⚠️ 已知 |

---

## 四、深度反思与决策建议

### 4.1 定位反思（最重要的问题）

当前项目处于一个十字路口，两条路线的投入方向完全不同：

- **路线 A：文档质量路线**（CodeWiki/DeepWiki 方向）——投入评测基准、prompt 工程、分层生成，目标是把生成文档质量做到可验证的 SOTA。
- **路线 B：agent 上下文层路线**（repositories-wiki/kiwiskil 方向）——投入 MCP server、skill 文件、增量自动维护，目标是成为"代码库编译好的知识层"，让 AI agent 会话不再重扫代码。

**推荐**：路线 B 为主 + 少量 A 的评测组件。理由：(1) 本项目已有增量、三引擎、call graph、确定性测试这些 agent 消费层的基础设施，缺的只是出口；(2) 文档质量路线需要大量 LLM 预算与评测工程，竞争者是 DeepWiki 和论文级团队；(3) kiwiskil 证明了"结构事实 + 少量 LLM 描述"就能解决 agent 上下文问题，与本项目现有架构（确定性 AST 为主）同构。唯一需要补的 A 组件是**自建小评测**（用 CodeWikiBench 协议对自己 3-5 个测试仓库跑分），用于验证和调参。

### 4.2 路线图（按优先级）

**P0（正确性/安全，1-2 天）**
1. 两处 Leiden expect 改为 Result 传播 + 降级（community.rs:124、feature.rs:171）——外部库失败不应 panic 整条流水线。
2. `tests/fixtures/sample-repo/config.toml` 改 mock provider（当前真实触网风险）。
3. CONTEXT.md 修正两处边界失实（generate/mod.rs:16、output/mod.rs:17）。

**P1（生态位差异化，3-5 天）**
4. **MCP server**：暴露现有能力——`wiki_read_index` / `wiki_search`（三引擎）/ `trace_callers`（call graph）/ `what_changed`（增量状态）。实现量小（现有工具函数全在），收益最大（对接 Claude Code/Cursor 生态）。
5. **AGENTS.md / skill 生成**：仿 kiwiskil 在 wiki 输出中附带 agent 引导文件（何时查 wiki、何时更新）。
6. **entry_points / 阅读路径**：无调用者符号 = 架构根（数据已在图中，只差查询输出）。

**P2（工程债务，1 周）**
7. parser walk 去重：宏或生成器统一 7 语言骨架（最大重复面，~800 行）。
8. 全量/增量双流水线合并（lib.rs 两段 ~150 行重复）。
9. LLM provider 统一 retry/SSE 抽象。
10. `scan_and_parse` 扫描根改为参数注入，去 CWD_LOCK。

**P3（知识质量，持续）**
11. 语义 lint：LLM 检查跨页矛盾/过期声明（对照 Karpathy lint 操作）。
12. 自建评测：CodeWikiBench 协议（分层 rubric + agent 判分）对 3-5 个仓库跑分，建立质量基线。
13. 本地嵌入选项：UniXcoder / ONNX ort（免 key，解锁 feature 聚类语义路径的验证）。
14. git hook 自动增量（secrin/kiwiskil 已验证的模式）。

### 4.3 明确不建议做（YAGNI）

- 换 Neo4j/Memgraph：当前规模 SQLite + 内存图足够，图数据库是运维负担。
- Web UI 完整版：除非有明确用户；先以 MCP 覆盖交互需求。
- 遗忘曲线/置信度机制：个人知识库场景才需要，代码 wiki 的"真相"在源码里，过期由 git diff 增量天然解决。
- 30+ 语言支持：7 语言已覆盖主流，语言的边际价值递减（CodeWiki 论文也确认系统语言是难点而非语言数量）。

---

## 五、结论摘要

- **工程质量**：显著高于同类开源工具平均水平——无 unsafe、260+ 测试、真实 git e2e、确定性锁、系统性 bug 修复留痕。T0-T5 演进计划完成度高。
- **架构**：六层边界基本干净（2 处文档失实），最大债务是复制粘贴（~800 行 parser 骨架 + 双 provider + 双流水线）。
- **最大战略差距**：无 AI agent 消费通道（MCP/skill），错失当前行业最大生态位；无质量评测基准，无法量化验证。
- **最大操作风险**：fixture 配置真实触网 + 2 处 Leiden panic 无降级。
- **推荐**：以"agent 上下文层"为定位，P0 修 3 个正确性项 → P1 做 MCP server + entry_points（现有基础设施的最后一公里）→ P2 清债务 → P3 补语义 lint 与自建评测。

---

## 附：信息来源清单

- Karpathy LLM Wiki gist（范式源头）：https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f
- LLM Wiki v2（生命周期演进）：https://gist.github.com/benmillerat/537cd1251225cb58ef9b242212528633
- CodeWiki / CodeWikiBench（ACL 2026 Findings）：https://aclanthology.org/2026.findings-acl.288.pdf 、https://github.com/FSoft-AI4Code/CodeWiki
- RepoWiki（he-yufeng，同名竞品）：https://github.com/he-yufeng/RepoWiki
- Secrin（Neo4j + git hook）：https://github.com/secrinlabs/secrin
- deepforge（prompt caching / K8s）：https://github.com/wisecoders/deepforge
- kiwiskil（结构事实优先 / pre-commit / skill）：https://github.com/TheLunarLogic/kiwiskil
- repositories-wiki（三 LLM 层级 / MCP / AGENTS.md）：https://github.com/eliavamar/repositories-wiki
- GraphRAG-MCP（本地 Leiden + sqlite-vec + FTS5）：https://github.com/kim-nam-jung/GraphRAG-mcp
- code-graph-rag（UniXcoder 本地嵌入 / Memgraph）：https://github.com/vitali87/code-graph-rag
- LLM Wiki 实现指南（vanja.io，含 QMD 混合检索）：https://vanja.io/the-knowledge-base-that-builds-itself/
