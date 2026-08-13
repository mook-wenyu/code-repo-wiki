# 运维指南：bench 基线重录 runbook

> 目的：为「文档生成质量优化」建立可复现的评测基线。
> 关键前置结论：程序**没有「评测配置版本」字段**——跨次可比靠「commit 钉死 + config 快照 + 二进制 rev 记录」的文档约定保证（见第 3 节）。
> 本文命令与常量基于当前二进制核证；需在有网络 + LLM key 的环境实际执行（见第 6 节）。

## 1. 前置条件

- 目标是 git 仓库，且**工作区干净**——Update Recall 回放前有 `git reset --hard` 安全闸，脏工作区直接拒绝（src/main.rs:1338 核证）。
- **先 generate 过**：TQS 依赖 `.code-repo-wiki/.state/export_snapshot.json`（旧文档集）；Completeness@K 依赖 `.code-repo-wiki/.search/text_index.db`（缺失时对应维度降级跳过并显式标注，不中断）。
- 配好 LLM key：`OPENCODEGO2_API_KEY` 必需（judge 维度缺 key 跳过）；`BAILIAN_API_KEY` 可选（缺失语义相关降级）。
- 有网络 + key 的实际环境。

## 2. 两段跑法（控制成本）

### 第一段：裁判层（推荐先跑，验证轮标准形态）

跳过 Update Recall 回放，只跑 Coverage / Doc Info / Completeness@K / lint / TQS / Rubric：

```bash
code-repo-wiki bench --root <repo> --judge --rubrics-only --json
```

- `--judge` 显式开启 TQS + Rubric LLM 裁判（judge 模型 = `config.llm.model`，见第 3 节）。
- `--rubrics-only` 与 `--repodoc` 互斥（`--repodoc` 五维含 Update Recall 回放）；因此第一段用 `--judge --rubrics-only`，不要用 `--repodoc`。
- `--json` 输出机器可读报告，保存为基线文件（如 `baselines/<repo>-<rev>.json`）。

### 第二段：Update Recall（可选，成本高）

需要回放数字时才跑完整模式：

```bash
code-repo-wiki bench --root <repo> --judge --json
```

- Update Recall 回放最近至多 `MAX_RECALL_COMMITS=20` 个 commit，每次回放触发一次全量生成（强制 mock provider，无 token 成本但墙钟长）——这是评测墙钟的主导成本。
- 大仓库评估成本后决定是否跑第二段；`--reference <path>`（可重复传）注入人工参考材料供 judge 对照，防止凭空打分。

## 3. 固定配置约定（可复现性）

程序无「评测配置版本」字段（src/bench/mod.rs、src/bench/manifest.rs 核证），跨次可比必须记录以下四项，每次重录与上次**同配置**，否则不可比：

1. **judge 模型**：`config.toml` 的 `[llm] model`（默认 `deepseek-v4-flash`；judge 无独立配置项，直接用 `config.llm.model`——src/bench/mod.rs:1458/1869 核证）。
2. **`[llm] thinking` 建议 `false`**：deepseek-v4 默认启用 thinking，裁判调用慢约 5×；显式 `thinking = false` 提速且不影响打分（可选但**建议固定**）。
3. **二进制 rev**：`git rev-parse HEAD`，写入基线记录（prompt/行为随二进制变化）。
4. **config.toml 快照**：`cp config.toml <基线目录>/config.toml`，或记录与上次的 diff。

被测仓库 commit 也需固定：单仓库 `bench` 用干净工作区的当前 HEAD（记录 `git rev-parse HEAD`）；多仓库横向对比用 manifest 第三列钉死（见第 5 节）。

其他已硬编码、无需记录的参数：temperature 无配置项（硬编码）；`BENCH_MAX_OUTPUT_TOKENS=16384`。

## 4. 判定「优化有效」

- **配对比较**：同一仓库、同一配置下，优化前后各跑一次（rubrics-only 即可），关键维度 delta 需**超过 judge 噪声带**，不能只看方向。
- **judge 噪声带**：LLM-as-judge 单次判定翻转率均值约 13.6%（t04 实测）；TQS 多数投票 5 轮约 90% 保真、95% 需平均 11 轮（TQS_REPEATS=5，低置信升级 11，src/bench/mod.rs:359–365 核证）。
- **人工标定 Cohen's Kappa**：对同一批模块人工打分与 judge 打分比对，κ ≥ 0.6 视为可用的参考口径（RepoDocBench 实践）；单次打分不可信（exact-match 高估 33–41pp κ）。
- Rubrics 全量打分仅作趋势参考，不承诺与人工评审一致（[限制项](../reference/limitations.md)）。

## 5. 与 manifest 的关系

- 多仓库横向对比用 `bench-manifest`：

```bash
code-repo-wiki bench-manifest --manifest bench/production-manifest.txt --work-dir <目录> --json
```

- manifest 格式与字段见 [bench/production-manifest.txt](../../bench/production-manifest.txt)（模板，commit 列需填真实值，禁止编造）。
- `template-config.toml`：清单跑分运行期自动写入 `--work-dir` 的模板配置**脱敏快照**（llm/embed 的 api_key 置空，防明文落盘）；评测进程使用内存合并后的真实凭据。
- 清单模式跳过 Update Recall 回放与 LLM 裁判（四快维度）；深度评测用单仓库 `bench --judge`。

## 6. 沙箱说明

实际跑 bench 需要网络 + LLM key：judge 维度真实调用 LLM、Update Recall 执行 git 回放 + 全量生成。**必须在有网络 + key 的环境执行**；沙箱/离线环境无法代跑基线。
