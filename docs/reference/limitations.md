# 限制项

## 并发

同一输出目录**并发运行不被支持**（状态/快照/缓存无锁，最后写入者胜；CI/编辑器集成请串行调用）。v36 起有单实例运行锁（`.state/run.lock`）：并发启动的后到者显式报错（含锁路径与人工删除指引），不再静默互相踩写。

## 语言与成本

- 每种语言都独立生成会线性放大 LLM 成本——当前只输出主语言（`wiki.language`，默认 `zh`）。
- **语言覆盖**：tree-sitter 解析支持 Rust/TypeScript/TSX/Python/Go/JS/JSX/MJS/CJS/C#/Java 11 种；**无 Ruby/PHP 解析器**（rails 等 Ruby 仓库只能解析其 JS/TS 资产；纯非支持语言仓库零源文件会显式报错「未找到任何源文件」——扫描是全量自动识别，无 include 白名单配置）。

## 技术栈解析边界

技术栈卡（`cards/{lang}/project/tech-stack.md`）的依赖清单采用**零 LLM 确定性解析**（防幻觉），支持六类清单：`Cargo.toml`/`Cargo.lock`/`package.json`/`pyproject.toml`/`requirements.txt`/`go.mod`。**不支持 XML 形态清单（`pom.xml`/*.csproj）**——理由：确定性解析为求极简不引入 XML 解析依赖（YAGNI），XML 清单不在支持范围。

## 扫描边界

- 内置噪音目录整棵跳过：`node_modules`/`vendor`/`Pods`/`Library` 等依赖与缓存目录任意深度命中即剪。
- **仅根级剪枝的目录**：`dist`/`build`/`out`/`bin`/`obj`/`Packages`/`Temp`/`Logs` 只在仓库根直接子目录出现时排除，嵌套同名目录（如 `src/bin/`、`src/Packages/`）按普通源码目录保留。
- **Unity 工程（v51/13.4）**：根级 `Packages/`（UPM 第三方包，常含数百个 `.cs`）、`Temp/`（编译缓存）、`Logs/`（编辑器日志）默认排除；嵌套同名目录不受影响（仅根级生效）。

## 大仓边界

- 单项目上限 **10 万个源文件**（超出显式报错）。
- 单次变更超过 **1 万行**自动回退全量生成。
- 实测（mock LLM，v30）：cal.com（5048 文件/5.9 万实体）全量约 6.2 分钟；图构建在万级实体仓库是主要耗时（v32 增量索引优化后 287s→20s）；真实 LLM 成本随仓库规模线性放大。

## 评测边界

`bench --judge` 是参考型口径（文档生成质量自评），判定受 LLM-as-judge 稳定性影响（judge 三态 + abstain + tie 升级阈值已缓解，flip 率已知项）；rubrics 全量打分仅作趋势参考，不承诺与人工评审一致。

## 语义搜索降级

embed Key 缺失或运行期失败时自动降级为纯文本搜索，`search`/`status` 显式提示「语义索引已降级」；降级状态持久化到 `semantic_degraded` 标记，下次成功生成自动清除。

## 已知问题（2026-08）

默认 LLM 端点 `https://opencode.ai/zen/go/v1` 曾出现上游临时拒绝（网关生成端点 400/500）——**已于 2026-08-10 实测恢复**（真实 `generate` 端到端成功，17 页产物）。若再遇 400/500：先重试一次（上游波动）；持续失败时按[配置参考的已知问题段](config.md#已知问题)切换兼容端点（阿里百炼）。

## 历史形态差异（v37 改名）

- 二进制/命令名：v37 起 `code-repo-wiki`（此前 `repo-wiki`），两者不共存。
- 产物目录：v37 起 `.code-repo-wiki/`（此前 `.repo-wiki/`），改名时不迁移，删除重生成。
- 用户级配置目录：v41 起 `~/.code-repo-wiki`（Windows `%USERPROFILE%\.code-repo-wiki`，自动迁移旧目录内容）；v37–v40 为 `%APPDATA%\code-repo-wiki\`（此前 `%APPDATA%\repo-wiki\`，改名时不迁移，删除重装并重新配置 key）。
