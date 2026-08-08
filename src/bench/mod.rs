//! 评测基准自动层（U10，对齐 RepoDocBench 协议的可落地子集）
//!
//! `repo-wiki bench` 对目标仓库跑五维自动评测，输出 Markdown/JSON 报告：
//!
//! 1. **Coverage 实体提及率**：AST 提取实体清单（复用 ingest 解析），
//!    统计每个实体在 Wiki 产物中的被提及数 → 提及/总数（RepoDoc 定义：
//!    文档覆盖 public API 的比例）。
//! 2. **Doc Info 文本统计**：页面数 / 词数 / 交叉引用数 / 代码块数 /
//!    Mermaid 图数（全部确定性统计，不调用 LLM）。
//! 3. **lint 健康**：复用 lint 6 类检查（孤儿页/断链/过时/bad-citation/
//!    entity-coverage/bad-mermaid），问题数即质量分。
//! 4. **Update Recall 增量召回**：git commit 回放（最多 20 个），逐个
//!    checkout 后跑增量更新，统计"有源码变更的 commit 中成功触发
//!    重生成的占比"——增量链路正确性的可复现指标（对齐 RepoDoc 的
//!    Update Recall：正确更新的组件 / 需更新的组件）。
//! 5. **Time 耗时**：扫描/增量生成各阶段耗时（LLM 侧用 mock provider，
//!    耗时反映流水线确定性开销，与模型无关）。
//!
//! LLM 裁判层（TQS 打分）在 U11，需真实 API key，独立子命令。
//!
//! 第二档评测（v21 E 组）：`bench-manifest` 多仓库清单批量跑分见
//! [`manifest`] 子模块（仓库×维度矩阵，mock 可跑）。

pub mod manifest;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::schema::WikiConfig;
use crate::generate::llm::LlmProvider;
use crate::project::ProjectRoot;

/// 增量回放的最大 commit 数（对齐 RepoDoc 每仓库 20 commit 协议）
const MAX_RECALL_COMMITS: usize = 20;

/// 评测裁判 LLM 调用的输出预算上限。
///
/// 推理型模型（deepseek-v4-flash）的 reasoning 会消耗输出预算，预算
/// 不足时响应可能只有 reasoning 块没有 message（实测 4000 复现、
/// 8192 起才出现完整 message），rubric 树/叶子判定等长结构化输出
/// 必须显式给足预算（v22 rubrics 首跑 3+3 轮全败的根因）。
const BENCH_MAX_OUTPUT_TOKENS: u32 = 16384;

/// v14 C 组（MVVP 缺口）：模块级复测标准差超过该阈值（0-10 分尺度）即
/// 判为低置信——分数波动过大，该模块的 TQS 结论不可信需人工复核
const LOW_CONFIDENCE_STD_THRESHOLD: f64 = 2.0;

/// 评测报告（Markdown 与 JSON 的公共数据源，JSON 由 serde 直出）
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// 仓库名（报告标识，缺省取 root 目录名）
    pub repo_name: String,
    /// 评测时间（ISO 8601）
    pub generated_at: String,
    pub coverage: CoverageReport,
    pub doc_info: DocInfoReport,
    pub lint: LintReport,
    pub update_recall: UpdateRecallReport,
    pub time: TimeReport,
    /// v32 8.1：生成流水线分段计时（update_recall 回放后从
    /// .state/last_timings.json 读取；无回放/无文件时 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<crate::GenerationTimings>,
    /// TQS 裁判打分（--judge 启用且 LLM 可用时 Some；否则 None）
    pub tqs: Option<TqsReport>,
    /// 维度 7：Rubric 层级完整性打分（--judge 启用且 LLM 可用时 Some）
    pub rubric: Option<RubricReport>,
    /// v32（6.3 FR-104）：Completeness@K 文档可检索性（五维对齐 RepoDocBench）
    pub completeness: CompletenessReport,
}

/// 维度 1：实体覆盖率
#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    /// 实体总数（AST 解析产出，去重）
    pub total_entities: usize,
    /// 在产物中被提及的实体数
    pub covered_entities: usize,
    /// 覆盖率 = covered / total（total 为 0 时为 1.0 空集约定）
    pub ratio: f64,
}

/// 维度 2：文本统计（+ v32 6.2：LLM 信息性判定并存）
#[derive(Debug, Clone, Serialize)]
pub struct DocInfoReport {
    /// 产物页面数（wiki/{lang}/*.md）
    pub pages: usize,
    /// 全部页面词数（空白分隔，去 markdown 围栏不影响统计口径）
    pub words: usize,
    /// 交叉引用链接数（[文本](目标) 形态）
    pub cross_references: usize,
    /// 代码块数（``` 围栏对）
    pub code_blocks: usize,
    /// Mermaid 图数
    pub diagrams: usize,
    /// v32（6.2 FR-101）：LLM 信息性判定是否执行——LLM 不可用时
    /// 降级跳过（false），报告显式标注而非静默
    #[serde(default)]
    pub llm_judged: bool,
    /// v32（6.2 FR-101）：LLM 信息性评分（0-10，已判定页面平均；
    /// abstain 页面不计入分母——FR-102 exclude 模式）
    #[serde(default)]
    pub llm_score: f64,
    /// v32（6.2）：LLM 判定成功的页面数
    #[serde(default)]
    pub llm_judged_modules: usize,
    /// v32（6.2）：LLM 判定 abstain 的页面数（uncertain 重试后仍不确定
    /// 或调用/解析失败；报告暴露 abstain 数——FR-102）
    #[serde(default)]
    pub llm_abstain_modules: usize,
}

/// 维度 3（v32 6.3 FR-104）：Completeness@K 文档可检索性
///
/// RepoDocBench 五维之一：实体的文档可检索性——用实体名检索 text 索引
/// （FTS5 BM25）top-K 条目，命中判定=任一条目所属模块与实体所属模块
/// 相同，且该模块页存在于产物。语义是「能否通过检索找到实体的模块页」，
/// 与 Coverage（提及率）互补：提及率高但检索命不中 = 文档难导航。
#[derive(Debug, Clone, Serialize)]
pub struct CompletenessReport {
    /// 实体总数（AST 解析去重后，与 Coverage 同源）
    pub total_entities: usize,
    /// 命中实体数（top-K 检索命中所属模块页）
    pub hit_entities: usize,
    /// K 值（text 索引检索条目数上限，FR-104 固定 10）
    pub k: usize,
    /// 命中率 = hit / total（total 为 0 时 1.0 空集约定）
    pub ratio: f64,
    /// v32（FR-101）：text 索引缺失/不可用时降级跳过（false），
    /// 报告显式标注而非静默
    #[serde(default)]
    pub judged: bool,
}

/// 维度 3：lint 健康
#[derive(Debug, Clone, Serialize)]
pub struct LintReport {
    /// 问题总数（0 = 健康）
    pub total_issues: usize,
    /// 各类问题计数（kind → 数量）
    pub by_kind: std::collections::BTreeMap<String, usize>,
}

/// 维度 4：增量召回
#[derive(Debug, Clone, Serialize)]
pub struct UpdateRecallReport {
    /// 回放的 commit 数（≤ MAX_RECALL_COMMITS）
    pub commits_scanned: usize,
    /// 有源码变更的 commit 数（前置条件：无变更 commit 不要求触发）
    pub commits_with_changes: usize,
    /// 成功触发重生成的 commit 数（documents 非空）
    pub correctly_updated: usize,
    /// 召回率 = correctly / with_changes（with_changes 为 0 时为 1.0 空集约定）
    pub recall: f64,
}

/// 维度 5：耗时
#[derive(Debug, Clone, Serialize)]
pub struct TimeReport {
    /// 扫描 + 解析耗时（毫秒）
    pub scan_ms: u64,
    /// 增量更新流水线耗时（毫秒）
    pub generate_ms: u64,
    /// 总耗时（毫秒）
    pub total_ms: u64,
}

/// 维度 7：Rubric 层级完整性打分（v14 C 组，CodeWikiBench 协议，--judge 启用时）
///
/// 协议（arXiv:2510.24428）：从被测仓库 README + docs/ 提取 docs_tree →
/// LLM 独立生成 3 份层级 rubrics（requirement + weight 1-3 + 递归 sub_tasks）
/// → 第 4 次调用语义合并（名称相似度 >70% 的节点合并，权重取均值）→
/// 裁判对每个叶子以产物为证据做 0/1 满足判定 → 加权自底向上聚合：
/// S(n) = Σw(c)·S(c)/Σw(c)；叶子 σ 按二项近似 sqrt(p(1-p))，非叶子
/// σ² = Σ(w²σ²)/Σw² 传播；Coverage = 满足叶子数 / 总叶子数。
/// 与 TQS 并存：TQS 是相对对比（新旧文档），rubric 是绝对质量（对
/// 仓库意图的覆盖度）。mock/无 key 时 None（"失败只告警"策略）。
#[derive(Debug, Clone, Serialize)]
pub struct RubricReport {
    /// 合并后的 rubric 节点总数（含非叶子）
    pub rubric_nodes: usize,
    /// 叶子（可判定项）数
    pub leaf_count: usize,
    /// 判定为满足（1）的叶子数
    pub satisfied_leaves: usize,
    /// 覆盖率 = satisfied / 有效判定叶子（leaf − abstain；0 时为 1.0
    /// 空集约定）——t04 起 abstain 叶子排除出分母（2606.00093 item 8：
    /// exclude 模式报 covered-subset 性能）
    pub coverage: f64,
    /// 加权总分 S（0-1；×10 可与 TQS 0-10 口径对比）
    pub score: f64,
    /// 加权标准差 σ_R（二项近似 + 权重传播）
    pub score_std: f64,
    /// 生成轮次（独立生成 3 次 + 合并 1 次 = 4 次 LLM 调用）
    pub generation_calls: usize,
    /// 裁判模型（config.llm.model）
    pub judge_model: String,
    /// t04：abstain 叶子数（判定调用/解析失败——不再 recode 为 false；
    /// 2606.00093 item 6：recode 改变 estimand，排除需显式报告）
    #[serde(default)]
    pub abstain_leaves: usize,
    /// t04：abstain 率 = abstain / 总叶子（2606.00093 item 7：
    /// abstention/tie/invalid 率本身作为指标报告）
    #[serde(default)]
    pub abstain_rate: f64,
    /// t04：叶子判定轮数（3 次多数投票；1:2 争议升级 5 次——2606.13685：
    /// 单次判定保真仅 86.6%，3 trials 达约 90%）
    #[serde(default)]
    pub leaf_verdict_repeats: usize,
    /// t04：聚合层级声明（叶子级多数投票 → 权重自底向上；2606.00093
    /// item 10 要求声明 micro/macro/item-level）
    #[serde(default)]
    pub aggregation_level: String,
}

/// Rubric 树节点（LLM 生成/合并/聚合的中间形态，仅内部使用）
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct RubricNode {
    /// 需求描述（叶子判定与合并相似度计算的文本基础）
    requirement: String,
    /// 权重（1-3，聚合加权；LLM 输出越界时 clamp）
    weight: f64,
    /// 递归子任务（空 = 叶子）
    #[serde(default)]
    sub_tasks: Vec<RubricNode>,
}

/// 聚合后的加权分数与标准差（内部形态）
struct RubricScore {
    /// 加权总分（0-1）
    score: f64,
    /// 加权 σ（二项近似 + 权重传播）
    std: f64,
    /// 该子树下叶子总数
    leaves: usize,
    /// 该子树下满足叶子数
    satisfied: usize,
}
///
/// RepoDocBench 协议：对同一模块的旧文档（导出快照）与当前产物，
/// 裁判按五维 0-10 打分（Clarity/Readability/Conciseness/Richness/
/// Structure），交换文档顺序两轮取平均消除位置偏差（position bias）。
/// 裁判模型与温度由 config.llm 决定（默认配置 mock/未配置 key 时
/// 本维度被跳过，report.tqs = None）。
#[derive(Debug, Clone, Serialize)]
pub struct TqsReport {
    /// 完成打分的模块数（旧文档与当前产物都存在的模块）
    pub judged_modules: usize,
    /// 五维平均分（0-10，顺序消偏 + 复测平均后取平均）
    pub avg_clarity: f64,
    pub avg_readability: f64,
    pub avg_conciseness: f64,
    pub avg_richness: f64,
    pub avg_structure: f64,
    /// 五维总分平均（0-10）
    pub avg_total: f64,
    /// t05/MVVP：每模块的复测次数（AB/BA 各 repeats 轮）
    pub repeats: usize,
    /// t05/MVVP：复测一致性（κ 近似）——同一模块任意两轮、同一维度
    /// 分数绝对差 ≤1 的比例。1.0 = 完全稳定；低值 = 裁判不稳定或
    /// 文档差异导致敏感（2606.19544 指出高 test-retest 与低位置偏差
    /// 可并存，一致性是可靠性下限）。
    pub kappa_like: f64,
    /// v14 C 组（MVVP 缺口）：机会校正 κ——把"同一维度分数差 ≤1"
    /// 视为二分类判定（一致/不一致），一致率经机会一致性 p²+(1-p)² 校正：
    /// κ = (P_o − P_e)/(1 − P_e)，负值截断为 0。**注意：机会基线用 p_obs
    /// 自身近似，非标准 Cohen's κ**（标准 κ 需两个独立 rater 的判定与
    /// 边际表，见 kappa_cohen）；保留字段名与语义兼容既有消费方。
    pub kappa: f64,
    /// t04：标准 Cohen's κ（2606.19544 通缩诊断的对照口径）——rater1 =
    /// AB 顺序调用、rater2 = BA 顺序调用，item = (模块, 维度, 轮)，
    /// 类别 = {A 胜, B 胜}（平局按 B 胜计入，连续分数相等概率近零）。
    /// 与 kappa_like/kappa 的自定义稳定率口径不同，独立报告。
    #[serde(default)]
    pub kappa_cohen: f64,
    /// t04：判定翻转率（模块级平均）——单次调用的 A 胜/平/B 胜判定与
    /// 模块内多数判定不一致的比例（2606.13685 单次 flip rate 13.6%；
    /// 2606.19544 self-consistency 的互补面）。
    #[serde(default)]
    pub flip_rate: f64,
    /// t04：位置翻转率（模块级平均）——同一轮内 AB 顺序与 BA 顺序的
    /// 判定相异比例（2606.19544 item 级定义：交换位置后判定翻转的比例；
    /// 区别于 position_bias 的 AB/BA 组均值口径）。
    #[serde(default)]
    pub position_flip_rate: f64,
    /// t04：κ 通缩诊断 = kappa_like − kappa——原始一致率被机会校正
    /// 削掉多少（2606.19544：Δκ 33-41pp 量级；本地无 human 参考时以
    /// 自稳定性口径替代）。
    #[serde(default)]
    pub delta_kappa: f64,
    /// t04：有效模块数 = 新旧文档都存在的模块总数（2606.00093 item 8
    /// coverage；judged_modules 仅计判定成功数，failed 模块不计入）
    #[serde(default)]
    pub eligible_modules: usize,
    /// t04：解析成功率 = 成功解析的裁判调用 / 全部调用（含失败模块；
    /// 2606.00093 item 7 要求 abstention/invalid 率单独作为指标报告）
    #[serde(default)]
    pub parse_success_rate: f64,
    /// t04：判定尺度声明（2606.00093 item 1：必须声明判定尺度）
    #[serde(default)]
    pub judgment_scale: String,
    /// t04：聚合层级声明（2606.00093 item 10）
    #[serde(default)]
    pub aggregation_level: String,
    /// t04：tie/abstain 处理声明（2606.00093 item 6：exclude/recode/retain
    /// 是三种不同 estimand，必须显式声明）
    #[serde(default)]
    pub tie_handling: String,
    /// v14 C 组（MVVP 缺口）：位置偏差 |P(A 胜) − 0.5|——对每模块每维度，
    /// AB 顺序（A 先）轮与 BA 顺序（B 先）轮的分数均值比较出 A 胜/负判定，
    /// P(A 胜) = A 胜判定数 / 判定总数。接近 0 = 文档顺序不影响裁判；
    /// 接近 0.5 = 裁判对位置敏感（分数结论可能被顺序污染）。
    pub position_bias: f64,
    /// v14 C 组（MVVP 缺口）：低置信模块清单——复测失败（裁判调用/解析
    /// 失败被跳过，不再静默）或复测标准差超过阈值的模块（分数波动大，
    /// 结论不可信；2606.19544 要求显式报告而非跳过）
    pub low_confidence_modules: Vec<String>,
    /// 五维分数的平均标准差（跨复测轮次；量化波动幅度）
    pub avg_std: f64,
    /// 裁判模型（config.llm.model；style 消偏在多裁判轮转下才完整，
    /// 单裁判时报告模型便于人工判断偏差来源——2604.23178）
    pub judge_model: String,
    /// v32（6.1 FR-102/FR-103）：三态判定中平局占比（模块级平均）——
    /// 全部调用级判定（judgment()==0）中 tie 的比例。tie 率升高 =
    /// 裁判区分度不足的信号（旧文档与产物五维总分经常相等），
    /// >TQS_TIE_ESCALATION_THRESHOLD 时升级复测轮数。
    #[serde(default)]
    pub tie_rate: f64,
    /// v32（6.1 FR-102）：三态判定明细 [A 胜, B 胜, 平局]——全部调用级
    /// 判定（judgment() 的 1/-1/0）的累计计数。tie 是独立类别而非静默
    /// 归入胜负（2606.00093 item 6 estimand 声明）；报告暴露三态结构
    /// 供人工判断裁判区分度。
    #[serde(default)]
    pub agreement_breakdown: [usize; 3],
}

/// t04：判定尺度声明（2606.00093 item 1）——0-10 连续五维点分，
/// 解析后 clamp 到 [0,10]（prompt 示例为整数，解析接受小数并收敛越界）
const TQS_JUDGMENT_SCALE: &str =
    "0-10 连续五维点分（clarity/readability/conciseness/richness/structure），解析后 clamp 到 [0,10]";

/// t04：聚合层级声明（2606.00093 item 10）——模块级 macro average
const TQS_AGGREGATION_LEVEL: &str = "模块级 macro average（每模块五维均值后跨模块平均）";

/// t04：tie/abstain 处理声明（2606.00093 item 6）——三态判定口径：
/// 平局不进胜；AB/BA 判定相异计位置翻转；2×2 一致表平局按 B 胜计入；
/// 解析/调用失败 → 模块排除（计入 low_confidence，不 recode）
const TQS_TIE_HANDLING: &str =
    "判定三态（A胜/平/B胜）：平局不进胜；AB 与 BA 顺序判定相异计位置翻转；2×2 一致表平局按 B 胜计入；解析/调用失败的模块排除并计入 low_confidence（不 recode）";

/// t04（Phase 2）：TQS 基础复测轮数（AB/BA 各 N 轮）——2606.13685：
/// 单次判定翻转率均值 13.6%，多数投票 90% 保真需约 3 trials、95% 需
/// 平均 11 trials；5 是 90%+ 区间内的性价比点
const TQS_REPEATS: usize = 5;

/// t04（Phase 2）：低置信模块的升级轮数（2606.13685：95% 保真需平均
/// 11 trials；hard 档（FR≥10%）需 15 trials，11 是成本可控的收敛近似）
const TQS_REPEATS_ESCALATED: usize = 11;

/// t04（Phase 2）：模块级判定翻转率超过该阈值即升级复测轮数
/// （2606.13685：28% 题目翻转率 >20%，hard 档需更多 trials）
const TQS_FLIP_RATE_ESCALATION_THRESHOLD: f64 = 0.20;

/// v32（6.1 FR-103）：模块级判定平局率超过该阈值即升级复测轮数——
/// tie 是独立类别（judgment()==0）而非静默计入 flip（2606.00093
/// item 6 estimand 声明）。tie 率 >30% 说明裁判区分度不足（新旧文档
/// 总分经常相等），单次判定不可信，需更多 trials 收敛。
const TQS_TIE_ESCALATION_THRESHOLD: f64 = 0.30;

/// 读取最近一次生成的分段计时（v32 8.1）
///
/// 由 run_pipeline_with_progress 在每次生成完成后写入
/// `.state/last_timings.json`；bench 的 update_recall 回放生成后读取。
/// 文件缺失/损坏（首次评测、无回放、半写）→ None，渲染层不输出该节。
fn read_last_timings(output_dir: &Path) -> Option<crate::GenerationTimings> {
    let path = output_dir.join(".state").join("last_timings.json");
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 收集全部产物页内容（wiki/{lang}/*.md，主语言 + 扩展语言）
fn collect_wiki_pages(output_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut pages = Vec::new();
    let Ok(entries) = std::fs::read_dir(output_dir.join("wiki")) else {
        return pages;
    };
    for lang in entries.flatten() {
        if !lang.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(lang.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                pages.push((path, content));
            }
        }
    }
    pages
}

/// 维度 1：实体覆盖率
///
/// 实体清单 = 全仓库 AST 解析结果（含各语言差异点：Go const/var、
/// TS enum、Java 构造器、Python 模块常量），去重后逐一在产物文本中
/// 做子串包含判定（实体名是文档必提的标识符，子串口径与 RepoDoc 的
/// AST 提及率一致——同名误报由名称唯一性控制）。
fn measure_coverage(root: &ProjectRoot, pages: &[(PathBuf, String)]) -> Result<CoverageReport> {
    let insights = crate::ingest::scan_and_parse_at(root)?.insights;
    let mut entities: Vec<String> = insights
        .iter()
        .flat_map(|i| i.entities.iter().map(|e| e.name.clone()))
        .collect();
    entities.sort();
    entities.dedup();
    let total = entities.len();

    let corpus: String = pages
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let covered = entities
        .iter()
        .filter(|name| corpus.contains(name.as_str()))
        .count();
    let ratio = if total == 0 { 1.0 } else { covered as f64 / total as f64 };
    Ok(CoverageReport { total_entities: total, covered_entities: covered, ratio })
}

/// 模块名派生（与 chunk_by_file/collect_index_items 同规则）：
/// 文件父目录的 Normal 路径组件用 "::" 连接；根目录文件为空串。
/// 两侧（实体侧与索引条目侧）共用本函数保证模块相等性判断自洽。
fn module_of(path: &std::path::Path) -> String {
    path.parent()
        .map(|p| {
            p.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("::")
        })
        .unwrap_or_default()
}

/// 维度 3：Completeness@K（FR-104）
///
/// 判定：实体名检索 text 索引（FTS5 BM25）top-K 条目中，任一条目
/// 所属模块与实体所属模块相同（两侧均经 module_of 从文件路径派生，
/// 同一规则保证相等性自洽），且该模块页存在于产物
/// （{module.join("_")}.md，与 wiki_file_name 同规则）。
///
/// 降级语义（FR-101）：text 索引缺失（未 generate 或索引不可用）时
/// judged=false 并返回，报告显式标注「未执行」；单实体检索失败（FTS5
/// 查询语法错误等特殊字符）跳过该实体不中断（与 rubrics abstain 同
/// 语义）。注意本函数独立扫描一次（与 measure_coverage 互不共享，
/// 保持两维度可独立测试；bench 非热路径，重复扫描成本可接受）。
fn measure_completeness_at_k(
    root: &ProjectRoot,
    config: &WikiConfig,
    pages: &[(std::path::PathBuf, String)],
) -> Result<CompletenessReport> {
    // FR-104：top-K = 10
    const K: usize = 10;

    // 实体清单去重并携带所属模块（与 coverage 同源，口径一致）
    let insights = crate::ingest::scan_and_parse_at(root)?.insights;
    let mut entities: Vec<(String, String)> = insights
        .iter()
        .flat_map(|i| {
            let module = module_of(&i.path);
            i.entities.iter().map(move |e| (e.name.clone(), module.clone()))
        })
        .collect();
    entities.sort();
    entities.dedup();
    let total = entities.len();

    // text 索引缺失（未构建/被清理）→ 降级跳过（judged=false 显式标注）。
    // 判据用索引文件是否存在而非打开失败：rusqlite 在父目录存在时会
    // 自动创建空索引文件，只有文件真的缺失才代表「从未构建」，
    // 否则会误报 judged=true 但恒 0 命中的空索引（v32 6.3 审查修正）。
    let index_dir = crate::search_index_dir(config);
    let index_path = index_dir.join("text_index.db");
    if !index_path.exists() {
        tracing::warn!(
            "bench: Completeness@K 降级跳过（text 索引不存在: {}）",
            index_path.display()
        );
        return Ok(CompletenessReport {
            total_entities: total,
            hit_entities: 0,
            k: K,
            ratio: if total == 0 { 1.0 } else { 0.0 },
            judged: false,
        });
    }
    let engine = match crate::search::text::TextEngine::open(&index_path) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("bench: Completeness@K 降级跳过（text 索引不可用: {e}）");
            return Ok(CompletenessReport {
                total_entities: total,
                hit_entities: 0,
                k: K,
                ratio: if total == 0 { 1.0 } else { 0.0 },
                judged: false,
            });
        }
    };

    // 产物模块页文件名集合（wiki/{lang}/*.md 的 stem），
    // 模块页命名与 wiki_file_name 同规则（module.join("_") + ".md"）
    let mut module_page_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (path, _) in pages {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            module_page_names.insert(stem.to_string());
        }
    }

    let mut hit = 0usize;
    for (name, module) in &entities {
        // 模块页文件名：模块名（:: 连接）转下划线
        let module_page = module.replace("::", "_");
        // FTS5 查询语法错误（实体名含特殊字符）→ 跳过该实体不中断
        let Ok(hits) = engine.search(name, K) else {
            continue;
        };
        let found = hits.iter().any(|(node, _)| {
            // 索引条目与实体两侧必须用同一 module_of 规则派生模块名：
            // 索引条目的 file_path 是 insight.path 的字符串（graph::build
            // 记录实体时原样保留），经 module_of 与实体侧
            // module_of(insight.path) 完全同构，相等性自洽。
            // 不能用 node.module_path 直接比较——它包含文件 stem
            // （graph.rs:82-85 构造 dir_segments + file_stem），与实体侧
            // 父目录规则不一致，会比较恒假（v32 6.3 审查修复）。
            let node_module = node
                .file_path
                .as_deref()
                .map(|fp| module_of(std::path::Path::new(fp)))
                .unwrap_or_default();
            node_module == *module && module_page_names.contains(&module_page)
        });
        if found {
            hit += 1;
        }
    }
    let ratio = if total == 0 { 1.0 } else { hit as f64 / total as f64 };
    Ok(CompletenessReport {
        total_entities: total,
        hit_entities: hit,
        k: K,
        ratio,
        judged: true,
    })
}

/// 维度 2：文本统计
fn measure_doc_info(pages: &[(PathBuf, String)]) -> DocInfoReport {
    let mut words = 0usize;
    let mut cross_references = 0usize;
    let mut code_blocks = 0usize;
    let mut diagrams = 0usize;

    for (_, content) in pages {
        words += content.split_whitespace().count();
        // 交叉引用：[文本](目标) 形态（粗粒度统计：`](` 出现次数）
        cross_references += content.matches("](").count();
        // 代码块围栏对数：``` 行数 / 2（未闭合按 1 对计，结构损坏由 lint 暴露）
        let fences = content.lines().filter(|l| l.trim_start().starts_with("```")).count();
        code_blocks += fences.div_ceil(2);
        // Mermaid 图数：```mermaid 起始围栏数
        diagrams += content.lines().filter(|l| l.trim_start().starts_with("```mermaid")).count();
    }

    DocInfoReport {
        pages: pages.len(),
        words,
        cross_references,
        code_blocks,
        diagrams,
        llm_judged: false,
        llm_score: 0.0,
        llm_judged_modules: 0,
        llm_abstain_modules: 0,
    }
}

/// v32（6.2 FR-102）：Doc Info LLM 判定三态（评分/不确定/不可解析）
enum DocInfoVerdict {
    Score(f64),
    Uncertain,
    Unparseable,
}

/// v32（6.2 FR-101）：Doc Information 的 LLM 判定维度——逐页裁判
/// 信息性评分（0-10，与文本统计并存）。LLM 不可用时降级跳过
/// （judged=false，报告显式标注不静默）；每页 uncertain 重试一次，
/// 仍不确定计 abstain（FR-102 三态协议；abstain 页面不计入评分分母）。
struct DocInfoLlmOutcome {
    judged: bool,
    score: f64,
    judged_modules: usize,
    abstain_modules: usize,
}

/// v32（6.2）：Doc Info 信息性裁判 prompt——要求 0-10 评分；
/// 页面过少/与模块无关时允许输出 uncertain（证据不足显式声明，
/// 不猜测——与 rubric 三态同协议）
fn doc_info_judge_prompt(module: &str, summary: &str) -> Vec<crate::generate::llm::Message> {
    vec![
        crate::generate::llm::Message::system(
            "你是 Wiki 文档信息性裁判。判断模块文档页是否提供了关于该模块的实质信息（职责/实体/关系/用法示例）。只输出 JSON：{\"score\": 0-10}。若页面内容过少或与模块无关，输出 {\"verdict\": \"uncertain\"}，不要猜测。",
        ),
        crate::generate::llm::Message::user(format!(
            "模块：{}\n\n--- 页面内容 ---\n{}",
            module, summary
        )),
    ]
}

/// v32（6.2）：解析 Doc Info 判定输出——{"score": 0-10} → Score（clamp
/// 到 [0,10] 收敛越界，与 TQS 口径一致）；{"verdict": "uncertain"} →
/// Uncertain；其他 → Unparseable（不计入评分分母）
fn parse_doc_info_score(content: &str) -> DocInfoVerdict {
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = match serde_json::from_str(stripped) {
        Ok(v) => v,
        Err(_) => return DocInfoVerdict::Unparseable,
    };
    if let Some(s) = value.get("score").and_then(|v| v.as_f64()) {
        return DocInfoVerdict::Score(s.clamp(0.0, 10.0));
    }
    if value.get("verdict").and_then(|v| v.as_str()) == Some("uncertain") {
        return DocInfoVerdict::Uncertain;
    }
    DocInfoVerdict::Unparseable
}

/// v32（6.2 FR-101/FR-102）：Doc Info LLM 判定（逐页评分，uncertain
/// 重试一次后 abstain；任一调用失败只影响该页不计中断）
fn measure_doc_info_llm(
    config: &WikiConfig,
    pages: &[(PathBuf, String)],
) -> DocInfoLlmOutcome {
    let provider = match crate::generate::create_provider(config) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Doc Info LLM 判定跳过（LLM 不可用）: {e}");
            return DocInfoLlmOutcome {
                judged: false,
                score: 0.0,
                judged_modules: 0,
                abstain_modules: 0,
            };
        }
    };
    let rt = crate::get_global_runtime();
    let mut total = 0.0f64;
    let mut judged_n = 0usize;
    let mut abstain_n = 0usize;
    for (path, content) in pages {
        let module = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // 页面正文截断（api/overview 等大页 token 成本可控；
        // 判定依据为信息性而非逐字内容）
        let summary = truncate(content, 8000);
        // FR-102 三态协议：uncertain 重试一次，仍不确定计 abstain
        let mut uncertain_retried = false;
        loop {
            let messages = doc_info_judge_prompt(&module, &summary);
            match rt
                .block_on(provider.complete_with_budget(&messages, Some(BENCH_MAX_OUTPUT_TOKENS)))
            {
                Ok(out) => match parse_doc_info_score(&out) {
                    DocInfoVerdict::Score(s) => {
                        total += s;
                        judged_n += 1;
                        break;
                    }
                    DocInfoVerdict::Uncertain => {
                        if !uncertain_retried {
                            uncertain_retried = true;
                            continue;
                        }
                        tracing::warn!("Doc Info 判定重试后仍 uncertain（计 abstain）: {module}");
                        abstain_n += 1;
                        break;
                    }
                    DocInfoVerdict::Unparseable => {
                        tracing::warn!("Doc Info 判定解析失败（计 abstain）: {module}");
                        abstain_n += 1;
                        break;
                    }
                },
                Err(e) => {
                    tracing::warn!("Doc Info 判定调用失败（计 abstain）: {e}");
                    abstain_n += 1;
                    break;
                }
            }
        }
    }
    DocInfoLlmOutcome {
        judged: true,
        score: if judged_n == 0 { 0.0 } else { total / judged_n as f64 },
        judged_modules: judged_n,
        abstain_modules: abstain_n,
    }
}

/// 维度 3：lint 健康（复用 lint 6 类检查，问题数即质量分）
fn measure_lint(output_dir: &Path, root: &ProjectRoot) -> LintReport {
    // 源码根必须 root 化：lint 内部以相对路径扫描时基于进程 cwd 解析，
    // --root 指向其他仓库时会把 cwd 误当源码根（v21 修复过 CLI lint/status/
    // update 三处与 mcp，此处是 bench 的遗漏——实测 --root 场景下 stale-entity
    // 检查扫错目录，实体表与引用键失配，区间重叠检查静默失效同源问题）。
    // v30+：扫描范围硬编码，源码根恒为仓库根。
    let source_roots = crate::commands::source_roots(root);
    let issues = crate::output::lint::lint(output_dir, &source_roots);
    let mut by_kind: std::collections::BTreeMap<String, usize> = Default::default();
    for issue in &issues {
        *by_kind.entry(issue.kind.to_string()).or_default() += 1;
    }
    LintReport { total_issues: issues.len(), by_kind }
}

/// 维度 4：增量召回（git commit 回放）
///
/// 逐 commit checkout 到工作区 → 跑增量更新（mock provider，不触网）→
/// 记录该 commit 是否有源码变更（git diff 判定）以及是否成功触发重生成
/// （run_pipeline 返回 documents 非空 = 有页面被重生成）。
/// 召回率 = 触发重生成的变更 commit / 有变更的 commit。
/// 边界：非 git 仓库返回空集（recall = 1.0 空集约定）；commit 不足 20 个
/// 按实际数量回放；checkout 失败（脏工作区/文件冲突）跳过该 commit 并告警。
fn measure_update_recall(
    config_path: Option<&Path>,
    root: &ProjectRoot,
) -> Result<UpdateRecallReport> {
    let repo = match git2::Repository::open(root.path()) {
        Ok(r) => r,
        Err(_) => {
            // 非 git 仓库：增量回放无 commit 可循（与 get_head_commit_hash_at
            // 的非 git 空值语义一致），报告空集而非报错
            tracing::warn!("bench: 非 git 仓库，增量召回维度跳过");
            return Ok(UpdateRecallReport {
                commits_scanned: 0,
                commits_with_changes: 0,
                correctly_updated: 0,
                recall: 1.0,
            });
        }
    };

    // 回放安全闸：工作区必须干净。回放用 git reset --hard 逐 commit
    // 回滚工作区与 HEAD，未提交改动会被**直接吞噬且无法恢复**（实测事故：
    // 脏工作区跑 bench 导致全部未提交改动丢失）。因此评测前强制检查，
    // 有未提交改动即拒绝执行——安全边界优先于评测便利性，宁可拒绝也不
    // 静默破坏用户数据（与"禁止兜底掩盖 bug"同源：这里是禁止兜底掩盖
    // 数据丢失）。
    let statuses = repo
        .statuses(None)
        .context("bench: 读取 git 状态失败")?;
    // 过滤被忽略条目：git reset --hard 只回滚 tracked 内容，ignored
    // 未跟踪文件（产物目录/依赖缓存等）不受回放影响，不构成数据丢失风险
    //（实测事故是 tracked 未提交改动被吞，与被忽略文件无关）
    let dirty: Vec<_> = statuses
        .iter()
        .filter(|e| !e.status().contains(git2::Status::IGNORED))
        .collect();
    if !dirty.is_empty() {
        // 拒绝时列出条目（前 10 条）：安全闸宁可错杀不可放过（回放会
        // reset --hard 丢弃未提交改动，实测事故），但条目明细能帮助用户
        // 判断是什么（未跟踪目录/被忽略文件误报等）
        let detail: Vec<String> = dirty
            .iter()
            .take(10)
            .map(|e| {
                let path = e.path().unwrap_or("(unknown)");
                let mut tags = Vec::new();
                if e.status().contains(git2::Status::INDEX_NEW) { tags.push("已暂存新增"); }
                if e.status().contains(git2::Status::WT_NEW) { tags.push("未跟踪"); }
                if e.status().contains(git2::Status::WT_MODIFIED) { tags.push("已修改"); }
                if e.status().contains(git2::Status::WT_DELETED) { tags.push("已删除"); }
                if e.status().contains(git2::Status::IGNORED) { tags.push("被忽略"); }
                format!("{} [{}]", path, tags.join(","))
            })
            .collect();
        anyhow::bail!(
            "评测前工作区必须干净（存在 {} 个未提交改动），请先 git commit 或 stash 后再运行 bench——回放会 reset --hard，未提交改动将被丢弃。改动明细: {}",
            dirty.len(),
            detail.join("; ")
        );
    }

    // 收集最近 MAX_RECALL_COMMITS 个 commit（revwalk 从 HEAD 起）
    let mut commits = Vec::new();
    if let Ok(mut walk) = repo.revwalk() {
        walk.push_head().ok();
        for oid in walk.flatten().take(MAX_RECALL_COMMITS) {
            if let Ok(commit) = repo.find_commit(oid) {
                commits.push(commit);
            }
        }
    }
    commits.reverse(); // 从旧到新回放

    let mut scanned = 0usize;
    let mut with_changes = 0usize;
    let mut correctly_updated = 0usize;

    // 记录原 HEAD（回放结束恢复；git2 reset 移动 HEAD 后，增量 diff 的
    // 基准（状态 last_commit_hash → HEAD）与回放 commit 对应，才能验证
    // 增量链路；仅 checkout_tree 不移动 HEAD，diff 基准恒为最新 commit，
    // 回放会得到错误的"无变更"短路）
    let original_head = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.id());
    // t03：守卫接管 HEAD 恢复职责（panic/中断也恢复，见 HeadRestoreGuard）
    let _head_guard = HeadRestoreGuard::new(&repo, original_head);

    for (i, commit) in commits.iter().enumerate() {
        let commit_id = commit.id();
        // reset --hard 到该 commit：工作区与 HEAD 均恢复（回放语义，
        // 每次回放到干净的 commit 状态；产物/状态一并回滚，由 update 重建）
        let obj = match repo.find_object(commit_id, None) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("bench: commit {commit_id} 对象解析失败，跳过: {e}");
                continue;
            }
        };
        if let Err(e) = repo.reset(&obj, git2::ResetType::Hard, None) {
            tracing::warn!("bench: commit {commit_id} reset 失败，跳过: {e}");
            continue;
        }
        scanned += 1;

        // 有源码变更判定：相对前一 commit 的 diff（首个 commit 视为基线）
        // tree()/diff 失败时显式告警并按"有变更"保守计入（低估召回率会
        // 虚高评测分，高估则更接近真实——错误可见而非静默假数据）
        let has_changes = if i == 0 {
            false
        } else {
            let prev_tree = match commits[i - 1].tree() {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!("bench: commit {} tree 读取失败，按有变更计入: {}", commits[i - 1].id(), e);
                    None
                }
            };
            let cur_tree = match commit.tree() {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!("bench: commit {} tree 读取失败，按有变更计入: {}", commit_id, e);
                    None
                }
            };
            matches!(prev_tree.as_ref().zip(cur_tree.as_ref()), Some((a, b)) if {
                match repo.diff_tree_to_tree(Some(a), Some(b), None) {
                    Ok(d) => d.deltas().len() > 0,
                    Err(e) => {
                        tracing::warn!("bench: commit {commit_id} diff 计算失败，按有变更计入: {e}");
                        true
                    }
                }
            })
        };
        if has_changes {
            with_changes += 1;
        }

        // 增量更新（mock provider）：documents 非空 = 触发重生成
        let result = crate::run_pipeline(
            config_path,
            None,
            false,
            root,
            &crate::GenerationMode::Incremental {
                watch_paths: Vec::new(),
                change_kind: None,
            },
        );
        match result {
            Ok(res) if !res.documents.is_empty() => {
                if has_changes {
                    correctly_updated += 1;
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("bench: commit {commit_id} 增量更新失败（跳过判定）: {e}");
            }
        }
    }

    // 恢复原始 HEAD（回放期间 reset 移动了 HEAD，恢复避免污染用户仓库）。
    // 正常路径显式恢复并传播错误——恢复失败必须让用户知道（仓库停留在
    // 回放 commit）；_guard 的 Drop 兜底处理 panic/中断路径（见下方结构体）。
    if let Some(oid) = original_head {
        let obj = repo
            .find_object(oid, None)
            .with_context(|| "回放后解析原 HEAD 失败")?;
        repo.reset(&obj, git2::ResetType::Hard, None)
            .with_context(|| "回放后恢复原 HEAD 失败（用户仓库停留在回放 commit）")?;
    }

    let recall = if with_changes == 0 { 1.0 } else { correctly_updated as f64 / with_changes as f64 };
    Ok(UpdateRecallReport {
        commits_scanned: scanned,
        commits_with_changes: with_changes,
        correctly_updated,
        recall,
    })
}

/// t03/P1-3：回放期间的 HEAD 恢复守卫（RAII）
///
/// 回放用 reset --hard 逐 commit 移动 HEAD；若回放中途 panic/中断
/// （内部 panic、Ctrl+C），流程末尾的显式恢复不会执行，用户仓库会停在
/// 回放 commit——与实测事故（U01-U10 被回放吞噬）同源的危险路径。
/// 本守卫在 Drop 中无条件恢复原始 HEAD：Drop 无法传播错误，恢复失败
/// 仅告警（至少不静默）；正常路径仍在函数末尾显式恢复并传播错误。
struct HeadRestoreGuard<'repo> {
    repo: &'repo git2::Repository,
    original: Option<git2::Oid>,
}

impl<'repo> HeadRestoreGuard<'repo> {
    /// 创建守卫并立即接管恢复职责（在回放循环之前调用）
    fn new(repo: &'repo git2::Repository, original: Option<git2::Oid>) -> Self {
        Self { repo, original }
    }
}

impl Drop for HeadRestoreGuard<'_> {
    fn drop(&mut self) {
        if let Some(oid) = self.original
            && let Ok(obj) = self.repo.find_object(oid, None)
            && let Err(e) = self.repo.reset(&obj, git2::ResetType::Hard, None)
        {
            tracing::warn!("bench: 回放后恢复 HEAD 失败（Drop 兜底路径）: {e}");
        }
    }
}

/// 运行自动层评测，返回报告
///
/// `config_path` 为目标仓库的配置文件路径（增量回放复用同一份配置，
/// 注意：回放会 checkout 目标仓库的 git commit——这是评测语义的一部分，
/// 运行前请确认工作区无未提交改动（脏工作区会跳过对应 commit）。
/// `judge` 为 true 时追加 TQS 裁判打分维度（需 LLM API key；快照缺失
/// 或 LLM 不可用时该维度返回 None，不中断其他维度）。
pub fn run_bench(
    config_path: Option<&Path>,
    root: &ProjectRoot,
    config: &WikiConfig,
    repo_name: &str,
    judge: bool,
) -> Result<BenchReport> {
    let start = Instant::now();

    let scan_start = Instant::now();
    let pages = collect_wiki_pages(config.output_dir());
    let coverage = measure_coverage(root, &pages)?;
    let scan_ms = scan_start.elapsed().as_millis() as u64;

    let mut doc_info = measure_doc_info(&pages);
    // v32（6.2 FR-101）：Doc Information LLM 判定维度与文本统计并存
    let llm_info = measure_doc_info_llm(config, &pages);
    doc_info.llm_judged = llm_info.judged;
    doc_info.llm_score = llm_info.score;
    doc_info.llm_judged_modules = llm_info.judged_modules;
    doc_info.llm_abstain_modules = llm_info.abstain_modules;
    // v32（6.3 FR-104）：Completeness@K 文档可检索性（text 索引缺失降级）
    let completeness = measure_completeness_at_k(root, config, &pages)?;
    let lint = measure_lint(config.output_dir(), root);

    let gen_start = Instant::now();
    let update_recall = measure_update_recall(config_path, root)?;
    let generate_ms = gen_start.elapsed().as_millis() as u64;
    // v32 8.1：回放生成后读取分段计时（无回放/文件缺失 → None 不渲染）
    let timings = read_last_timings(config.output_dir());

    let tqs = if judge {
        measure_tqs(config)?
    } else {
        None
    };
    // v14 C 组：维度 7 Rubric（docs_tree 缺失/LLM 不可用 → None 不中断）
    let rubric = if judge {
        measure_rubrics(config, root)?
    } else {
        None
    };

    Ok(BenchReport {
        repo_name: repo_name.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        coverage,
        doc_info,
        lint,
        update_recall,
        time: TimeReport {
            scan_ms,
            generate_ms,
            total_ms: start.elapsed().as_millis() as u64,
        },
        timings,
        tqs,
        rubric,
        completeness,
    })
}

/// 运行纯裁判层评测（--rubrics-only）：只执行快维度（Coverage/Doc Info/lint）
/// 与 LLM 裁判维度（TQS/Rubric），**跳过 Update Recall 的 git commit 回放**。
///
/// 适用场景：对大型仓库（数万文件）跑分时 Update Recall 回放成本不可接受
/// （每次回放都触发真实生成），而裁判打分只需当前产物与快照即可完成。
/// 返回的 `update_recall` 为「跳过」占位（commits_scanned=0/with_changes=0/
/// correctly_updated=0/recall=1.0 空集约定），`time.generate_ms=0`；
/// 渲染层 `render_markdown` 对无回放的 recall 会标注「跳过（--rubrics-only）」。
pub fn run_rubrics_only(
    root: &ProjectRoot,
    config: &WikiConfig,
    repo_name: &str,
) -> Result<BenchReport> {
    let start = Instant::now();

    let scan_start = Instant::now();
    let pages = collect_wiki_pages(config.output_dir());
    let coverage = measure_coverage(root, &pages)?;
    let scan_ms = scan_start.elapsed().as_millis() as u64;

    let mut doc_info = measure_doc_info(&pages);
    // v32（6.2 FR-101）：rubrics-only 模式同样跑 Doc Info LLM 判定
    let llm_info = measure_doc_info_llm(config, &pages);
    doc_info.llm_judged = llm_info.judged;
    doc_info.llm_score = llm_info.score;
    doc_info.llm_judged_modules = llm_info.judged_modules;
    doc_info.llm_abstain_modules = llm_info.abstain_modules;
    // v32（6.3 FR-104）：Completeness@K（text 索引缺失降级，无 LLM 成本）
    let completeness = measure_completeness_at_k(root, config, &pages)?;
    let lint = measure_lint(config.output_dir(), root);

    // Update Recall 回放成本不可接受（v21 D 组）：大仓库跳过，
    // 语义上等价于"快照缺失"——回放入口（run_bench）仍可单独跑。
    tracing::info!("bench --rubrics-only: 跳过 Update Recall git 回放");
    let update_recall = UpdateRecallReport {
        commits_scanned: 0,
        commits_with_changes: 0,
        correctly_updated: 0,
        recall: 1.0,
    };

    let tqs = measure_tqs(config)?;
    let rubric = measure_rubrics(config, root)?;

    Ok(BenchReport {
        repo_name: repo_name.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        coverage,
        doc_info,
        lint,
        update_recall,
        time: TimeReport {
            scan_ms,
            generate_ms: 0,
            total_ms: start.elapsed().as_millis() as u64,
        },
        // rubrics-only 跳过 git 回放（无生成），不读取分段计时
        timings: None,
        tqs,
        rubric,
        completeness,
    })
}

/// TQS 打分执行：对每个"旧文档（快照）与当前产物都存在"的模块页，
/// 两轮裁判（顺序 AB/BA）取五维平均。LLM 不可用（create_provider 失败）
/// 或快照缺失时返回 None（与自动层"失败只告警"策略一致，不中断评测）。
fn measure_tqs(config: &WikiConfig) -> Result<Option<TqsReport>> {
    // 快照 = 旧文档集（上次生成意图）；当前产物从磁盘读
    let snapshot_path = crate::output::export_snapshot_path(config.output_dir());
    let Ok(snapshot_content) = std::fs::read_to_string(&snapshot_path) else {
        tracing::warn!("TQS 跳过：导出快照不存在（先运行 generate 落盘快照）");
        return Ok(None);
    };
    let snapshot: crate::output::ExportSnapshot = serde_json::from_str(&snapshot_content)
        .with_context(|| "解析导出快照失败")?;
    // 只评模块页（WikiPage）；旧文档按 title 索引
    let old_docs: std::collections::HashMap<String, String> = snapshot
        .documents
        .iter()
        .filter(|d| matches!(d.kind, crate::model::DocumentKind::WikiPage))
        .map(|d| (d.title.clone(), d.content.clone()))
        .collect();

    // 新文档 = 磁盘产物：title → wiki/{lang}/{title.replace("::","_")}.md
    let mut pairs: Vec<(String, String, String)> = Vec::new(); // (title, old, new)
    for title in old_docs.keys() {
        let page_path = crate::output::wiki_page_path(
            config.output_dir(),
            &config.wiki.language,
            &crate::model::WikiDocument {
                title: title.clone(),
                kind: crate::model::DocumentKind::WikiPage,
                content: String::new(),
                language: config.wiki.language.clone(),
                module_path: Vec::new(),
                references: Vec::new(),
                last_updated: String::new(),
                based_on_commit: None,
                fingerprint: None,
            },
        );
        if let Ok(new_content) = std::fs::read_to_string(&page_path) {
            pairs.push((title.clone(), old_docs[title].clone(), new_content));
        }
    }
    if pairs.is_empty() {
        tracing::warn!("TQS 跳过：无新旧文档都存在的模块页");
        return Ok(None);
    }

    // 裁判 LLM（config.llm 决定模型；未配置 key 时 create_provider 报错 → 跳过）
    let provider = match crate::generate::create_provider(config) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("TQS 跳过（LLM 不可用）: {e}");
            return Ok(None);
        }
    };
    let rt = crate::get_global_runtime();

    // t05/MVVP：复测次数（AB/BA 各 TQS_REPEATS 轮）。2606.19544 的 MVVP
    // 协议要求 ≥3 次复测计算可靠性——单次打分不可信（exact-match 高估
    // 33-41pp κ）。t04（2606.13685）：单次判定翻转率均值 13.6%，多数投票
    // 90% 保真需约 3 trials、95% 需平均 11 trials；基础轮数取 5（90%+
    // 区间内性价比点），低置信模块自动升级至 11（Phase 2，成本上限
    // 每模块 2×11 次调用）。
    let mut sums = [0.0f64; 5];
    let mut judged = 0usize;
    // 复测一致性（κ 近似）与标准差：跨模块累计
    let mut consistent_pairs = 0usize;
    let mut total_pairs = 0usize;
    let mut std_sum = 0.0f64;
    // v14 C 组（MVVP 缺口）：位置偏差与低置信模块
    // a_first 标记随分数记录（AB/BA 分组），position_bias 据此比较胜负
    let mut position_wins_a = 0usize;
    let mut position_pairs = 0usize;
    let mut low_confidence: Vec<String> = Vec::new();
    // 模块级复测标准差（avg_std 的模块维度来源，低置信判定用）
    let mut module_stds: Vec<(String, f64)> = Vec::new();
    // t04（Phase 1）：判定级指标跨模块累计——翻转率/位置翻转率/2×2 表
    let mut flip_sum = 0.0f64;
    let mut pos_flip_sum = 0.0f64;
    let mut kappa_table = [[0usize; 2]; 2];
    // 各模块实际复测轮数（升级补轮后 > TQS_REPEATS；t09 实测报告字段曾失真）
    let mut actual_repeats: Vec<usize> = Vec::new();
    // 解析成功率（全部调用，含失败模块；2606.00093 item 7）
    let mut parse_ok = 0usize;
    let mut parse_total = 0usize;
    // v32（6.1 FR-102/FR-103）：三态协议统计——平局率（模块级平均，
    // 与 flip_rate 同口径）与三态明细 [A 胜, B 胜, 平局] 跨模块累计
    let mut tie_sum = 0.0f64;
    let mut agreement_breakdown = [0usize; 3];
    for (title, old, new) in &pairs {
        // 每轮 = AB + BA 两次调用（顺序消偏）；共 repeats 轮；低置信升级补轮
        let mut round_scores: Vec<(bool, [f64; 5], [f64; 5])> = Vec::new();
        let mut failed = false;
        let mut target = TQS_REPEATS;
        while round_scores.len() < target * 2 {
            for a_first in [true, false] {
                parse_total += 1;
                let messages = tqs_prompt(&config.wiki.language, old, new, a_first);
                match rt.block_on(provider.complete_with_budget(&messages, Some(BENCH_MAX_OUTPUT_TOKENS))) {
                    Ok(content) => match parse_tqs_score(&content) {
                        Ok((a, b)) => {
                            parse_ok += 1;
                            round_scores.push((a_first, a, b));
                        }
                        Err(e) => {
                            tracing::warn!("TQS 裁判输出解析失败（模块 {title}）: {e}");
                            failed = true;
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::warn!("TQS 裁判调用失败（模块 {title}）: {e}");
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                break;
            }
            // Phase 2（t03）：基础轮数跑满后低置信升级——翻转率 >20%、
            // 模块 σ 超阈值（2606.13685 分层表：hard 档需更多 trials）
            // 或平局率 >30%（v32 6.1：tie 独立类别，区分度不足同样需升级）
            if round_scores.len() == TQS_REPEATS * 2
                && (module_judgment_metrics(&round_scores).flip_rate
                    > TQS_FLIP_RATE_ESCALATION_THRESHOLD
                    || module_std(&round_scores) > LOW_CONFIDENCE_STD_THRESHOLD
                    || module_tie_rate(&round_scores) > TQS_TIE_ESCALATION_THRESHOLD)
            {
                target = TQS_REPEATS_ESCALATED;
            }
        }
        let rounds = round_scores.len();
        if failed || rounds < TQS_REPEATS * 2 {
            // 复测失败：显式记录低置信模块（此前静默跳过，2606.19544
            // 要求失败可见——裁判不可用本身就是评测结论的一部分）
            low_confidence.push(title.clone());
            continue;
        }
        // 实际复测轮数（基础 5 轮 + 低置信升级补轮），供报告如实
        // 反映升级是否发生（v28 t09 实测发现 repeats 字段曾硬编码
        // 恒为 5——升级已执行但报告失真）
        actual_repeats.push(rounds / 2);
        // 每维均值（全部轮次平均，同时消位置偏差与复测波动）；
        // scores 为去掉 a_first 标记的分数数组（一致性/标准差/均值共用，
        // 与 v14 语义一致：a_first=true 取 A 分数，false 取 B 分数）
        let scores: Vec<[f64; 5]> = round_scores
            .iter()
            .map(|(af, a, b)| if *af { *a } else { *b })
            .collect();
        for i in 0..5 {
            let dim_sum: f64 = scores.iter().map(|s| s[i]).sum();
            sums[i] += dim_sum / scores.len() as f64;
            // 该维标准差（复测波动幅度）
            let mean = dim_sum / scores.len() as f64;
            let var: f64 = scores.iter().map(|s| (s[i] - mean).powi(2)).sum::<f64>() / scores.len() as f64;
            std_sum += var.sqrt();
        }
        // κ 一致性：该模块内任意两轮、同一维度分数绝对差 ≤1 的比例
        for a in 0..scores.len() {
            for b in (a + 1)..scores.len() {
                for &sa in &scores[a] {
                    for &sb in &scores[b] {
                        total_pairs += 1;
                        if (sa - sb).abs() <= 1.0 {
                            consistent_pairs += 1;
                        }
                    }
                }
            }
        }
        // v14 C 组（MVVP 缺口）：位置偏差——每维度比较 AB 组与 BA 组
        // 均值，A 胜判定累计（P(A 胜) 偏离 0.5 即位置敏感）
        for i in 0..5 {
            let ab: Vec<f64> = round_scores.iter().filter(|(af, _, _)| *af).map(|(_, a, _)| a[i]).collect();
            let ba: Vec<f64> = round_scores.iter().filter(|(af, _, _)| !*af).map(|(_, _, b)| b[i]).collect();
            if !ab.is_empty() && !ba.is_empty() {
                position_pairs += 1;
                let ab_mean = ab.iter().sum::<f64>() / ab.len() as f64;
                let ba_mean = ba.iter().sum::<f64>() / ba.len() as f64;
                if ab_mean > ba_mean {
                    position_wins_a += 1;
                }
            }
        }
        // v14 C 组：模块级复测标准差（五维平均，低置信判定依据——
        // 分数波动超过阈值说明该模块结论不可信，需人工复核）
        module_stds.push((title.clone(), module_std(&round_scores)));
        // t04（Phase 1）：判定级指标——flip_rate 相对模块多数判定
        // （2606.19544 self-consistency 互补面）、position_flip_rate
        // （AB↔BA 交换后判定翻转，逐对口径）、kappa_cohen 的 2×2 一致表
        let metrics = module_judgment_metrics(&round_scores);
        flip_sum += metrics.flip_rate;
        pos_flip_sum += metrics.position_flip_rate;
        // v32（6.1）：平局率（模块级平均口径，与 flip_rate 一致）与
        // 三态明细累计——每轮 AB/BA 两次调用判定各入一桶
        tie_sum += module_tie_rate(&round_scores);
        for (_, a, b) in &round_scores {
            match judgment(a, b) {
                1 => agreement_breakdown[0] += 1,
                -1 => agreement_breakdown[1] += 1,
                _ => agreement_breakdown[2] += 1,
            }
        }
        let table = module_kappa_table(&round_scores);
        for (i, row) in table.iter().enumerate() {
            for (j, &v) in row.iter().enumerate() {
                kappa_table[i][j] += v;
            }
        }
        judged += 1;
    }
    if judged == 0 {
        return Ok(None);
    }
    let avg = |i: usize| sums[i] / judged as f64;
    let kappa_like = if total_pairs == 0 {
        1.0
    } else {
        consistent_pairs as f64 / total_pairs as f64
    };
    // v14 C 组（MVVP 缺口）：机会校正 κ——一致率经机会一致性
    // p²+(1−p)² 校正，衡量"超出偶然一致"的稳定性（kappa_like 是原始率；
    // 机会基线用 p_obs 近似，非标准 Cohen's κ，对照口径见 kappa_cohen）
    let kappa = if total_pairs == 0 {
        0.0
    } else {
        let p_obs = consistent_pairs as f64 / total_pairs as f64;
        let p_exp = p_obs.powi(2) + (1.0 - p_obs).powi(2);
        if p_exp >= 1.0 {
            0.0
        } else {
            ((p_obs - p_exp) / (1.0 - p_exp)).max(0.0)
        }
    };
    // v14 C 组（MVVP 缺口）：位置偏差 |P(A 胜) − 0.5|
    let position_bias = if position_pairs == 0 {
        0.0
    } else {
        let p_a = position_wins_a as f64 / position_pairs as f64;
        (p_a - 0.5).abs()
    };
    // v14 C 组：低置信模块 = 复测失败（已收集）+ 模块 σ 超阈值
    for (module, std) in &module_stds {
        if *std > LOW_CONFIDENCE_STD_THRESHOLD {
            low_confidence.push(module.clone());
        }
    }
    low_confidence.sort();
    low_confidence.dedup();
    let avg_std = std_sum / (judged * 5) as f64;
    // v28 t09：实际复测轮数（平均，升级补轮后 >5）；无成功模块时
    // 保持基础轮数语义（报告字段不虚报）
    let repeats_actual = if actual_repeats.is_empty() {
        TQS_REPEATS
    } else {
        actual_repeats.iter().sum::<usize>() / actual_repeats.len()
    };
    Ok(Some(TqsReport {
        judged_modules: judged,
        avg_clarity: avg(0),
        avg_readability: avg(1),
        avg_conciseness: avg(2),
        avg_richness: avg(3),
        avg_structure: avg(4),
        avg_total: (avg(0) + avg(1) + avg(2) + avg(3) + avg(4)) / 5.0,
        repeats: repeats_actual,
        kappa_like,
        kappa,
        position_bias,
        low_confidence_modules: low_confidence,
        avg_std,
        judge_model: config.llm.model.clone(),
        // t04（Phase 1）：判定级可靠性指标与协议声明
        kappa_cohen: kappa_cohen_from_table(&kappa_table),
        flip_rate: flip_sum / judged as f64,
        position_flip_rate: pos_flip_sum / judged as f64,
        delta_kappa: kappa_like - kappa,
        eligible_modules: pairs.len(),
        parse_success_rate: if parse_total == 0 {
            1.0
        } else {
            parse_ok as f64 / parse_total as f64
        },
        judgment_scale: TQS_JUDGMENT_SCALE.into(),
        aggregation_level: TQS_AGGREGATION_LEVEL.into(),
        tie_handling: TQS_TIE_HANDLING.into(),
        // v32（6.1 FR-102/FR-103）：三态协议——平局率与明细
        // （judged>0 已由前面的 judged==0 早退保证）
        tie_rate: tie_sum / judged as f64,
        agreement_breakdown,
    }))
}

/// 五维总分（判定胜负的比较量，0-50）
fn total_score(s: &[f64; 5]) -> f64 {
    s.iter().sum()
}

/// 单次调用的 A 胜/平/B 胜三态判定：1 = A 胜、0 = 平、-1 = B 胜。
/// t04：flip_rate 与 position_flip_rate 均基于该三态判定（tie 是独立
/// 类别而非静默归入胜负——2606.00093 item 6 的 estimand 声明）
fn judgment(a: &[f64; 5], b: &[f64; 5]) -> i8 {
    let ta = total_score(a);
    let tb = total_score(b);
    if ta > tb {
        1
    } else if ta < tb {
        -1
    } else {
        0
    }
}

/// 多数判定（三态众数；并列按 A胜 > 平 > B胜 取——连续分数下平局
/// 概率近零，仅保证确定性）
fn majority_judgment(judgments: &[i8]) -> i8 {
    // 三态计数：index = (判定 + 1)（-1→0, 0→1, 1→2）
    let mut counts = [0usize; 3];
    for &j in judgments {
        counts[(j + 1) as usize] += 1;
    }
    if counts[2] >= counts[1] && counts[2] >= counts[0] {
        1
    } else if counts[1] >= counts[0] {
        0
    } else {
        -1
    }
}

/// 模块级判定指标（Phase 1，t03）：
///
/// - `flip_rate`：各调用的三态判定与模块内多数判定不一致的比例
///   （2606.13685 flip rate 13.6% 的本地口径；2606.19544
///   self-consistency = 1 − flip_rate）。
/// - `position_flip_rate`：同一轮内 AB 顺序与 BA 顺序两次调用的判定
///   相异比例（2606.19544 item 级定义：交换位置后判定翻转；逐对口径，
///   区别于 position_bias 的组均值比较）。
///
/// 轮配对约定：round_scores 每轮写入两次调用（先 a_first=true 的 AB
/// 再 a_first=false 的 BA），故 2k 与 2k+1 构成同轮 AB/BA 对。
struct ModuleJudgmentMetrics {
    flip_rate: f64,
    position_flip_rate: f64,
}

fn module_judgment_metrics(round_scores: &[(bool, [f64; 5], [f64; 5])]) -> ModuleJudgmentMetrics {
    let judgments: Vec<i8> = round_scores.iter().map(|(_, a, b)| judgment(a, b)).collect();
    let majority = majority_judgment(&judgments);
    let flips = judgments.iter().filter(|&&j| j != majority).count();
    let mut pos_flips = 0usize;
    let mut pairs = 0usize;
    for k in (0..round_scores.len()).step_by(2) {
        let (Some((_, a1, b1)), Some((_, a2, b2))) = (round_scores.get(k), round_scores.get(k + 1)) else {
            continue;
        };
        if judgment(a1, b1) != judgment(a2, b2) {
            pos_flips += 1;
        }
        pairs += 1;
    }
    ModuleJudgmentMetrics {
        flip_rate: if round_scores.is_empty() {
            0.0
        } else {
            flips as f64 / round_scores.len() as f64
        },
        position_flip_rate: if pairs == 0 { 0.0 } else { pos_flips as f64 / pairs as f64 },
    }
}

/// 模块级复测标准差（五维平均；低置信判定与升级检查共用，
/// 消除 v14 内联重复）
fn module_std(round_scores: &[(bool, [f64; 5], [f64; 5])]) -> f64 {
    if round_scores.is_empty() {
        return 0.0;
    }
    let scores: Vec<[f64; 5]> = round_scores
        .iter()
        .map(|(af, a, b)| if *af { *a } else { *b })
        .collect();
    let mut var = 0.0f64;
    for i in 0..5 {
        let mean: f64 = scores.iter().map(|s| s[i]).sum::<f64>() / scores.len() as f64;
        var += scores.iter().map(|s| (s[i] - mean).powi(2)).sum::<f64>() / scores.len() as f64;
    }
    (var / 5.0).sqrt()
}

/// v32（6.1 FR-103）：模块级平局率——round_scores 中 judgment()==0
/// 的比例（tie 独立类别；与 module_judgment_metrics 共用 judgment 三态
/// 口径，保证升级触发与报告统计一致）
fn module_tie_rate(round_scores: &[(bool, [f64; 5], [f64; 5])]) -> f64 {
    if round_scores.is_empty() {
        return 0.0;
    }
    let ties = round_scores
        .iter()
        .filter(|(_, a, b)| judgment(a, b) == 0)
        .count();
    ties as f64 / round_scores.len() as f64
}

/// 单模块 AB/BA 判定的 2×2 一致表（标准 Cohen's κ 的输入）：
/// rater1 = AB 顺序调用、rater2 = BA 顺序调用；item = (模块, 维度, 轮)；
/// 类别 = {A 胜, B 胜}，平局按 B 胜计入（连续分数相等概率近零，
/// 该归并口径写入 tie_handling 声明）
fn module_kappa_table(round_scores: &[(bool, [f64; 5], [f64; 5])]) -> [[usize; 2]; 2] {
    let mut table = [[0usize; 2]; 2];
    for k in (0..round_scores.len()).step_by(2) {
        let (Some((_, a1, b1)), Some((_, a2, b2))) = (round_scores.get(k), round_scores.get(k + 1)) else {
            continue;
        };
        for d in 0..5 {
            // 0 = A 胜（a > b），1 = B 胜（含平）
            let j1 = usize::from(a1[d] <= b1[d]);
            let j2 = usize::from(a2[d] <= b2[d]);
            table[j1][j2] += 1;
        }
    }
    table
}

/// 标准 Cohen's κ = (P_o − P_e)/(1 − P_e)：
/// P_o = 两 rater 判定一致比例，P_e = 边际概率乘积和（机会一致）。
/// 与 kappa_like/kappa 的自定义稳定率口径不同：标准 κ 基于两个独立
/// rater（AB/BA 调用）的真实边际表；负值保留（比随机更不一致是有效
/// 信号，2606.19544 报告口径）。2606.19544：exact-match 高估
/// 33.8-41.3pp，κ 才是机会校正后的可靠性。
fn kappa_cohen_from_table(t: &[[usize; 2]; 2]) -> f64 {
    let n = t[0][0] + t[0][1] + t[1][0] + t[1][1];
    if n == 0 {
        return 0.0;
    }
    let po = (t[0][0] + t[1][1]) as f64 / n as f64;
    let r1_a = (t[0][0] + t[0][1]) as f64 / n as f64;
    let r2_a = (t[0][0] + t[1][0]) as f64 / n as f64;
    let pe = r1_a * r2_a + (1.0 - r1_a) * (1.0 - r2_a);
    if pe >= 1.0 {
        0.0
    } else {
        (po - pe) / (1.0 - pe)
    }
}

/// Rubric 独立生成轮次（CodeWikiBench：多模型独立生成后语义合并；
/// 单裁判下用多次独立生成近似多模型合成）
const RUBRIC_GENERATIONS: usize = 3;

/// t04（Phase 2）：叶子判定轮数——3 次多数投票（2606.13685：单次
/// 判定保真仅 86.6%，3 trials 达约 90% 共识保真）
const RUBRIC_LEAF_REPEATS: usize = 3;

/// t04（Phase 2）：争议叶子（1:2 分裂或含 abstain 平票）升级轮数——
/// 5 次仍无多数则整叶子 abstain（2606.13685：hard 档需更多 trials，
/// 5 是成本可控的收敛点）
const RUBRIC_LEAF_REPEATS_ESCALATED: usize = 5;

/// t04：Rubric 聚合层级声明（2606.00093 item 10）——叶子级多数投票
/// → 权重自底向上聚合
const RUBRIC_AGGREGATION_LEVEL: &str = "叶子级 3 次多数投票（争议升级 5 次）→ 权重自底向上聚合（abstain 叶子排除）";

/// Rubric 打分执行（维度 7）：docs_tree → 3 次独立生成 → 1 次合并 →
/// 叶子 0/1 判定 → 加权自底向上聚合
///
/// root 为被测仓库根（README/docs 收集基准）；产物证据从 config.output_dir().display()
/// 读取（overview + api + 模块页标题，截断控制 token 成本）。
/// LLM 不可用或 docs_tree 缺失时返回 None（"失败只告警"，不中断评测）。
fn measure_rubrics(config: &WikiConfig, root: &ProjectRoot) -> Result<Option<RubricReport>> {
    // 1. docs_tree 收集：README + docs/*.md（仓库意图的权威来源；
    //    缺失时无法推导需求，跳过本维度——不是文档质量问题）
    let mut docs_text = String::new();
    let readme = root.path().join("README.md");
    if let Ok(c) = std::fs::read_to_string(&readme) {
        docs_text.push_str(&format!("# README.md\n{c}\n"));
    }
    let docs_dir = root.path().join("docs");
    if docs_dir.is_dir() {
        let mut files: Vec<PathBuf> = walk_docs(&docs_dir);
        files.sort();
        for f in files {
            if let Ok(c) = std::fs::read_to_string(&f) {
                docs_text.push_str(&format!("# {}\n{c}\n", f.display()));
            }
        }
    }
    if docs_text.trim().is_empty() {
        tracing::warn!("Rubric 跳过：被测仓库无 README/docs 文档（无法推导仓库意图）");
        return Ok(None);
    }
    // 成本控制：docs 过长时保留前 40K 字符（意图声明通常在前部）
    docs_text.truncate(40_000);

    let provider = match crate::generate::create_provider(config) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Rubric 跳过（LLM 不可用）: {e}");
            return Ok(None);
        }
    };
    let rt = crate::get_global_runtime();

    // 2. 独立生成 3 份 rubric 树（单轮失败不影响其余轮次）
    let mut trees: Vec<Vec<RubricNode>> = Vec::new();
    for i in 0..RUBRIC_GENERATIONS {
        let messages = rubric_generation_prompt(&docs_text);
        // 预算显式给足：deepseek-v4-flash 等推理型模型会消耗 reasoning
        // 输出预算，无预算时 max_output_tokens 交服务器默认（实测 4096
        // 档偶发只有 reasoning 块无 message，见 llm.rs 注释）；e5626ff
        // 修复时漏改本调用点，与 TQS/合并保持同口径
        match rt.block_on(provider.complete_with_budget(&messages, Some(BENCH_MAX_OUTPUT_TOKENS))) {
            Ok(content) => match parse_rubric_tree(&content) {
                Ok(tree) => trees.push(tree),
                Err(e) => {
                    tracing::warn!("Rubric 生成解析失败（第 {} 轮）: {e}", i + 1);
                }
            },
            Err(e) => {
                tracing::warn!("Rubric 生成调用失败（第 {} 轮）: {e}", i + 1);
            }
        }
    }
    if trees.is_empty() {
        tracing::warn!("Rubric 跳过：{} 轮生成全部失败", RUBRIC_GENERATIONS);
        return Ok(None);
    }
    // 3. 第 4 次调用语义合并（>70% 相似度节点合并由 LLM 执行；合并失败
    //    降级为第一份生成结果——合并是质量增强而非契约）
    let merged = match rt.block_on(provider.complete_with_budget(
        &rubric_merge_prompt(&trees),
        Some(BENCH_MAX_OUTPUT_TOKENS),
    )) {
        Ok(content) => match parse_rubric_tree(&content) {
            Ok(tree) => tree,
            Err(e) => {
                tracing::warn!("Rubric 合并解析失败，降级使用第一份生成结果: {e}");
                trees[0].clone()
            }
        },
        Err(e) => {
            tracing::warn!("Rubric 合并调用失败，降级使用第一份生成结果: {e}");
            trees[0].clone()
        }
    };
    let leaves = collect_leaves(&merged);
    if leaves.is_empty() {
        tracing::warn!("Rubric 跳过：合并后无叶子");
        return Ok(None);
    }
    // 4. 叶子判定证据：产物文档摘要（overview + api 实体清单 + 页面标题；
    //    页面正文全量在真实评测下 token 成本不可控，用摘要形态做判定）。
    //    检索增强（方案甲）：仅摘要+标题时 LLM 系统性保守判「证据不足→
    //    不满足」（实测 satisfied 0-12.6%），故按叶子 requirement 关键词
    //    检索 wiki 页正文 top-K，命中页正文片段拼入证据补足判定依据。
    //    pages 一次收集全量复用，循环内只做关键词检索（正文读取 I/O 不重复）
    let pages = collect_wiki_pages(config.output_dir());
    // 5. 叶子 0/1 判定（顺序与 collect_leaves 一致，供聚合索引）。
    //    t04（Phase 2）：每叶子 3 次调用多数投票——2606.13685 单次判定
    //    保真仅 86.6%，3 trials 达约 90%；1:2 争议（含 abstain 平票）
    //    升级至 5 次；判定结果 2 类（0/1），解析/调用失败独立计 abstain
    //    不再 recode 为 false（2606.00093 item 6：recode 改变 estimand）
    let mut verdicts: Vec<Option<bool>> = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        // 每叶子独立构建证据：摘要锚点固定（全局基线），检索节随
        // requirement 变化；top_k=2（2 页 × 3000 字符 ≈ 6K，叠加摘要
        // ≈ 19K 仍在 20K cap 内，页数再多检索节尾部会被截断而失去
        // 意义）；检索节追加后整体截断，总证据仍 cap 20K 防 token 失控
        let mut evidence = build_evidence(config.output_dir(), &config.wiki.language);
        let retrieved = search_pages(&pages, &extract_keywords(&leaf.requirement), 2);
        if !retrieved.is_empty() {
            evidence.push_str("\n\n# 检索到的页面正文\n");
            for (name, snippet) in &retrieved {
                evidence.push_str(&format!("- {name}: {snippet}\n"));
            }
            evidence = truncate(&evidence, 20_000);
        }
        let mut votes: Vec<Option<bool>> = Vec::new();
        // v32（6.1 FR-102）：uncertain 重试标记与独立尝试计数——LLM
        // 主动声明证据不足时换选项顺序重试一次；重试后仍 uncertain 记
        // abstain（None）。attempts 独立于 votes.len() 自增（votes 在
        // uncertain 重试时不增长），保证重试调用真正换 variant
        let mut uncertain_retried = false;
        let mut attempts = 0usize;
        while votes.len() < RUBRIC_LEAF_REPEATS_ESCALATED {
            // 选项顺序随机化（2602.02219：2 选项 swap 即 n=2 平衡排列，
            // 消 primacy/recency；按 requirement 哈希确定性取，复跑可复现）
            let messages = rubric_judge_prompt(
                &leaf.requirement,
                &evidence,
                option_variant(&leaf.requirement, attempts),
            );
            attempts += 1;
            // 同生成轮：判定输出短但需完整 message（推理型模型预算吞没
            // 风险一致），与 TQS/合并同口径给足预算
            match rt.block_on(provider.complete_with_budget(&messages, Some(BENCH_MAX_OUTPUT_TOKENS))) {
                Ok(content) => match parse_rubric_verdict(&content) {
                    Some(RubricVerdict::Satisfied) => votes.push(Some(true)),
                    Some(RubricVerdict::Unsatisfied) => votes.push(Some(false)),
                    // 首次 uncertain：不 push（votes 不变），下轮换 variant
                    // 重试；重试后仍 uncertain：记 abstain 推进循环收敛
                    Some(RubricVerdict::Uncertain) => {
                        if !uncertain_retried {
                            uncertain_retried = true;
                            continue;
                        }
                        tracing::warn!(
                            "Rubric 叶子判定重试后仍 uncertain（计 abstain）: {}",
                            leaf.requirement
                        );
                        votes.push(None);
                    }
                    None => {
                        tracing::warn!("Rubric 叶子判定解析失败（计 abstain）: {}", leaf.requirement);
                        votes.push(None);
                    }
                },
                Err(e) => {
                    tracing::warn!("Rubric 叶子判定调用失败（计 abstain）: {e}");
                    votes.push(None);
                }
            };
            // 3 票后多数已定（true/false 票数不等）即停；否则争议升级至 5 票
            if votes.len() == RUBRIC_LEAF_REPEATS && verdict_resolved(&votes) {
                break;
            }
        }
        verdicts.push(majority_verdict(&votes));
    }
    // 6. 加权自底向上聚合（顶层多根包为虚拟根，weight=1；叶子 σ 二项
    //    近似，非叶子按权重平方传播；abstain 叶子显式排除——不贡献
    //    权重/分数/σ，coverage 以有效判定叶子为分母）
    let mut leaf_idx = 0usize;
    let root = RubricNode {
        requirement: "root".into(),
        weight: 1.0,
        sub_tasks: merged.clone(),
    };
    let aggregated = aggregate_score(&root, &verdicts, &mut leaf_idx);
    let leaf_count = leaves.len();
    let abstain = verdicts.iter().filter(|v| v.is_none()).count();
    let satisfied = verdicts.iter().filter(|v| **v == Some(true)).count();
    // 排除 abstain 后 coverage（00093 item 8：exclude 模式报覆盖子集性能；
    // 空集约定 1.0 与既有口径一致）
    let judged = leaf_count - abstain;
    let coverage = if judged == 0 {
        1.0
    } else {
        satisfied as f64 / judged as f64
    };
    let abstain_rate = if leaf_count == 0 {
        0.0
    } else {
        abstain as f64 / leaf_count as f64
    };
    Ok(Some(RubricReport {
        rubric_nodes: count_nodes(&root.sub_tasks),
        leaf_count,
        satisfied_leaves: satisfied,
        coverage,
        score: aggregated.score,
        score_std: aggregated.std,
        generation_calls: RUBRIC_GENERATIONS + 1,
        judge_model: config.llm.model.clone(),
        abstain_leaves: abstain,
        abstain_rate,
        leaf_verdict_repeats: RUBRIC_LEAF_REPEATS,
        aggregation_level: RUBRIC_AGGREGATION_LEVEL.into(),
    }))
}

/// Rubric 生成 prompt：docs_tree → 层级 rubric JSON
fn rubric_generation_prompt(docs_text: &str) -> Vec<crate::generate::llm::Message> {
    let system = "你是仓库文档需求分析器。根据仓库的 README 与 docs 推导出文档应满足的需求清单（用于评测 Wiki 文档对仓库意图的覆盖度）。输出 JSON：{\"rubrics\": [{\"requirement\": \"需求描述\", \"weight\": 1-3, \"sub_tasks\": [...]}]}，层级最多 3 层，叶子必须无 sub_tasks。只输出 JSON。";
    vec![
        crate::generate::llm::Message::system(system),
        crate::generate::llm::Message::user(docs_text.to_string()),
    ]
}

/// Rubric 合并 prompt：多份独立生成的 rubric 树 → 语义合并后的单树
fn rubric_merge_prompt(trees: &[Vec<RubricNode>]) -> Vec<crate::generate::llm::Message> {
    let mut user = String::from("合并以下多份独立生成的 rubrics 为一份：语义相同或高度相似（>70%）的需求合并为一条（权重取均值），其余保留；保持层级结构（最多 3 层）。只输出合并后的 JSON：{\"rubrics\": [...]}。\n\n");
    for (i, tree) in trees.iter().enumerate() {
        user.push_str(&format!(
            "--- 第 {} 份 ---\n{}\n",
            i + 1,
            serde_json::to_string_pretty(tree).unwrap_or_default()
        ));
    }
    vec![
        crate::generate::llm::Message::system("你是文档需求合并器。只输出合并后的 JSON。"),
        crate::generate::llm::Message::user(user),
    ]
}

/// Rubric 叶子判定 prompt：需求 vs 产物证据 → 三态判定
/// satisfied/unsatisfied/uncertain。
///
/// `reverse_options` 为 true 时 satisfied/unsatisfied 选项顺序反转
/// （2602.02219：2 选项 swap 即 n=2 的平衡排列特例，少量随机顺序即可
/// 获得大部分 primacy/recency 消偏收益；uncertain 恒定第三项——它是
/// "证据不足"类别而非选项位置消偏对象）。
/// v32（6.1 FR-102）：三态协议——uncertain 表示 LLM 主动声明证据
/// 不足以判定（区别于解析/调用失败的管线 abstain）；uncertain 由
/// 调用方重试一次，仍不确定才记 abstain（不计入分母）。
fn rubric_judge_prompt(requirement: &str, evidence: &str, reverse_options: bool) -> Vec<crate::generate::llm::Message> {
    let options = if reverse_options {
        "\"unsatisfied\" 或 \"satisfied\""
    } else {
        "\"satisfied\" 或 \"unsatisfied\""
    };
    let system = format!(
        "你是 Wiki 文档质量裁判。判断下面的文档产物是否满足给定的需求。只输出 JSON：{{\"verdict\": {options} 或 \"uncertain\"}}。若给出的产物证据不足以判定（摘要与检索片段均未提及相关事实），输出 \"uncertain\"，不要猜测。"
    );
    vec![
        crate::generate::llm::Message::system(system),
        crate::generate::llm::Message::user(format!(
            "需求：{}\n\n--- 文档产物摘要 ---\n{}",
            requirement, evidence
        )),
    ]
}

/// v32（6.1 FR-102）：叶子判定三态（LLM 输出协议）——Satisfied/Unsatisfied
/// 是 0/1 判定；Uncertain = LLM 主动声明证据不足（与解析/调用失败的
/// 管线 abstain 区分：uncertain 由调用方重试一次，仍不确定才记 abstain）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubricVerdict {
    Satisfied,
    Unsatisfied,
    Uncertain,
}

/// 解析 rubric 生成/合并输出：剥离围栏 → JSON 数组或 {rubrics: [...]}
fn parse_rubric_tree(content: &str) -> Result<Vec<RubricNode>> {
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = serde_json::from_str(stripped)
        .with_context(|| "解析 Rubric JSON 失败")?;
    // 数组形态直接取；对象形态取 rubrics 键；单对象形态视为单节点树
    let nodes: Vec<serde_json::Value> = match &value {
        serde_json::Value::Array(arr) => arr.clone(),
        serde_json::Value::Object(map) => match map.get("rubrics") {
            Some(serde_json::Value::Array(arr)) => arr.clone(),
            _ => vec![value.clone()],
        },
        _ => anyhow::bail!("Rubric 输出既非数组也非对象"),
    };
    nodes
        .into_iter()
        .map(|n| parse_rubric_node(&n).with_context(|| "Rubric 节点字段缺失"))
        .collect()
}

/// 手工解析单个 rubric 节点（LLM 输出非确定性，需容错）：
///
/// - `requirement` 必填字符串，缺失即失败（错误带上下文可诊断）
/// - `weight` 接受数字或数字字符串（LLM 偶发输出 `"weight": "3"`）
/// - `sub_tasks` 数组元素为字符串时视为叶子节点（LLM 偶发输出字符串
///   数组而非对象数组，实测 8192 预算档复现；字符串语义=需求文本）
fn parse_rubric_node(v: &serde_json::Value) -> Result<RubricNode> {
    let map = v
        .as_object()
        .with_context(|| "Rubric 节点必须是对象")?;
    let requirement = map
        .get("requirement")
        .and_then(|r| r.as_str())
        .with_context(|| "Rubric 节点缺少 requirement 字段")?
        .to_string();
    let weight = map
        .get("weight")
        .and_then(|w| w.as_f64().or_else(|| w.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(1.0);
    let sub_tasks = match map.get("sub_tasks") {
        Some(serde_json::Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    serde_json::Value::String(s) => {
                        // 字符串子任务 → 叶子节点（weight 取父权重 1.0）
                        out.push(RubricNode {
                            requirement: s.clone(),
                            weight: 1.0,
                            sub_tasks: Vec::new(),
                        });
                    }
                    _ => out.push(parse_rubric_node(item)?),
                }
            }
            out
        }
        _ => Vec::new(),
    };
    Ok(RubricNode {
        requirement,
        weight,
        sub_tasks,
    })
}

/// 解析叶子判定输出：{"verdict": "satisfied"|"unsatisfied"|"uncertain"}
/// （v32 6.1 三态协议；旧版 {"satisfied": bool} 字段不再产出——输出
/// 模板已切换，产物仅在真实评测时生成，无向后兼容负担）
fn parse_rubric_verdict(content: &str) -> Option<RubricVerdict> {
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = serde_json::from_str(stripped).ok()?;
    match value.get("verdict")?.as_str()? {
        "satisfied" => Some(RubricVerdict::Satisfied),
        "unsatisfied" => Some(RubricVerdict::Unsatisfied),
        "uncertain" => Some(RubricVerdict::Uncertain),
        _ => None,
    }
}

/// 递归收集叶子（sub_tasks 为空）
fn collect_leaves(nodes: &[RubricNode]) -> Vec<&RubricNode> {
    let mut out = Vec::new();
    for node in nodes {
        if node.sub_tasks.is_empty() {
            out.push(node);
        } else {
            out.extend(collect_leaves(&node.sub_tasks));
        }
    }
    out
}

/// 节点总数（含非叶子）
fn count_nodes(nodes: &[RubricNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_nodes(&n.sub_tasks))
        .sum()
}

/// 权重 clamp 到 [1, 3]（LLM 输出越界时收敛，聚合分母不因异常权重变形）
fn node_weight(w: f64) -> f64 {
    w.clamp(1.0, 3.0)
}

/// 加权自底向上聚合：S(n)=Σw(c)S(c)/Σw(c)；叶子 σ=sqrt(p(1-p)) 二项近似，
/// 非叶子 σ²=Σ(w²σ²)/Σw² 权重平方传播（CodeWikiBench 层级聚合公式）。
///
/// verdicts 元素为 Option<bool>：Some = 0/1 判定，None = abstain
/// （t04：abstain 叶子显式排除——不贡献权重/分数/σ/叶子计数，
/// 与 2606.00093 item 6 的 exclude 模式一致，coverage 由调用方以
/// 有效判定叶子为分母）
fn aggregate_score(node: &RubricNode, verdicts: &[Option<bool>], leaf_idx: &mut usize) -> RubricScore {
    if node.sub_tasks.is_empty() {
        let satisfied = verdicts.get(*leaf_idx).copied().flatten();
        *leaf_idx += 1;
        return match satisfied {
            Some(true) => RubricScore { score: 1.0, std: 0.0, leaves: 1, satisfied: 1 },
            Some(false) => RubricScore { score: 0.0, std: 0.0, leaves: 1, satisfied: 0 },
            // abstain：整叶子从聚合中排除（权重不进分母、不计数）
            None => RubricScore { score: 0.0, std: 0.0, leaves: 0, satisfied: 0 },
        };
    }
    let mut w_sum = 0.0f64;
    let mut s_sum = 0.0f64;
    let mut w2_sum = 0.0f64;
    let mut s2_sum = 0.0f64;
    let mut leaves = 0usize;
    let mut satisfied = 0usize;
    for sub in &node.sub_tasks {
        let w = node_weight(sub.weight);
        let rs = aggregate_score(sub, verdicts, leaf_idx);
        // abstain 子树（rs.leaves == 0）整体排除：权重不进分母、
        // 分数/σ 不贡献（2606.00093 item 6 exclude 模式）
        let w_eff = if rs.leaves == 0 { 0.0 } else { w };
        w_sum += w_eff;
        s_sum += w_eff * rs.score;
        w2_sum += w_eff * w_eff;
        s2_sum += w_eff * w_eff * rs.std * rs.std;
        leaves += rs.leaves;
        satisfied += rs.satisfied;
    }
    RubricScore {
        score: if w_sum > 0.0 { s_sum / w_sum } else { 0.0 },
        std: if w2_sum > 0.0 { (s2_sum / w2_sum).sqrt() } else { 0.0 },
        leaves,
        satisfied,
    }
}

/// 多数投票：true 票 > false 票 → Some(true)；反之 Some(false)；
/// 平票（含 abstain 票，如 1:1:1、2:2:1）→ None（叶子 abstain）。
/// t04（2606.13685 多数投票聚合；叶子级聚合是 binary verdict flip
/// 与其数据集形态的直接对应）
fn majority_verdict(votes: &[Option<bool>]) -> Option<bool> {
    let t = votes.iter().filter(|v| **v == Some(true)).count();
    let f = votes.iter().filter(|v| **v == Some(false)).count();
    if t > f {
        Some(true)
    } else if f > t {
        Some(false)
    } else {
        None
    }
}

/// 投票是否已能定案（true/false 票数不等即多数已定）——3 票阶段用于
/// 判定是否需要争议升级（1:1:1 或 1:1:abstain 等平票才升级到 5 票）
fn verdict_resolved(votes: &[Option<bool>]) -> bool {
    let t = votes.iter().filter(|v| **v == Some(true)).count();
    let f = votes.iter().filter(|v| **v == Some(false)).count();
    t != f
}

/// 判定选项顺序的确定性伪随机（2602.02219：选项位置影响选择，需要
/// 随机顺序；用 requirement 文本哈希 + 调用序号取模，不引入 rand
/// 依赖、复跑可复现——与本仓库 llm.rs 抖动做法一致）
fn option_variant(requirement: &str, call_idx: usize) -> bool {
    let h: u32 = requirement
        .chars()
        .fold(0u32, |acc, c| acc.wrapping_mul(31).wrapping_add(c as u32));
    ((h as usize) + call_idx) % 2 == 1
}

/// 递归收集 docs 目录下全部 .md 文件
fn walk_docs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_docs(&path));
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    out
}

/// 构建叶子判定证据：overview + api 实体清单 + 模块页标题（截断控制 token）
fn build_evidence(output_dir: &Path, lang: &str) -> String {
    let mut evidence = String::new();
    for name in ["overview.md", "api.md"] {
        let path = output_dir.join("wiki").join(lang).join(name);
        if let Ok(c) = std::fs::read_to_string(&path) {
            evidence.push_str(&format!("# {name}\n{}\n", truncate(&c, 6_000)));
        }
    }
    let wiki_dir = output_dir.join("wiki").join(lang);
    if let Ok(entries) = std::fs::read_dir(&wiki_dir) {
        let mut titles: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".md")
                    .then(|| name.trim_end_matches(".md").to_string())
            })
            .collect();
        titles.sort();
        evidence.push_str(&format!(
            "# 模块页\n{}\n",
            titles.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")
        ));
    }
    truncate(&evidence, 20_000)
}

/// 字符串截断（中文字符安全：按 char 边界截断）
fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 判断是否为 CJK 表意文字（统一表意/扩展 A/兼容区；2-gram 切分只对
/// 汉字有意义，日文假名等非汉字字形不切分，按非 CJK 字符处理）
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}

/// 从需求文本提取检索关键词：CJK 连续串按滑动窗口 2-gram 切分
/// （如「安装配置指南」→ 安装/装配/配置/置指/指南；单字不成词，
/// 2-gram 覆盖绝大多数中文术语），英文词/数字串保留原样
/// （"GPT-4" 拆为 "GPT"/"4" 两个关键词）。空串/纯标点返回空 Vec，
/// 调用方按「无关键词可检索」退化处理（维持现状证据）。
fn extract_keywords(requirement: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut ascii_run = String::new();
    let flush_cjk = |run: &mut Vec<char>, out: &mut Vec<String>| {
        for w in run.windows(2) {
            out.push(w.iter().collect());
        }
        run.clear();
    };
    let flush_ascii = |run: &mut String, out: &mut Vec<String>| {
        if !run.is_empty() {
            out.push(std::mem::take(run));
        }
    };
    for c in requirement.chars() {
        if is_cjk(c) {
            flush_ascii(&mut ascii_run, &mut keywords);
            cjk_run.push(c);
        } else if c.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut keywords);
            ascii_run.push(c);
        } else {
            flush_cjk(&mut cjk_run, &mut keywords);
            flush_ascii(&mut ascii_run, &mut keywords);
        }
    }
    flush_cjk(&mut cjk_run, &mut keywords);
    flush_ascii(&mut ascii_run, &mut keywords);
    keywords
}

/// 按关键词对 wiki 页正文做计数检索：每页统计全部关键词出现次数之和
/// （2-gram 各分量各计各的，如正文含「安装」×2 +「配置」×1 则合计 3），
/// 按命中数降序取 top_k（平局按页名字典序），每页正文截断 3000 字符
/// （与 build_evidence 同口径控制 token）。返回 (页名, 正文片段)；
/// 无命中或关键词为空返回空 Vec，调用方维持现状证据（退化安全）。
fn search_pages(pages: &[(PathBuf, String)], keywords: &[String], top_k: usize) -> Vec<(String, String)> {
    if keywords.is_empty() || top_k == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(String, usize, String)> = pages
        .iter()
        .filter_map(|(path, content)| {
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let count: usize = keywords
                .iter()
                .filter(|k| !k.is_empty())
                .map(|k| content.matches(k.as_str()).count())
                .sum();
            (count > 0).then(|| (name, count, truncate(content, 3_000)))
        })
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.truncate(top_k);
    hits.into_iter().map(|(name, _, snippet)| (name, snippet)).collect()
}

/// TQS 裁判 prompt（五维定义固定措辞 + 0-10 量表 + strict JSON）
fn tqs_prompt(lang: &str, doc_a: &str, doc_b: &str, a_first: bool) -> Vec<crate::generate::llm::Message> {
    let (first, second) = if a_first { (doc_a, doc_b) } else { (doc_b, doc_a) };
    let system = format!(
        r#"你是代码仓库 Wiki 文档质量裁判。对下面两份同一模块的文档（顺序 A、B）分别打五维分，每维 0-10 分：
- clarity（清晰度）：意图表达是否一目了然
- readability（可读性）：行文是否流畅连贯、便于通读
- conciseness（简洁性）：是否无冗余啰嗦
- richness（丰富度）：信息量与示例是否充分
- structure（结构）：逻辑组织是否清晰

规则：
1. 只评文档质量，禁止因长度差异偏袒（长≠好）；
2. 分数可相同；
3. 先给一句话理由（A、B 各一条），再输出 JSON。

仅输出 JSON，无 prose、无 markdown 围栏，格式：
{{"A": {{"clarity": 0, "readability": 0, "conciseness": 0, "richness": 0, "structure": 0}},
 "B": {{"clarity": 0, "readability": 0, "conciseness": 0, "richness": 0, "structure": 0}}}}
语言：{lang}"#
    );
    vec![
        crate::generate::llm::Message::system(system),
        crate::generate::llm::Message::user(format!(
            "文档 A（第一份）：\n{first}\n\n---\n\n文档 B（第二份）：\n{second}"
        )),
    ]
}

/// 解析裁判 JSON 输出（容错：剥离代码围栏/围栏外文本，取首个 JSON 对象；
/// 分数越界 clamp 到 0-10；缺字段/非 JSON 报错——整条作废重打而非静默裁剪）。
///
/// 返回 (A 分数, B 分数) 两份：t04 的逐对判定指标（position_flip_rate、
/// kappa_cohen）需要同一调用内 A 与 B 的相对判定，仅存第一份会丢失
/// 一半信息（此前只取第一份是点分口径，保留原均值语义由调用方按
/// a_first 选择）。
fn parse_tqs_score(content: &str) -> Result<([f64; 5], [f64; 5])> {
    let trimmed = content.trim();
    // 剥离 ```json ... ``` 围栏（若裁判不遵守 strict JSON）
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim().trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    // 定位首个 '{' 到最后一个 '}'（裁判可能在 JSON 前写了理由）
    let start = inner.find('{').ok_or_else(|| anyhow::anyhow!("输出不含 JSON 对象"))?;
    let end = inner.rfind('}').ok_or_else(|| anyhow::anyhow!("JSON 对象未闭合"))?;
    let json_str = &inner[start..=end];
    let v: serde_json::Value = serde_json::from_str(json_str)
        .with_context(|| "裁判输出不是合法 JSON")?;
    // 顺序 AB 与 BA 都返回 {A:…, B:…}：A/B 缺任一即报错（输出畸形作废，
    // 不把 B 当 A 兜底——判定的 A 胜/平/B 胜三态依赖两份分数）
    let parse_doc = |key: &str| -> Result<[f64; 5]> {
        let doc = v
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("缺少 {key} 文档分数"))?;
        let mut scores = [0.0f64; 5];
        for (i, dim) in ["clarity", "readability", "conciseness", "richness", "structure"]
            .iter()
            .enumerate()
        {
            scores[i] = doc
                .get(*dim)
                .and_then(|x| x.as_f64())
                .ok_or_else(|| anyhow::anyhow!("缺少维度 {dim}"))?
                .clamp(0.0, 10.0);
        }
        Ok(scores)
    };
    Ok((parse_doc("A")?, parse_doc("B")?))
}

/// v32（6.4 FR-101）：RepoDocBench 对齐五维聚合摘要。
///
/// 五维 = Coverage（实体提及率）/ Doc Information（LLM 判定+文本统计并存）/
/// Completeness@K / TQS / Update Recall。各维缺失时**降级跳过并显式标注**
/// （FR-101：不得静默）——缺失来源：LLM 不可用（doc_info 判定与
/// completeness 降级、TQS None）、导出快照缺失（TQS None）、
/// 非 git 仓库/快照缺失（Update Recall 无提交可扫描）。
pub fn render_repodoc(report: &BenchReport) -> String {
    let mut out = String::from("## RepoDocBench 对齐五维报告\n\n");
    // 维度 1：Coverage（实体提及率）——恒可计算，无降级路径
    out.push_str(&format!(
        "- **Coverage 实体提及率**: {:.2}（{}/{} 实体被产物提及）\n",
        report.coverage.ratio,
        report.coverage.covered_entities,
        report.coverage.total_entities
    ));
    // 维度 2：Doc Information——LLM 判定与文本统计并存；LLM 不可用时
    // 判定维降级跳过（llm_judged=false 由 measure_doc_info_llm 显式标注）
    if report.doc_info.llm_judged {
        out.push_str(&format!(
            "- **Doc Information**: LLM 判定 {:.2}/10（{} 页判定，{} abstain）；文本统计 {} 页/{} 词/{} 交叉引用\n",
            report.doc_info.llm_score,
            report.doc_info.llm_judged_modules,
            report.doc_info.llm_abstain_modules,
            report.doc_info.pages,
            report.doc_info.words,
            report.doc_info.cross_references
        ));
    } else {
        out.push_str(&format!(
            "- **Doc Information**: LLM 判定降级跳过（LLM 不可用）；文本统计 {} 页/{} 词/{} 交叉引用\n",
            report.doc_info.pages, report.doc_info.words, report.doc_info.cross_references
        ));
    }
    // 维度 3：Completeness@K——text 索引缺失降级（judged=false 显式标注）
    if report.completeness.judged {
        out.push_str(&format!(
            "- **Completeness@K**: {:.2}（{}/{} 实体命中所属模块页，K={}）\n",
            report.completeness.ratio,
            report.completeness.hit_entities,
            report.completeness.total_entities,
            report.completeness.k
        ));
    } else {
        out.push_str("- **Completeness@K**: 降级跳过（text 索引缺失——未生成或索引不可用）\n");
    }
    // 维度 4：TQS——LLM 裁判；快照缺失/LLM 不可用 → None（降级标注）
    match &report.tqs {
        Some(t) => out.push_str(&format!(
            "- **TQS**: {:.2}（{} 模块，judge {}）\n",
            t.avg_total, t.judged_modules, t.judge_model
        )),
        None => out.push_str("- **TQS**: 降级跳过（导出快照缺失或 LLM 不可用，详见日志）\n"),
    }
    // 维度 5：Update Recall——非 git 仓库/快照缺失 → 0 提交（降级标注）
    if report.update_recall.commits_scanned == 0 {
        out.push_str("- **Update Recall**: 降级跳过（非 git 仓库或快照缺失）\n");
    } else {
        out.push_str(&format!(
            "- **Update Recall**: {:.2}（扫描 {} 提交/{} 变更提交/{} 正确更新）\n",
            report.update_recall.recall,
            report.update_recall.commits_scanned,
            report.update_recall.commits_with_changes,
            report.update_recall.correctly_updated
        ));
    }
    out.push('\n');
    out
}

/// 渲染 Markdown 报告（人类可读，CI/人工复跑对比用）
pub fn render_markdown(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 评测报告: {}\n\n", report.repo_name));
    out.push_str(&format!("> 生成时间: {}\n\n", report.generated_at));

    out.push_str("## 1. 实体覆盖率（Coverage）\n\n");
    out.push_str(&format!(
        "- 实体总数: {}\n- 已覆盖: {}（{:.1}%）\n\n",
        report.coverage.total_entities,
        report.coverage.covered_entities,
        report.coverage.ratio * 100.0
    ));

    out.push_str("## 2. 文本统计（Doc Info）\n\n");
    out.push_str(&format!(
        "- 页面: {}\n- 词数: {}\n- 交叉引用: {}\n- 代码块: {}\n- Mermaid 图: {}\n",
        report.doc_info.pages,
        report.doc_info.words,
        report.doc_info.cross_references,
        report.doc_info.code_blocks,
        report.doc_info.diagrams
    ));
    // v32（6.2 FR-101）：LLM 信息性判定与文本统计并存；未执行时
    // 显式标注降级（不静默）
    if report.doc_info.llm_judged {
        out.push_str(&format!(
            "- LLM 信息性评分（0-10）: {:.2}（判定 {} 页，abstain {} 页）\n",
            report.doc_info.llm_score,
            report.doc_info.llm_judged_modules,
            report.doc_info.llm_abstain_modules
        ));
    } else {
        out.push_str("- LLM 信息性判定: 未执行（LLM 不可用，降级跳过）\n");
    }
    out.push('\n');

    // v32（6.3 FR-104）：Completeness@K 文档可检索性；text 索引缺失
    // 时显式标注降级（FR-101 不静默）
    out.push_str("## 3. Completeness@K（文档可检索性）\n\n");
    if report.completeness.judged {
        out.push_str(&format!(
            "- 实体总数: {}\n- 命中实体数（top-{} 检索命中所属模块页）: {}\n- 命中率: {:.2}\n",
            report.completeness.total_entities,
            report.completeness.k,
            report.completeness.hit_entities,
            report.completeness.ratio
        ));
    } else {
        out.push_str("- 未执行（text 索引缺失——未生成或索引不可用，降级跳过）\n");
    }
    out.push('\n');

    out.push_str("## 4. lint 健康\n\n");
    if report.lint.total_issues == 0 {
        out.push_str("- 通过（无孤儿页/断链/过时/引用/覆盖/mermaid 问题）\n\n");
    } else {
        out.push_str(&format!("- 问题总数: {}\n", report.lint.total_issues));
        for (kind, count) in &report.lint.by_kind {
            out.push_str(&format!("  - {kind}: {count}\n"));
        }
        out.push('\n');
    }

    out.push_str("## 5. 增量召回（Update Recall）\n\n");
    if report.update_recall.commits_scanned == 0 {
        // v21 D 组：--rubrics-only 明确标注跳过，避免误读为"无 commit 可回放"
        out.push_str("- 跳过（--rubrics-only 模式：不执行 git commit 回放）\n\n");
    } else {
        out.push_str(&format!(
            "- 回放 commit: {}（上限 {}）\n- 有变更: {}\n- 正确更新: {}（{:.1}%）\n\n",
            report.update_recall.commits_scanned,
            MAX_RECALL_COMMITS,
            report.update_recall.commits_with_changes,
            report.update_recall.correctly_updated,
            report.update_recall.recall * 100.0
        ));
    }

    out.push_str("## 6. 耗时（Time）\n\n");
    out.push_str(&format!(
        "- 扫描: {}ms\n- 增量: {}ms\n- 总计: {}ms\n",
        report.time.scan_ms, report.time.generate_ms, report.time.total_ms
    ));
    // v32 8.1：分段计时（update_recall 回放后的 last_timings.json；缺失不输出）
    if let Some(t) = &report.timings {
        out.push_str(&format!(
            "- 分段: 扫描/解析 {}ms | 图构建 {}ms | 增量分析 {}ms | 分块 {}ms | 卡片 {}ms | Wiki 页 {}ms | 阅读指南 {}ms | 渲染 {}ms | 索引 {}ms | 状态 {}ms | 总计 {}ms\n",
            t.scan_parse_ms, t.graph_ms, t.incremental_ms, t.chunk_ms, t.card_ms,
            t.wiki_ms, t.index_guide_ms, t.render_ms, t.index_ms, t.state_ms, t.total_ms
        ));
    }

    out.push_str("## 7. TQS 文本质量（LLM 裁判，--judge）\n\n");
    if let Some(tqs) = &report.tqs {
        out.push_str(&format!(
            "- 判定模块: {}（有效 {}，复测 {} 轮/模块，裁判 {}\n- Clarity: {:.1}\n- Readability: {:.1}\n- Conciseness: {:.1}\n- Richness: {:.1}\n- Structure: {:.1}\n- 总分: {:.1}\n- 复测一致性（κ 近似）: {:.2}\n- 机会校正 κ: {:.2}\n- 位置偏差 |P(A胜)−0.5|: {:.2}\n- 复测标准差: {:.2}\n",
            tqs.judged_modules,
            tqs.eligible_modules,
            tqs.repeats,
            tqs.judge_model,
            tqs.avg_clarity,
            tqs.avg_readability,
            tqs.avg_conciseness,
            tqs.avg_richness,
            tqs.avg_structure,
            tqs.avg_total,
            tqs.kappa_like,
            tqs.kappa,
            tqs.position_bias,
            tqs.avg_std
        ));
        out.push_str(&format!(
            "- 标准 Cohen's κ（AB/BA 交换一致，机会校正）: {:.2}\n- 判定翻转率（相对模块多数判定）: {:.2}\n- 位置翻转率（逐对 AB↔BA 交换）: {:.2}\n- κ 通缩 Δκ（一致率−机会校正）: {:.2}\n- 解析成功率: {:.2}\n- v32 三态明细（A 胜/B 胜/平局）: {}/{}/{}\n- 平局率（模块级平均，三态判定中 tie 占比）: {:.2}\n",
            tqs.kappa_cohen,
            tqs.flip_rate,
            tqs.position_flip_rate,
            tqs.delta_kappa,
            tqs.parse_success_rate,
            tqs.agreement_breakdown[0],
            tqs.agreement_breakdown[1],
            tqs.agreement_breakdown[2],
            tqs.tie_rate
        ));
        out.push_str(&format!(
            "- 判定尺度: {}\n- 聚合层级: {}\n- tie/abstain 处理: {}\n",
            tqs.judgment_scale, tqs.aggregation_level, tqs.tie_handling
        ));
        if !tqs.low_confidence_modules.is_empty() {
            out.push_str(&format!(
                "- 低置信模块（复测失败或波动大）: {}\n",
                tqs.low_confidence_modules.join(", ")
            ));
        }
    } else {
        out.push_str("- 未启用（使用 --judge 且配置 LLM API key 后启用）\n\n");
    }

    out.push_str("## 8. Rubric 层级完整性（LLM 裁判，--judge）\n\n");
    if let Some(rubric) = &report.rubric {
        out.push_str(&format!(
            "- 节点 {} 个（叶子 {} 个，满足 {} 个），生成 {} 次 LLM 调用，裁判 {}\n- 覆盖率: {:.1}%（基于有效判定叶子）\n- 加权总分 S: {:.3}（σ_R {:.3}）\n",
            rubric.rubric_nodes,
            rubric.leaf_count,
            rubric.satisfied_leaves,
            rubric.generation_calls,
            rubric.judge_model,
            rubric.coverage * 100.0,
            rubric.score,
            rubric.score_std
        ));
        out.push_str(&format!(
            "- abstain 叶子: {}（{:.1}%，不计入覆盖率）\n- 叶子判定: {} 次多数投票/叶子\n- 聚合层级: {}\n\n",
            rubric.abstain_leaves,
            rubric.abstain_rate * 100.0,
            rubric.leaf_verdict_repeats,
            rubric.aggregation_level
        ));
    } else {
        out.push_str("- 未启用（使用 --judge 且被测仓库有 README/docs 时启用）\n\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{LlmProviderType, LlmSection, WikiSection};
    use std::path::PathBuf;

    /// 构造临时小仓库：src/a.rs + src/b.rs（含 git 仓库，供增量回放）
    fn bench_repo(tag: &str) -> (ProjectRoot, PathBuf, WikiConfig) {
        let dir = std::env::temp_dir().join(format!("repo_wiki_bench_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha(x: u32) -> u32 { x + 1 }\n").unwrap();
        std::fs::write(dir.join("src").join("b.rs"), "pub fn beta(x: u32) -> u32 { x + 2 }\n").unwrap();

        let config = WikiConfig {
            output_dir: Some((dir.join(".repo-wiki").to_string_lossy().into_owned()).into()),
            wiki: WikiSection { language: "zh".into(), guide: Default::default() },
            llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
            ..Default::default()
        };
        std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();

        // git init（增量回放的前置条件；首次提交需签名）
        let git = git2::Repository::init(&dir).unwrap();
        let mut cfg = git.config().unwrap();
        cfg.set_str("user.name", "bench").unwrap();
        cfg.set_str("user.email", "bench@test.com").unwrap();
        let root = ProjectRoot::new(dir.clone());
        (root, dir.join("config.toml"), config)
    }

    /// git2 提交当前工作区，返回 commit id
    fn commit_all(repo_path: &Path, message: &str) -> String {
        let repo = git2::Repository::open(repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("bench", "bench@test.com").unwrap();
        let commit_id = match repo.head().ok() {
            Some(head) => {
                let parent = head.peel_to_commit().unwrap();
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent]).unwrap()
            }
            None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]).unwrap(),
        };
        commit_id.to_string()
    }

    /// 覆盖率：全量生成后实体应全部被提及（mock 产物含模块页）
    #[test]
    fn test_coverage_after_generate() {
        let (root, config_path, config) = bench_repo("cov");
        commit_all(root.path(), "init");
        crate::run_pipeline(Some(&config_path), None, false, &root, &crate::GenerationMode::Full).unwrap();

        let pages = collect_wiki_pages(config.output_dir());
        assert!(!pages.is_empty(), "全量生成后应有产物页");
        let cov = measure_coverage(&root, &pages).unwrap();
        assert_eq!(cov.total_entities, 2, "应解析出 alpha/beta 两个实体");
        assert_eq!(cov.covered_entities, 2, "mock 生成后产物应提及全部实体");
        assert!((cov.ratio - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// v32（6.3 FR-104）：Completeness@K——索引条目同模块且模块页存在时命中
    ///
    /// 受控构造：手动建 text 索引（pipeline 同款路径 index_dir/text_index.db），
    /// 索引条目 module_path=["src","net"]（与 chunk_by_file 同规则），
    /// 产物模块页 src_net_tcp.md（wiki_file_name 同规则）存在。
    #[test]
    fn test_completeness_hit_when_module_page_exists() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_bench_ckhit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src").join("net")).unwrap();
        std::fs::write(
            dir.join("src").join("net").join("tcp.rs"),
            "pub fn tcp_fn(x: u32) -> u32 { x }\n",
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some((dir.join(".repo-wiki").to_string_lossy().into_owned()).into()),
            wiki: WikiSection { language: "zh".into(), guide: Default::default() },
            llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
            ..Default::default()
        };
        let index_dir = crate::search_index_dir(&config);
        std::fs::create_dir_all(&index_dir).unwrap();
        let mut engine =
            crate::search::text::TextEngine::open(index_dir.join("text_index.db")).unwrap();
        engine
            .index_batch(&[(
                crate::model::CodeNode {
                    id: crate::model::NodeId::new(0),
                    kind: crate::model::NodeKind::Function,
                    name: "tcp_fn".into(),
                    // 刻意指向同模块目录下的「另一个文件」（src/net/tcp2.rs 并不
                    // 真实存在，仅索引条目）：目录级模块判定（module_of 派生）
                    // 必须命中；若实现退化为文件级精确匹配此处为 0（v32 6.3
                    // 审查：同模块不同文件断言）。
                    file_path: Some("src/net/tcp2.rs".into()),
                    line_range: None,
                    doc_comment: None,
                    signature: Some("pub fn tcp_fn(x: u32) -> u32".into()),
                    visibility: None,
                    // 镜像 graph::build 的真实构造：父目录 + 文件 stem
                    // （graph.rs:82-85）。判定按 file_path 派生模块，
                    // 与 module_path 字段无关——此处刻意保持生产形态，
                    // 防止未来实现改回 module_path 比较时夹具静默放行
                    // （测试/生产分叉教训）。
                    module_path: vec!["src".into(), "net".into(), "tcp2".into()],
                },
                "pub fn tcp_fn(x: u32) -> u32 { x }".to_string(),
            )])
            .unwrap();

        let root = ProjectRoot::new(dir.clone());
        // 模块页 src_net.md（模块名 src::net 的页面，wiki_file_name 同规则）
        let pages = vec![(dir.join(".repo-wiki/wiki/zh/src_net.md"), "content".to_string())];
        let rep = measure_completeness_at_k(&root, &config, &pages).unwrap();
        assert!(rep.judged, "索引存在应执行判定");
        assert_eq!(rep.total_entities, 1);
        assert_eq!(
            rep.hit_entities, 1,
            "目录级模块判定：索引条目文件与实体文件不同（tcp2.rs vs tcp.rs）仍命中；若实现退化为文件级精确匹配此处为 0"
        );
        assert_eq!(rep.k, 10, "FR-104 固定 top-K=10");
        assert!((rep.ratio - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// v32（6.3 FR-104）：产物缺模块页时同模块条目不命中（可检索性判定）
    #[test]
    fn test_completeness_miss_when_module_page_absent() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_bench_ckmiss_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src").join("a.rs"),
            "pub fn alpha(x: u32) -> u32 { x }\n",
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some((dir.join(".repo-wiki").to_string_lossy().into_owned()).into()),
            wiki: WikiSection { language: "zh".into(), guide: Default::default() },
            llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
            ..Default::default()
        };
        let index_dir = crate::search_index_dir(&config);
        std::fs::create_dir_all(&index_dir).unwrap();
        let mut engine =
            crate::search::text::TextEngine::open(index_dir.join("text_index.db")).unwrap();
        engine
            .index_batch(&[(
                crate::model::CodeNode {
                    id: crate::model::NodeId::new(0),
                    kind: crate::model::NodeKind::Function,
                    name: "alpha".into(),
                    file_path: Some("src/a.rs".into()),
                    line_range: None,
                    doc_comment: None,
                    signature: Some("pub fn alpha(x: u32) -> u32".into()),
                    visibility: None,
                    // 镜像 graph::build 真实构造（父目录 + 文件 stem）
                    module_path: vec!["src".into(), "a".into()],
                },
                "pub fn alpha(x: u32) -> u32 { x }".to_string(),
            )])
            .unwrap();

        let root = ProjectRoot::new(dir.clone());
        // pages 为空：模块页 src.md 不存在 → 不命中
        let rep = measure_completeness_at_k(&root, &config, &[]).unwrap();
        assert!(rep.judged, "索引存在仍执行判定");
        assert_eq!(rep.total_entities, 1);
        assert_eq!(rep.hit_entities, 0, "模块页缺失不应命中");
        assert!((rep.ratio - 0.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// v32（6.3 FR-101）：text 索引缺失 → 降级跳过（judged=false 显式标注）
    #[test]
    fn test_completeness_degrades_without_index() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_bench_ckdeg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src").join("a.rs"),
            "pub fn alpha(x: u32) -> u32 { x }\n",
        )
        .unwrap();

        let config = WikiConfig {
            output_dir: Some((dir.join(".repo-wiki").to_string_lossy().into_owned()).into()),
            wiki: WikiSection { language: "zh".into(), guide: Default::default() },
            llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
            ..Default::default()
        };
        let root = ProjectRoot::new(dir.clone());
        // 未建索引：search_index_dir 不存在
        let rep = measure_completeness_at_k(&root, &config, &[]).unwrap();
        assert!(!rep.judged, "索引缺失应降级跳过");
        assert_eq!(rep.total_entities, 1, "实体统计仍给出（与 coverage 同源）");
        assert_eq!(rep.ratio, 0.0, "降级时不虚报命中率");

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// v32（6.3）：模块名派生规则（与 chunk_by_file/collect_index_items 同规则）
    #[test]
    fn test_module_of_rules() {
        assert_eq!(module_of(std::path::Path::new("src/net/tcp.rs")), "src::net");
        assert_eq!(
            module_of(std::path::Path::new("tcp.rs")),
            "",
            "根目录文件模块为空串"
        );
    }

    /// 覆盖率：无产物时覆盖率为 0（实体存在但无页面提及）
    #[test]
    fn test_coverage_zero_without_pages() {
        let (root, _, config) = bench_repo("cov0");
        let pages = collect_wiki_pages(config.output_dir());
        assert!(pages.is_empty());
        let cov = measure_coverage(&root, &pages).unwrap();
        assert_eq!(cov.total_entities, 2);
        assert_eq!(cov.covered_entities, 0);
        assert!((cov.ratio - 0.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 文本统计：含 mermaid 与链接的页面各计数正确
    #[test]
    fn test_doc_info_counts() {
        let pages = vec![(
            PathBuf::from("wiki/zh/x.md"),
            "# 标题\n\n[模块](wiki/zh/a.md) 与 [源码](src/a.rs:1)。\n\n```rust\nfn x() {}\n```\n\n```mermaid\nflowchart LR\nA --> B\n```\n".to_string(),
        )];
        let info = measure_doc_info(&pages);
        assert_eq!(info.pages, 1);
        assert_eq!(info.cross_references, 2);
        assert_eq!(info.code_blocks, 2);
        assert_eq!(info.diagrams, 1);
        assert!(info.words > 0);
        // v32（6.2）：文本统计函数不触发 LLM 判定（字段默认未执行）
        assert!(!info.llm_judged);
        assert_eq!(info.llm_score, 0.0);
    }

    /// v32（6.2 FR-101/FR-102）：Doc Info LLM 判定解析——0-10 评分
    /// clamp、uncertain 三态、非法输出 → Unparseable
    #[test]
    fn test_parse_doc_info_score() {
        assert!(matches!(
            parse_doc_info_score(r#"{"score": 8}"#),
            DocInfoVerdict::Score(s) if (s - 8.0).abs() < 1e-9
        ));
        assert!(matches!(
            parse_doc_info_score("```json\n{\"score\": 11}\n```"),
            DocInfoVerdict::Score(s) if (s - 10.0).abs() < 1e-9
        ), "越界评分应 clamp 到 10");
        assert!(matches!(
            parse_doc_info_score(r#"{"score": -3}"#),
            DocInfoVerdict::Score(s) if s.abs() < 1e-9
        ), "负分应 clamp 到 0");
        assert!(matches!(
            parse_doc_info_score(r#"{"verdict": "uncertain"}"#),
            DocInfoVerdict::Uncertain
        ));
        assert!(matches!(parse_doc_info_score("no json"), DocInfoVerdict::Unparseable));
        assert!(matches!(parse_doc_info_score(r#"{"score": "高"}"#), DocInfoVerdict::Unparseable));
        assert!(matches!(parse_doc_info_score(r#"{}"#), DocInfoVerdict::Unparseable));
    }

    /// 增量召回：有变更的 commit 应全部触发重生成（mock 下正确更新）
    #[test]
    fn test_update_recall_with_changes() {
        let (root, config_path, _config) = bench_repo("recall");
        commit_all(root.path(), "init");
        crate::run_pipeline(Some(&config_path), None, false, &root, &crate::GenerationMode::Full).unwrap();

        // 第二个 commit：修改 b.rs
        std::fs::write(root.path().join("src").join("b.rs"), "pub fn beta(x: u32) -> u32 { x + 100 }\n").unwrap();
        commit_all(root.path(), "change beta");

        let report = measure_update_recall(Some(&config_path), &root).unwrap();
        assert_eq!(report.commits_scanned, 2, "应回放 2 个 commit");
        assert_eq!(report.commits_with_changes, 1, "第 2 个 commit 有变更");
        assert_eq!(report.correctly_updated, 1, "变更 commit 应正确触发重生成");
        assert!((report.recall - 1.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// v21 D 组：--rubrics-only 模式跳过 git 回放，快维度仍正常
    /// （在已有产物、多 commit 的仓库上验证：recall 占位 0/1.0，渲染标注跳过）
    #[test]
    fn test_run_rubrics_only_skips_replay() {
        let (root, config_path, config) = bench_repo("rubonly");
        commit_all(root.path(), "init");
        crate::run_pipeline(Some(&config_path), None, false, &root, &crate::GenerationMode::Full).unwrap();
        std::fs::write(root.path().join("src").join("b.rs"), "pub fn beta(x: u32) -> u32 { x + 100 }\n").unwrap();
        commit_all(root.path(), "change beta");

        let report = run_rubrics_only(&root, &config, "demo").unwrap();
        assert_eq!(report.update_recall.commits_scanned, 0, "rubrics-only 不执行回放");
        assert_eq!(report.update_recall.correctly_updated, 0);
        assert_eq!(report.time.generate_ms, 0, "无生成耗时");
        assert_eq!(report.coverage.total_entities, 2, "快维度（Coverage）仍正常");
        assert!(report.doc_info.pages > 0, "mock 生成后应有产物页（快维度 Doc Info 正常）");
        // lint 计数本身有效即可（mock 产物可能存在已知噪声，不在此处断言为 0）
        let md = render_markdown(&report);
        assert!(md.contains("跳过（--rubrics-only 模式"), "渲染应标注回放跳过: {md}");

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 报告渲染：Markdown 报告含五个维度标题
    #[test]
    fn test_render_markdown_sections() {
        let report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 0, covered_entities: 0, ratio: 1.0 },
            doc_info: DocInfoReport {
    pages: 0,
    words: 0,
    cross_references: 0,
    code_blocks: 0,
    diagrams: 0,
    llm_judged: false,
    llm_score: 0.0,
    llm_judged_modules: 0,
    llm_abstain_modules: 0,
},
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport { commits_scanned: 0, commits_with_changes: 0, correctly_updated: 0, recall: 1.0 },
            time: TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 },
            timings: None,
            tqs: None,
            rubric: None,
            completeness: CompletenessReport {
                total_entities: 0,
                hit_entities: 0,
                k: 10,
                ratio: 1.0,
                judged: false,
            },
        };
        let md = render_markdown(&report);
        for section in ["实体覆盖率", "文本统计", "lint 健康", "增量召回", "耗时"] {
            assert!(md.contains(section), "报告应含 {section} 节: {md}");
        }
    }

    /// U11：裁判 JSON 解析——围栏剥离 + 理由前缀容错 + 越界 clamp。
    /// t04 起返回 (A, B) 双分数（逐对判定指标需要两份分数）
    #[test]
    fn test_parse_tqs_score_tolerates_fences_and_prose() {
        let content = "理由：A 更清晰。\n```json\n{\"A\": {\"clarity\": 8.5, \"readability\": 7, \"conciseness\": 12, \"richness\": 6, \"structure\": 9}, \"B\": {\"clarity\": 7, \"readability\": 6, \"conciseness\": 8, \"richness\": 5, \"structure\": 7}}\n```\n";
        let (a, b) = parse_tqs_score(content).unwrap();
        assert_eq!(a[0], 8.5, "clarity");
        assert_eq!(a[2], 10.0, "conciseness 越界应 clamp 到 10");
        assert_eq!(b[0], 7.0, "B 分数应独立解析");
    }

    /// v22 修复：Rubric 解析容错——字符串 sub_tasks 转叶子、字符串权重
    /// 可解析（实测 8192 预算档 deepseek-v4-flash 输出字符串数组）
    #[test]
    fn test_parse_rubric_tree_tolerates_string_subtasks() {
        let content = r#"```json
{"rubrics": [
  {"requirement": "架构文档应描述流水线", "weight": 2, "sub_tasks": ["介绍解析阶段", "说明图构建", {"requirement": "增量语义", "weight": "3", "sub_tasks": []}]},
  {"requirement": "索引应可搜索", "weight": 1}
]}
```"#;
        let nodes = parse_rubric_tree(content).unwrap();
        assert_eq!(nodes.len(), 2, "两个顶层需求");
        let first = &nodes[0];
        assert_eq!(first.requirement, "架构文档应描述流水线");
        assert_eq!(first.sub_tasks.len(), 3, "字符串子任务应转叶子节点");
        assert_eq!(first.sub_tasks[0].requirement, "介绍解析阶段");
        assert!(first.sub_tasks[0].sub_tasks.is_empty(), "字符串子任务是叶子");
        assert_eq!(first.sub_tasks[1].weight, 1.0, "字符串叶子权重取 1.0");
        assert_eq!(first.sub_tasks[2].weight, 3.0, "字符串权重应可解析为数字");
        assert_eq!(nodes[1].weight, 1.0, "缺省 weight 回落 1.0");
    }

    /// U11：缺 A/B/维度/非 JSON → 报错（整条作废，不静默裁剪；
    /// t04 起 A 或 B 缺任一即报错，不把 B 当 A 兜底）
    #[test]
    fn test_parse_tqs_score_rejects_missing_field() {
        let content = r#"{"A": {"clarity": 8, "readability": 7}}"#;
        assert!(parse_tqs_score(content).is_err(), "缺 B 文档应报错");
        let only_b = r#"{"B": {"clarity": 8, "readability": 7, "conciseness": 6, "richness": 5, "structure": 4}}"#;
        assert!(parse_tqs_score(only_b).is_err(), "缺 A 文档应报错");
        let full = r#"{"A": {"clarity": 8, "readability": 7, "conciseness": 6, "richness": 5, "structure": 4}, "B": {"clarity": 1, "readability": 2, "conciseness": 3, "richness": 4, "structure": 5}}"#;
        assert!(parse_tqs_score(full).is_ok(), "A/B 齐全应解析成功");
        assert!(parse_tqs_score("no json here").is_err(), "非 JSON 应报错");
    }

    /// v32（6.4 FR-101）：--repodoc 五维聚合摘要——全维可用时输出各维数值
    #[test]
    fn test_render_repodoc_all_dimensions_judged() {
        let report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 100, covered_entities: 87, ratio: 0.87 },
            doc_info: DocInfoReport {
                pages: 5,
                words: 1200,
                cross_references: 30,
                code_blocks: 3,
                diagrams: 1,
                llm_judged: true,
                llm_score: 6.5,
                llm_judged_modules: 5,
                llm_abstain_modules: 1,
            },
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport {
                commits_scanned: 2,
                commits_with_changes: 2,
                correctly_updated: 2,
                recall: 1.0,
            },
            time: TimeReport { scan_ms: 1, generate_ms: 2, total_ms: 3 },
            timings: None,
            tqs: Some(TqsReport {
                judged_modules: 2,
                avg_clarity: 8.0,
                avg_readability: 7.5,
                avg_conciseness: 6.0,
                avg_richness: 7.0,
                avg_structure: 8.5,
                avg_total: 7.4,
                repeats: 5,
                kappa_like: 1.0,
                kappa: 0.5,
                position_bias: 0.05,
                low_confidence_modules: Vec::new(),
                avg_std: 0.5,
                judge_model: "mock-model".into(),
                kappa_cohen: 0.8,
                flip_rate: 0.1,
                position_flip_rate: 0.2,
                delta_kappa: 0.5,
                eligible_modules: 2,
                parse_success_rate: 1.0,
                judgment_scale: "0-10 连续五维点分".into(),
                aggregation_level: "模块级".into(),
                tie_handling: "exclude".into(),
                tie_rate: 0.0,
                agreement_breakdown: [10, 10, 0],
            }),
            rubric: None,
            completeness: CompletenessReport {
                total_entities: 100,
                hit_entities: 80,
                k: 10,
                ratio: 0.8,
                judged: true,
            },
        };
        let s = render_repodoc(&report);
        assert!(s.contains("**Coverage 实体提及率**: 0.87"), "Coverage 行: {s}");
        assert!(s.contains("LLM 判定 6.50/10"), "Doc Info LLM 判定行: {s}");
        assert!(s.contains("5 页判定，1 abstain"), "abstain 数暴露: {s}");
        assert!(s.contains("**Completeness@K**: 0.80"), "Completeness 行: {s}");
        assert!(s.contains("**TQS**: 7.40"), "TQS 行: {s}");
        assert!(s.contains("**Update Recall**: 1.00"), "Update Recall 行: {s}");
        assert!(!s.contains("降级跳过"), "全维可用时不应出现降级标注: {s}");
    }

    /// v32（6.4 FR-101）：各维缺失时降级跳过并显式标注（不得静默）
    #[test]
    fn test_render_repodoc_degraded_dimensions_annotated() {
        let report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 10, covered_entities: 5, ratio: 0.5 },
            doc_info: DocInfoReport {
                pages: 2,
                words: 300,
                cross_references: 4,
                code_blocks: 0,
                diagrams: 0,
                llm_judged: false,
                llm_score: 0.0,
                llm_judged_modules: 0,
                llm_abstain_modules: 0,
            },
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport {
                commits_scanned: 0,
                commits_with_changes: 0,
                correctly_updated: 0,
                recall: 1.0,
            },
            time: TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 },
            timings: None,
            tqs: None,
            rubric: None,
            completeness: CompletenessReport {
                total_entities: 10,
                hit_entities: 0,
                k: 10,
                ratio: 0.0,
                judged: false,
            },
        };
        let s = render_repodoc(&report);
        assert!(s.contains("**Doc Information**: LLM 判定降级跳过"), "LLM 判定降级标注: {s}");
        assert!(s.contains("**Completeness@K**: 降级跳过"), "Completeness 降级标注: {s}");
        assert!(s.contains("**TQS**: 降级跳过"), "TQS 降级标注: {s}");
        assert!(s.contains("**Update Recall**: 降级跳过"), "Update Recall 降级标注: {s}");
        assert!(s.contains("文本统计 2 页"), "降级时文本统计仍输出: {s}");
        assert!(s.contains("**Coverage 实体提及率**: 0.50"), "Coverage 恒输出: {s}");
        assert!(!s.contains("LLM 判定 0.00/10"), "降级分支不得伪装成执行: {s}");
    }

    /// U11：报告渲染——启用时输出五维分数，未启用时提示 --judge
    #[test]
    fn test_render_markdown_tqs_section() {
        let mut report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 0, covered_entities: 0, ratio: 1.0 },
            doc_info: DocInfoReport {
                pages: 0,
                words: 0,
                cross_references: 0,
                code_blocks: 0,
                diagrams: 0,
                llm_judged: false,
                llm_score: 0.0,
                llm_judged_modules: 0,
                llm_abstain_modules: 0,
            },
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport { commits_scanned: 0, commits_with_changes: 0, correctly_updated: 0, recall: 1.0 },
            time: TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 },
            timings: None,
            tqs: None,
            rubric: None,
            completeness: CompletenessReport {
                total_entities: 0,
                hit_entities: 0,
                k: 10,
                ratio: 1.0,
                judged: false,
            },
        };
        let md_off = render_markdown(&report);
        assert!(md_off.contains("--judge"), "未启用时应提示 --judge: {md_off}");

        report.tqs = Some(TqsReport {
            judged_modules: 2,
            avg_clarity: 8.0,
            avg_readability: 7.5,
            avg_conciseness: 6.0,
            avg_richness: 7.0,
            avg_structure: 8.5,
            avg_total: 7.4,
            repeats: 5,
            kappa_like: 1.0,
            kappa: 0.5,
            position_bias: 0.05,
            low_confidence_modules: Vec::new(),
            avg_std: 0.5,
            judge_model: "mock-model".into(),
            kappa_cohen: 0.8,
            flip_rate: 0.1,
            position_flip_rate: 0.2,
            delta_kappa: 0.5,
            eligible_modules: 2,
            parse_success_rate: 1.0,
            judgment_scale: "0-10 连续五维点分".into(),
            aggregation_level: "模块级 macro average".into(),
            tie_handling: "三态判定；失败模块排除".into(),
            tie_rate: 0.1,
            agreement_breakdown: [8, 9, 3],
        });
        let md_on = render_markdown(&report);
        assert!(md_on.contains("判定模块: 2"), "应输出判定模块数: {md_on}");
        assert!(md_on.contains("Clarity: 8.0"), "应输出五维分数: {md_on}");
        assert!(md_on.contains("复测一致"), "应输出 MVVP 复测一致性: {md_on}");
        assert!(md_on.contains("位置偏差"), "应输出位置偏差: {md_on}");
        assert!(md_on.contains("标准 Cohen's κ"), "应输出标准 κ: {md_on}");
        assert!(md_on.contains("判定翻转率"), "应输出翻转率: {md_on}");
        assert!(md_on.contains("三态明细"), "应输出三态明细: {md_on}");
        assert!(md_on.contains("平局率"), "应输出平局率: {md_on}");
        assert!(md_on.contains("判定尺度"), "应输出判定尺度声明: {md_on}");
    }

    /// v14 C 组：Rubric JSON 解析——围栏剥离/数组形态/rubrics 键形态/单对象形态
    #[test]
    fn test_parse_rubric_tree_forms() {
        let array_form = r#"```json
[{"requirement": "a", "weight": 2, "sub_tasks": [{"requirement": "b", "weight": 1}]}]
```"#;
        let tree = parse_rubric_tree(array_form).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].sub_tasks.len(), 1, "子任务应解析");

        let obj_form = r#"{"rubrics": [{"requirement": "x", "weight": 3}]}"#;
        let tree = parse_rubric_tree(obj_form).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].requirement, "x");

        let single_form = r#"{"requirement": "solo", "weight": 1}"#;
        let tree = parse_rubric_tree(single_form).unwrap();
        assert_eq!(tree.len(), 1, "单对象应视为单节点树");
        assert!(parse_rubric_tree("not json").is_err(), "非 JSON 应报错");
    }

    /// v14 C 组 + v32（6.1 FR-102）：叶子判定三态解析（satisfied/
    /// unsatisfied/uncertain）+ 权重 clamp + 加权聚合确定性
    #[test]
    fn test_rubric_aggregate_and_verdict() {
        use RubricVerdict as V;
        assert_eq!(parse_rubric_verdict(r#"{"verdict": "satisfied"}"#), Some(V::Satisfied));
        assert_eq!(
            parse_rubric_verdict("```json\n{\"verdict\": \"unsatisfied\"}\n```"),
            Some(V::Unsatisfied)
        );
        assert_eq!(parse_rubric_verdict(r#"{"verdict": "uncertain"}"#), Some(V::Uncertain));
        assert_eq!(parse_rubric_verdict(r#"{"verdict": "satisfied"}"#), Some(V::Satisfied), "围栏剥离");
        assert_eq!(parse_rubric_verdict("no json"), None);
        assert_eq!(parse_rubric_verdict(r#"{"verdict": "maybe"}"#), None, "非法三态值");
        assert_eq!(parse_rubric_verdict(r#"{"satisfied": true}"#), None, "旧字段不再接受");

        // 树：根(weight 1) → [a(2): 叶子, b(3): [c(1): 叶子, d(1): 叶子]]
        // 叶子判定 [true, false, true] → a=1, c=0, d=1 → b=(0+1)/2=0.5
        // S = (2·1 + 3·0.5)/5 = 0.7
        let node = RubricNode {
            requirement: "root".into(),
            weight: 1.0,
            sub_tasks: vec![
                RubricNode { requirement: "a".into(), weight: 2.0, sub_tasks: vec![] },
                RubricNode {
                    requirement: "b".into(),
                    weight: 3.0,
                    sub_tasks: vec![
                        RubricNode { requirement: "c".into(), weight: 1.0, sub_tasks: vec![] },
                        RubricNode { requirement: "d".into(), weight: 1.0, sub_tasks: vec![] },
                    ],
                },
            ],
        };
        let verdicts = vec![Some(true), Some(false), Some(true)];
        let mut idx = 0usize;
        let s = aggregate_score(&node, &verdicts, &mut idx);
        assert!((s.score - 0.7).abs() < 1e-9, "加权总分应为 0.7, 实际: {}", s.score);
        assert_eq!(s.leaves, 3);
        assert_eq!(s.satisfied, 2);
        assert_eq!(idx, 3, "叶子索引应遍历完");

        // 权重越界 clamp：weight=99 → 视为 3（LLM 输出越界收敛）
        let bad = RubricNode { requirement: "w".into(), weight: 99.0, sub_tasks: vec![] };
        assert_eq!(node_weight(bad.weight), 3.0);
    }

    /// v14 C 组：报告渲染第七节——启用时输出 Rubric 指标，未启用时提示
    #[test]
    fn test_render_markdown_rubric_section() {
        let mut report = BenchReport {
            repo_name: "demo".into(),
            generated_at: "2026-08-03T00:00:00Z".into(),
            coverage: CoverageReport { total_entities: 0, covered_entities: 0, ratio: 1.0 },
            doc_info: DocInfoReport {
    pages: 0,
    words: 0,
    cross_references: 0,
    code_blocks: 0,
    diagrams: 0,
    llm_judged: false,
    llm_score: 0.0,
    llm_judged_modules: 0,
    llm_abstain_modules: 0,
},
            lint: LintReport { total_issues: 0, by_kind: Default::default() },
            update_recall: UpdateRecallReport { commits_scanned: 0, commits_with_changes: 0, correctly_updated: 0, recall: 1.0 },
            time: TimeReport { scan_ms: 0, generate_ms: 0, total_ms: 0 },
            timings: None,
            tqs: None,
            rubric: None,
            completeness: CompletenessReport {
                total_entities: 0,
                hit_entities: 0,
                k: 10,
                ratio: 1.0,
                judged: false,
            },
        };
        let md_off = render_markdown(&report);
        assert!(md_off.contains("Rubric"), "应含 Rubric 节: {md_off}");

        report.rubric = Some(RubricReport {
            rubric_nodes: 5,
            leaf_count: 3,
            satisfied_leaves: 2,
            coverage: 2.0 / 3.0,
            score: 0.7,
            score_std: 0.35,
            generation_calls: 4,
            judge_model: "mock-model".into(),
            abstain_leaves: 0,
            abstain_rate: 0.0,
            leaf_verdict_repeats: 3,
            aggregation_level: "叶子级多数投票".into(),
        });
        let md_on = render_markdown(&report);
        assert!(md_on.contains("覆盖率: 66.7%"), "应输出覆盖率: {md_on}");
        assert!(md_on.contains("加权总分 S: 0.700"), "应输出加权总分: {md_on}");
        assert!(md_on.contains("abstain 叶子"), "应输出 abstain 指标: {md_on}");
        assert!(md_on.contains("多数投票"), "应输出叶子判定协议: {md_on}");
    }

    /// 方案甲：关键词提取——CJK 连续串 2-gram 切分 / 英文数字保留原样 /
    /// 空串与纯标点退化返回空 Vec
    #[test]
    fn test_extract_keywords() {
        let kws = extract_keywords("安装配置指南");
        assert_eq!(
            kws,
            vec!["安装", "装配", "配置", "置指", "指南"],
            "连续中文按滑动窗口 2-gram 切分"
        );
        assert!(!kws.contains(&"安".to_string()), "单字不成 2-gram");

        let mixed = extract_keywords("支持 Setup v2 认证");
        assert!(mixed.contains(&"Setup".to_string()), "英文词保留原样");
        assert!(mixed.contains(&"v2".to_string()), "英文+数字串保留原样");
        assert!(mixed.contains(&"认证".to_string()), "中文 2 字串切出一个 2-gram");

        assert!(extract_keywords("").is_empty(), "空串返回空");
        assert!(extract_keywords("！！！---").is_empty(), "纯标点无关键词返回空");
    }

    /// 方案甲：计数检索排序——命中数多者排前，无命中页不返回，
    /// 全无命中/空关键词返回空 Vec
    #[test]
    fn test_search_pages_ranks() {
        let pages = vec![
            (PathBuf::from("wiki/zh/a.md"), "安装 安装 安装 说明".into()),
            (PathBuf::from("wiki/zh/b.md"), "安装 安装 配置 配置 指南".into()),
            (PathBuf::from("wiki/zh/c.md"), "与本需求无关的内容".into()),
        ];
        let kws = vec!["安装".to_string(), "配置".to_string()];
        let ranked = search_pages(&pages, &kws, 2);
        assert_eq!(ranked.len(), 2, "仅命中页返回: {:?}", ranked);
        assert_eq!(ranked[0].0, "b", "命中 4 次（安装×2+配置×2）应排前");
        assert_eq!(ranked[1].0, "a", "命中 3 次排后");
        assert!(!ranked.iter().any(|(n, _)| n == "c"), "无命中页不返回");

        assert!(search_pages(&pages, &["不存在的关键词".to_string()], 2).is_empty(), "无命中返回空");
        assert!(search_pages(&pages, &[], 2).is_empty(), "空关键词返回空");
    }

    /// 方案甲：检索注入——含关键词正文的页面被检索出并拼入证据节
    /// （拼接格式与 measure_rubrics 完全一致：摘要证据后追加
    /// 「# 检索到的页面正文」节；tempdir 模式与既有测试一致）
    #[test]
    fn test_build_evidence_includes_retrieved_pages() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_bench_retr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wiki_zh = dir.join("wiki").join("zh");
        std::fs::create_dir_all(&wiki_zh).unwrap();
        std::fs::write(wiki_zh.join("a.md"), "# 模块 A\n\n与质量保障无关的说明。\n").unwrap();
        std::fs::write(
            wiki_zh.join("b.md"),
            "# 模块 B\n\n本项目通过认证 认证 双认证流程保证质量。\n",
        )
        .unwrap();

        let pages = collect_wiki_pages(&dir);
        let retrieved = search_pages(&pages, &extract_keywords("认证"), 2);
        assert_eq!(retrieved.len(), 1, "仅含「认证」正文的页被检索出: {:?}", retrieved);
        assert_eq!(retrieved[0].0, "b", "命中的应是 b 页");

        // 与 measure_rubrics 相同的拼接路径：摘要证据 + 检索节 + 整体 cap
        let mut evidence = "基线摘要".to_string();
        if !retrieved.is_empty() {
            evidence.push_str("\n\n# 检索到的页面正文\n");
            for (name, snippet) in &retrieved {
                evidence.push_str(&format!("- {name}: {snippet}\n"));
            }
            evidence = truncate(&evidence, 20_000);
        }
        assert!(evidence.contains("# 检索到的页面正文"), "证据应含检索节标题: {evidence}");
        assert!(evidence.contains("- b: "), "证据应含命中的 b 页: {evidence}");
        assert!(evidence.contains("认证"), "检索节应含关键词命中正文");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// t04：模块级判定指标——flip_rate（相对多数判定）与
    /// position_flip_rate（逐对 AB↔BA 交换翻转）手算核对
    #[test]
    fn test_module_judgment_metrics() {
        // 3 轮：判定序列 A胜, B胜, A胜, B胜, A胜, 平 → 众数 A 胜（3/6）
        let mixed = vec![
            (true, [10.0; 5], [5.0; 5]),
            (false, [4.0; 5], [8.0; 5]),
            (true, [9.0; 5], [6.0; 5]),
            (false, [5.0; 5], [7.0; 5]),
            (true, [8.0; 5], [7.0; 5]),
            (false, [6.0; 5], [6.0; 5]),
        ];
        let m = module_judgment_metrics(&mixed);
        // A 胜 3 次应为众数（多数判定），flip 相对该众数计算
        assert_eq!(majority_judgment(&[1, -1, 1, -1, 1, 0]), 1);
        assert_eq!(majority_judgment(&[1, -1]), 1, "并列按 A 胜优先");
        assert_eq!(majority_judgment(&[-1, -1, 1]), -1);
        // 与多数不一致：B 胜 ×2 + 平 ×1 = 3/6
        assert!((m.flip_rate - 0.5).abs() < 1e-9, "flip_rate 应为 0.5: {}", m.flip_rate);
        // 每轮 AB 与 BA 判定都不同：3/3
        assert!((m.position_flip_rate - 1.0).abs() < 1e-9, "position_flip_rate 应为 1.0: {}", m.position_flip_rate);

        // 全一致：无翻转
        let consistent = vec![
            (true, [10.0; 5], [5.0; 5]),
            (false, [9.0; 5], [6.0; 5]),
        ];
        let m2 = module_judgment_metrics(&consistent);
        assert!(m2.flip_rate.abs() < 1e-9);
        assert!(m2.position_flip_rate.abs() < 1e-9);

        // 空输入退化（不 panic）
        let m3 = module_judgment_metrics(&[]);
        assert_eq!(m3.flip_rate, 0.0);
        assert_eq!(m3.position_flip_rate, 0.0);
    }

    /// v32（6.1 FR-103）：模块级平局率——tie 独立类别（judgment()==0）
    /// 占比，供升级触发与报告统计共用
    #[test]
    fn test_module_tie_rate() {
        // 3 轮 6 次调用：A 胜×3、B 胜×1、平×2 → tie 率 2/6
        let mixed = vec![
            (true, [10.0; 5], [5.0; 5]),
            (false, [4.0; 5], [8.0; 5]),
            (true, [9.0; 5], [6.0; 5]),
            (false, [5.0; 5], [7.0; 5]),
            (true, [6.0; 5], [6.0; 5]),
            (false, [6.0; 5], [6.0; 5]),
        ];
        assert!((module_tie_rate(&mixed) - 2.0 / 6.0).abs() < 1e-9, "tie 率应为 1/3: {}", module_tie_rate(&mixed));
        // 无平局
        let no_tie = vec![(true, [10.0; 5], [5.0; 5]), (false, [4.0; 5], [8.0; 5])];
        assert_eq!(module_tie_rate(&no_tie), 0.0);
        // 全平局
        let all_tie = vec![(true, [6.0; 5], [6.0; 5]), (false, [6.0; 5], [6.0; 5])];
        assert_eq!(module_tie_rate(&all_tie), 1.0);
        // 空输入退化
        assert_eq!(module_tie_rate(&[]), 0.0);
        // 升级阈值判定：>0.30 触发
        assert!(module_tie_rate(&all_tie) > TQS_TIE_ESCALATION_THRESHOLD);
        assert!(module_tie_rate(&no_tie) < TQS_TIE_ESCALATION_THRESHOLD);
    }

    /// t04：标准 Cohen's κ——2×2 一致表公式手算 + 模块级 2×2 表累计
    #[test]
    fn test_kappa_cohen_formula_and_table() {
        // 完全一致：κ = 1.0
        assert!((kappa_cohen_from_table(&[[5, 0], [0, 5]]) - 1.0).abs() < 1e-9);
        // 完全不一致（AB 判 A 胜时 BA 恒判 B 胜）：负值保留（比随机更差）
        assert!(kappa_cohen_from_table(&[[0, 5], [5, 0]]) < 0.0);
        // 边际平衡：po = 2/3, pe = 0.5 → κ = 1/3
        let k = kappa_cohen_from_table(&[[10, 5], [5, 10]]);
        assert!((k - 1.0 / 3.0).abs() < 1e-6, "κ 应为 1/3: {k}");
        // 空表：0.0
        assert_eq!(kappa_cohen_from_table(&[[0; 2]; 2]), 0.0);

        // 模块级 2×2 累计：3 轮 AB 恒 A 胜、BA 恒 B 胜 → 表 [0][1]=15
        let rs = vec![
            (true, [10.0; 5], [5.0; 5]),
            (false, [4.0; 5], [8.0; 5]),
            (true, [9.0; 5], [6.0; 5]),
            (false, [5.0; 5], [7.0; 5]),
            (true, [8.0; 5], [7.0; 5]),
            (false, [6.0; 5], [6.0; 5]),
        ];
        let table = module_kappa_table(&rs);
        assert_eq!(table, [[0, 15], [0, 0]], "3 轮 × 5 维全落 [AB A 胜][BA B 胜]");
        // 平局（6.0 vs 6.0）按 B 胜计入（tie_handling 声明口径）；
        // 轮内 AB 与 BA 调用都是 A=6,B=6 → 判定相同，双计 B 胜
        let tie = vec![
            (true, [6.0; 5], [6.0; 5]),
            (false, [6.0; 5], [6.0; 5]),
        ];
        let table_tie = module_kappa_table(&tie);
        assert_eq!(table_tie, [[0, 0], [0, 5]], "平局双计 B 胜");
    }

    /// t04：多数投票——平票（含 abstain）无多数 → None（叶子 abstain）；
    /// abstain 票不影响已定多数
    #[test]
    fn test_majority_verdict_and_escalation() {
        assert_eq!(majority_verdict(&[Some(true), Some(true), Some(false)]), Some(true));
        assert_eq!(majority_verdict(&[Some(true), Some(false), Some(false)]), Some(false));
        assert_eq!(majority_verdict(&[Some(true), Some(false), None]), None, "1:1 平票无多数");
        assert_eq!(
            majority_verdict(&[Some(true), Some(false), Some(true), Some(false), None]),
            None,
            "2:2 平票无多数"
        );
        assert_eq!(majority_verdict(&[Some(true), Some(true), None]), Some(true), "abstain 不影响已定多数");
        assert_eq!(majority_verdict(&[Some(true), Some(true), Some(true)]), Some(true), "全票");
        assert_eq!(majority_verdict(&[None, None, None]), None, "全 abstain 无多数");

        // 升级判定：3 票时多数已定则停，否则升级 5 票
        assert!(verdict_resolved(&[Some(true), Some(true), Some(false)]), "2:1 已定案");
        assert!(!verdict_resolved(&[Some(true), Some(false), None]), "1:1+abstain 争议需升级");
        assert!(verdict_resolved(&[Some(true), Some(true), None]), "2:0+abstain 已定案");
    }

    /// t04：abstain 叶子从聚合中显式排除——不贡献权重/分数/叶子计数，
    /// 但叶子索引仍推进
    #[test]
    fn test_rubric_aggregate_excludes_abstain() {
        // 树：根(weight 1) → [a(2): 叶子, b(3): [c(1): 叶子, d(1): 叶子]]
        let node = RubricNode {
            requirement: "root".into(),
            weight: 1.0,
            sub_tasks: vec![
                RubricNode { requirement: "a".into(), weight: 2.0, sub_tasks: vec![] },
                RubricNode {
                    requirement: "b".into(),
                    weight: 3.0,
                    sub_tasks: vec![
                        RubricNode { requirement: "c".into(), weight: 1.0, sub_tasks: vec![] },
                        RubricNode { requirement: "d".into(), weight: 1.0, sub_tasks: vec![] },
                    ],
                },
            ],
        };
        // 判定 [true, abstain, true]：c 排除 → b = (0·1 + 1·1)/1 = 1.0
        // S = (2·1 + 3·1.0)/5 = 1.0，有效叶子 2，满足 2
        let verdicts = vec![Some(true), None, Some(true)];
        let mut idx = 0usize;
        let s = aggregate_score(&node, &verdicts, &mut idx);
        assert!((s.score - 1.0).abs() < 1e-9, "abstain 排除后总分应为 1.0: {}", s.score);
        assert_eq!(s.leaves, 2, "abstain 叶子不计数");
        assert_eq!(s.satisfied, 2);
        assert_eq!(idx, 3, "索引仍遍历全部叶子");
    }

    /// t04：协议参数保护（2606.13685 多数投票 n 取值）——基础轮数与
    /// 升级轮数锁死，防止后续改动静默退化
    #[test]
    fn test_repeat_protocol_constants() {
        assert_eq!(TQS_REPEATS, 5, "TQS 基础轮数 5（90%+ 保真性价比点）");
        assert_eq!(TQS_REPEATS_ESCALATED, 11, "低置信升级 11（95% 保真）");
        assert_eq!(RUBRIC_LEAF_REPEATS, 3, "叶子 3 次多数投票（约 90% 保真）");
        assert_eq!(RUBRIC_LEAF_REPEATS_ESCALATED, 5, "争议叶子升级 5 次");
    }

    /// t04：判定选项顺序的确定性伪随机——同一输入可复现，连续 3 次
    /// 调用覆盖两种选项顺序（2602.02219 n=2 平衡排列）
    #[test]
    fn test_option_variant_balanced_and_deterministic() {
        assert_eq!(
            option_variant("需要认证", 0),
            option_variant("需要认证", 0),
            "同一输入应可复现"
        );
        let variants: Vec<bool> = (0..3).map(|k| option_variant("需要认证", k)).collect();
        assert!(variants.contains(&true) && variants.contains(&false), "3 次调用应覆盖两种顺序: {variants:?}");
    }
}
