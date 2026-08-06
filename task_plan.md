# task_plan.md — 两缺陷修复（v29）

> 端到端业务流程清单（正常路径/异常路径/边界），实施时逐条对照。

## 缺陷 1：lint entity-coverage 模块名误报（P3）

### 业务流程
1. lint() 读取主语言目录页面 + api.md。
2. api_known_entities 从 api.md 的 `- \`` 实体行提取**叶子实体**集合 known。
3. check_entity_coverage 对每页 `- \`` 声称行提取实体名，不在 known → 报 entity-coverage。
4. **缺陷**：合成页（architecture.md 等）引用 api.md 的 `## <模块名>` 节标题（如 `src`、`src::storage`），模块名是容器名、不在叶子清单 → 误报。
5. **修复**：新增 api_module_names（`## ` 节标题集合）纳入已知名：
   - 声称行**原文**精确命中模块名 → 放行（多段名 `src::storage` 提取后会被截断为 `src`，必须原文匹配）；
   - 提取后的实体名命中 known 或模块名 → 放行；
   - 其余声称不在 known → 仍报 entity-coverage（防幻觉语义不变）。
6. 防回归测试：含 `## src` / `## src::storage` 的 api.md + 引用 `src`、`src::storage` 且含编造名 `GhostEntity` 的页面 → 仅报 GhostEntity。

### 边界
- `### x` 不以 `## ` 开头（第三个字符是 `#`）→ 不误收子标题。
- 既有测试 test_lint_entity_coverage_detects_fake（`## m` + FakeEntity）行为不变。
- stale-entity（check_stale_entities）仍只认叶子实体，不受影响。

## 缺陷 2：单目录仓库退化保护

### 业务流程
1. detect_communities_with_resolution 收集 File 节点 → file_dir_key 分组到 dirs（BTreeMap）。
2. dirs.len() >= MIN_DIRS_FOR_SUPERNODE(24) → 目录页路径（每目录一社区）。
3. **缺陷**：单目录仓库（全部文件平铺同一目录，如 src/）dirs.len()==1 → 走实体级 Leiden → 整库聚成 1-2 个社区或每文件一社区，模块划分失去意义。
4. **修复**：分流条件改为 `dirs.len() <= 1 || dirs.len() >= MIN_DIRS_FOR_SUPERNODE` —— 单目录直接走目录页路径（单一社区 = 全部文件，模块名 = community_name 公共目录前缀 = 目录名）。
5. 下游兼容（ModuleDetector::detect）：单社区 → 一个 ModuleCluster（名如 `src`）→ api.md `## src` → 模块页 src.md，与缺陷 1 修复语义互相自洽；<root> 根目录散文件同样整体一社区（命名落 module_n 回退，语义不变）。
6. 防回归测试：10 文件全在 dir00/（无跨文件边）→ 断言 1 社区含全部 10 文件 + 确定性；根目录 5 散文件 → 整体 1 社区。

### 边界
- file_nodes 为空/单文件：既有早退路径不变（0 或 1 社区）。
- 多目录小仓库（2..23 目录）：仍走实体级 Leiden，行为不变。
- 既有测试（basic/stable_order/阈值上下）全部保持原路径。

## 验证顺序
1. cargo test --lib output::lint（含新测试）
2. cargo test --lib analysis::community（含新测试）
3. cargo test --lib（全量 lib 套件）
4. cargo test（29 套件，含集成）
5. cargo clippy --all-targets 0 警告
6. cargo machete
