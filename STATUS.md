# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无
- 测试覆盖率：cargo check --all-targets 0 错误 0 警告；cargo clippy --all-targets -- -D warnings 0 警告；cargo test 216 通过 0 失败（166 lib + 50 集成，14 套件）
- 代码量：约 10,100 行 / 55 .rs 文件

## 二、本次变更影响范围
- 修改的功能：深度分析遗留缺陷修复——跨文件调用边、模块检测阈值、概览断链、api.md/模块图空、CLI 调用链补全
- **跨文件调用边（最重大）**：build_call_edges 原逐文件构建（name_map 仅含已处理文件）+ 匹配载体仅 signature/doc → 真实图 Calls 边仅 2 条；改为全图收集实体+函数体文本（行号切片），主循环后统一构建 → Calls 边 2→6125
- **模块检测阈值**：Calls 修复后子模块 coupling 全 >0.7 被拒（模块 3→1）；删除硬阈值，目录前缀分组（Rust 目录=模块约定），cohesion/coupling 降为描述性元数据 → 检出 10 模块与目录一一对应
- **概览/架构断链**：target_path 用 replace("::","/") vs 写盘 join("_")；模块页 title 末段 vs 完整模块名 → wiki.rs 三处统一 + title 改完整模块名；真实产物断链消失
- **api.md/模块图空**：ModuleCluster.node_ids 仅 File 节点 → detect() 输出扩展为 File+实体（api.md 56B→159KB）；父子模块重叠 + insert 后写覆盖 → or_insert 先到先得（call-graph 空→3305B）
- **CLI 调用链补全**：execute_search Hybrid 分支重建 graph + build_call_index 注入 SearchAgent；main.rs JSON 输出补 callers/callees；真实搜索 authenticate 返回 callers=["main"]
- **e2e 断言更新**：模块化后删除只重写模块页内容而非删页面
- 摸到的文件：src/analysis/{graph,module}.rs、src/generate/wiki.rs、src/output/mermaid.rs、src/lib.rs、src/main.rs、tests/test_cli.rs、tests/test_e2e.rs、tests/test_overview.rs
- 是否改变了接口/契约：execute_search 增加 graph 依赖（Hybrid 分支）；SearchHit JSON 新增 callers/callees 字段；ModuleCluster.node_ids 语义扩展（含实体）

## 三、已知风险点（由AI诚实自曝）
- 函数体文本匹配含 callee_name 可能误报（字符串/注释中的名字）；extract_body 行号切片在嵌套函数/宏下准确性未验证
- 模块检测父子重叠是目录聚类固有语义，跨模块实体计数有重复；模块图按"最深层归属"消歧
- "目录=模块"在非 Rust 扁平结构语言下可能过度细分（未验证）
- 04 真实 LLM 验证仍阻塞（OPENAI_API_KEY 缺失）；B1-B6 运行时项需真实会话
- 工作树 10 文件修改待提交（含 default-config.toml 改 deepseek 配置、.opencode/opencode-swarm.json 删除等环境改动，非本目标）

## 四、下次最该做的事（AI建议）
1. 提交本轮 5 缺陷修复 + 3 新增防回归测试（10 文件）
2. 配置 OPENAI_API_KEY 后执行真实 LLM generate 验证（04 解阻塞）
