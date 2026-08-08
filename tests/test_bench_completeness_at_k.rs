//! v32 6.3（FR-104）Completeness@K 文档可检索性——集成验证
//! （覆盖 src/bench/mod.rs 的 measure_completeness_at_k 与 render_markdown）
//!
//! 单元测试区（src/bench/mod.rs #[cfg(test)]）已覆盖私有函数
//! measure_completeness_at_k 的四个基础用例；本文件从公共 API
//! （run_rubrics_only + render_markdown）端到端补验 v32 6.3 审查修复
//! 的边界场景与渲染契约：
//!
//! 1. 渲染「## 3. Completeness@K（文档可检索性）」两分支：
//!    - judged=true → 「命中实体数（top-10 …）」行
//!    - judged=false → 「未执行（text 索引缺失…）」显式标注（FR-101 不静默）
//!    - 节号 1..8 全序唯一（3 插入后 4-8 无冲突/缺号/重复）
//! 2. 命中判定（FR-104 验收）：实体名检索 text 索引 top-K 任一条目
//!    所属模块与实体模块相同（两侧均按 file_path 父目录派生）且模块页
//!    存在于产物 → 命中。
//!    - 目录级判定：索引条目 file_path 指向同模块目录下**另一个文件**
//!      （src/net/tcp2.rs 不存在实体文件）仍命中——若实现退化为文件级
//!      精确匹配此处为 0（v32 6.3 审查修复 ① 回归）
//!    - 反方向：索引条目属**不同模块**、模块页存在 → 不命中（防误报）
//!    - 同模块条目但**模块页缺失** → 不命中（可检索性判定）
//! 3. 降级判据（v32 6.3 审查修复 ② 回归）：.search 目录存在但
//!    text_index.db 缺失 → judged=false 显式降级（rusqlite 父目录存在
//!    时会自动建空索引，判据必须看文件存在而非目录/打开失败）
//!
//! 防触网：全部用例 LLM 用 Mock provider（不触网、无 key 依赖）；
//! 不配置 embed；索引为本地 SQLite 文件。环境中的真实 LLM key
//! （OPENCODEGO2_API_KEY/BAILIAN_API_KEY）不会被任何用例读取。

use std::path::{Path, PathBuf};

use repo_wiki::bench::{
    render_markdown, run_rubrics_only, BenchReport, CompletenessReport, CoverageReport,
    DocInfoReport, LintReport, TimeReport, UpdateRecallReport,
};
use repo_wiki::config::schema::{
    LlmProviderType, LlmSection, WikiConfig, WikiSection, SEARCH_INDEX_DIR,
};
use repo_wiki::model::{CodeNode, NodeId, NodeKind};
use repo_wiki::project::ProjectRoot;
use repo_wiki::search::text::TextEngine;

// ================= 工具 =================

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("repo_wiki_ck_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造被测仓库：dir/src/<rel> 写入源码，.repo-wiki/wiki/zh/<page>
/// 写入手工产物页（确定性内容，不依赖生成流水线），LLM 恒为 Mock。
/// 返回 (root, config)。
fn repo_with_pages(dir: &Path, source_rel: &str, source: &str, pages: &[(&str, &str)]) -> (ProjectRoot, WikiConfig) {
    let file = dir.join(source_rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, source).unwrap();
    let wiki_zh = dir.join(".repo-wiki").join("wiki").join("zh");
    std::fs::create_dir_all(&wiki_zh).unwrap();
    for (name, content) in pages {
        std::fs::write(wiki_zh.join(name), content).unwrap();
    }
    let config = WikiConfig {
        output_dir: Some(dir.join(".repo-wiki").to_string_lossy().into_owned().into()),
        wiki: WikiSection { language: "zh".into(), guide: Default::default() },
        llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
        ..Default::default()
    };
    std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();
    (ProjectRoot::new(dir.to_path_buf()), config)
}

/// 在 config.output_dir()/.search/text_index.db 建 text 索引并写入条目。
/// 条目 module_path 镜像 graph::build 的真实构造（父目录 + 文件 stem，
/// src/analysis/graph.rs:82-85）——生产形态下 module_path 含 stem，
/// 与实体侧父目录规则不一致，判定必须经 file_path 派生（v32 6.3 修复）。
fn write_index(config: &WikiConfig, node: CodeNode, content: &str) {
    let index_dir = config.output_dir().join(SEARCH_INDEX_DIR);
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = TextEngine::open(index_dir.join("text_index.db")).unwrap();
    engine.index_batch(&[(node, content.to_string())]).unwrap();
    // engine 先 drop（释放 SQLite 文件锁），再进入被测流程与清理
}

/// 全空报告骨架（render 分支测试用；completeness 由各用例自填）
fn base_report(completeness: CompletenessReport) -> BenchReport {
    BenchReport {
        repo_name: "demo".into(),
        generated_at: "2026-08-03T00:00:00Z".into(),
        coverage: CoverageReport { total_entities: 2, covered_entities: 2, ratio: 1.0 },
        doc_info: DocInfoReport {
            pages: 1,
            words: 10,
            cross_references: 0,
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
        completeness,
    }
}

/// 断言渲染输出的 8 个节号全序唯一（3 插入后 4-8 不得冲突/缺号/重复）
fn assert_section_order(md: &str) {
    let headers: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
    let expected = [
        "## 1. 实体覆盖率（Coverage）",
        "## 2. 文本统计（Doc Info）",
        "## 3. Completeness@K（文档可检索性）",
        "## 4. lint 健康",
        "## 5. 增量召回（Update Recall）",
        "## 6. 耗时（Time）",
        "## 7. TQS 文本质量（LLM 裁判，--judge）",
        "## 8. Rubric 层级完整性（LLM 裁判，--judge）",
    ];
    assert_eq!(
        headers, expected,
        "节号 1..8 应全序唯一（Completeness@K 插入后 4-8 无冲突）: {md}"
    );
}

// ================= 渲染分支 =================

/// FR-104/FR-101：judged=true 渲染「命中实体数（top-10 …）」行，
/// 不出现降级标注；节号 1..8 全序唯一
#[test]
fn test_render_completeness_judged_branch_and_section_order() {
    let md = render_markdown(&base_report(CompletenessReport {
        total_entities: 2,
        hit_entities: 1,
        k: 10,
        ratio: 0.5,
        judged: true,
    }));
    assert!(
        md.contains("## 3. Completeness@K（文档可检索性）"),
        "节 3 标题应存在: {md}"
    );
    assert!(
        md.contains("- 实体总数: 2\n- 命中实体数（top-10 检索命中所属模块页）: 1\n- 命中率: 0.50"),
        "judged 分支应输出命中明细（k=10、命中率 {:.2}）: {md}",
        0.5
    );
    assert!(
        !md.contains("未执行（text 索引缺失"),
        "judged 分支不得出现降级标注: {md}"
    );
    assert_section_order(&md);
}

/// FR-101：judged=false 渲染「未执行（text 索引缺失…）」显式标注，
/// 不得输出命中明细；节号 1..8 全序唯一
#[test]
fn test_render_completeness_degraded_branch_and_section_order() {
    let md = render_markdown(&base_report(CompletenessReport {
        total_entities: 2,
        hit_entities: 0,
        k: 10,
        ratio: 0.0,
        judged: false,
    }));
    assert!(
        md.contains("- 未执行（text 索引缺失——未生成或索引不可用，降级跳过）"),
        "降级分支应显式标注（FR-101 不静默）: {md}"
    );
    assert!(
        !md.contains("命中实体数（top-"),
        "降级分支不得输出命中明细: {md}"
    );
    assert_section_order(&md);
}

// ================= 命中判定（FR-104 验收） =================

/// 目录级命中：索引条目 file_path 指向同模块目录下**另一个文件**
/// （src/net/tcp2.rs 实体文件不存在，仅索引条目）→ 仍命中。
/// 若实现退化为文件级精确匹配此处为 0（v32 6.3 审查修复 ① 回归）。
/// 端到端：run_rubrics_only 公共路径 + 渲染 judged 分支。
#[test]
fn test_e2e_hit_same_module_directory_level() {
    let dir = temp_dir("hit_dir");
    let (root, config) = repo_with_pages(
        &dir,
        "src/net/tcp.rs",
        "pub fn tcp_fn(x: u32) -> u32 { x }\n",
        &[("src_net.md", "# 模块 src::net\n\nTCP 模块文档。\n")],
    );
    // 索引条目镜像 graph::build 真实构造：module_path = 父目录 + stem
    // （src/analysis/graph.rs:82-85）；判定按 file_path 派生模块，
    // 与 module_path 字段无关——夹具保持生产形态防测试/生产分叉
    write_index(
        &config,
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "tcp_fn".into(),
            file_path: Some("src/net/tcp2.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: Some("pub fn tcp_fn(x: u32) -> u32".into()),
            visibility: None,
            module_path: vec!["src".into(), "net".into(), "tcp2".into()],
        },
        "pub fn tcp_fn(x: u32) -> u32 { x }",
    );

    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let c = &report.completeness;
    assert!(c.judged, "索引存在应执行判定");
    assert_eq!(c.total_entities, 1, "应解析出 tcp_fn 一个实体");
    assert_eq!(
        c.hit_entities, 1,
        "目录级模块判定：索引条目文件与实体文件不同（tcp2.rs vs tcp.rs）仍命中；若实现退化为文件级精确匹配此处为 0"
    );
    assert_eq!(c.k, 10, "FR-104 固定 top-K=10");
    assert!((c.ratio - 1.0).abs() < 1e-9);

    let md = render_markdown(&report);
    assert!(
        md.contains("- 实体总数: 1\n- 命中实体数（top-10 检索命中所属模块页）: 1\n- 命中率: 1.00"),
        "端到端渲染 judged 分支应输出命中明细: {md}"
    );
    assert_section_order(&md);
    let _ = std::fs::remove_dir_all(root.path());
}

/// 反方向防误报：索引条目属**不同模块**（lib/x.rs），实体模块 src 的
/// 模块页存在 → 不命中（检索条目模块 != 实体模块）
#[test]
fn test_e2e_miss_cross_module_entry() {
    let dir = temp_dir("miss_xmod");
    let (root, config) = repo_with_pages(
        &dir,
        "src/a.rs",
        "pub fn alpha(x: u32) -> u32 { x }\n",
        &[("src.md", "# 模块 src\n\n文档。\n")],
    );
    write_index(
        &config,
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "alpha".into(),
            file_path: Some("lib/x.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: Some("pub fn alpha(x: u32) -> u32".into()),
            visibility: None,
            module_path: vec!["lib".into(), "x".into()],
        },
        "pub fn alpha(x: u32) -> u32 { x }",
    );

    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let c = &report.completeness;
    assert!(c.judged, "索引存在应执行判定");
    assert_eq!(c.total_entities, 1);
    assert_eq!(
        c.hit_entities, 0,
        "索引条目模块（lib）与实体模块（src）不同不得命中（防跨模块误报）"
    );
    assert!((c.ratio - 0.0).abs() < 1e-9);
    let _ = std::fs::remove_dir_all(root.path());
}

/// 同模块条目但模块页缺失 → 不命中（FR-104 可检索性判定：
/// 模块页必须存在于产物）
#[test]
fn test_e2e_miss_when_module_page_absent() {
    let dir = temp_dir("miss_page");
    let (root, config) = repo_with_pages(
        &dir,
        "src/net/tcp.rs",
        "pub fn tcp_fn(x: u32) -> u32 { x }\n",
        &[], // 无 src_net.md 产物页
    );
    write_index(
        &config,
        CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "tcp_fn".into(),
            file_path: Some("src/net/tcp.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: Some("pub fn tcp_fn(x: u32) -> u32".into()),
            visibility: None,
            module_path: vec!["src".into(), "net".into(), "tcp".into()],
        },
        "pub fn tcp_fn(x: u32) -> u32 { x }",
    );

    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let c = &report.completeness;
    assert!(c.judged, "索引存在仍执行判定");
    assert_eq!(c.total_entities, 1);
    assert_eq!(c.hit_entities, 0, "模块页缺失（src_net.md 不存在）不得命中");
    assert!((c.ratio - 0.0).abs() < 1e-9);
    let _ = std::fs::remove_dir_all(root.path());
}

// ================= 降级判据（修复 ② 回归） =================

/// .search 目录**存在**但 text_index.db 缺失 → judged=false 显式降级。
/// rusqlite 在父目录存在时会自动创建空索引，判据必须看文件存在
/// （v32 6.3 审查修复 ②：此前按打开失败判定，空索引会误报
/// judged=true 但恒 0 命中）。
#[test]
fn test_e2e_degrades_when_index_file_missing_dir_exists() {
    let dir = temp_dir("deg_dir");
    let (root, config) = repo_with_pages(
        &dir,
        "src/a.rs",
        "pub fn alpha(x: u32) -> u32 { x }\n",
        &[("src.md", "# 模块 src\n\n文档。\n")],
    );
    // 只建 .search 目录，不建 text_index.db
    std::fs::create_dir_all(config.output_dir().join(SEARCH_INDEX_DIR)).unwrap();

    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let c = &report.completeness;
    assert!(!c.judged, "索引文件缺失应降级跳过（judged=false），即使 .search 目录存在");
    assert_eq!(c.total_entities, 1, "实体统计仍给出（与 coverage 同源）");
    assert_eq!(c.hit_entities, 0);
    assert_eq!(c.ratio, 0.0, "降级时不虚报命中率");

    let md = render_markdown(&report);
    assert!(
        md.contains("- 未执行（text 索引缺失——未生成或索引不可用，降级跳过）"),
        "降级渲染应显式标注: {md}"
    );
    assert!(!md.contains("命中实体数（top-"), "降级分支不得输出命中明细: {md}");
    assert_section_order(&md);
    let _ = std::fs::remove_dir_all(root.path());
}

/// .search 目录与索引文件都不存在 → judged=false（最朴素的缺失路径）
#[test]
fn test_e2e_degrades_when_no_index_dir() {
    let dir = temp_dir("deg_none");
    let (root, config) = repo_with_pages(
        &dir,
        "src/a.rs",
        "pub fn alpha(x: u32) -> u32 { x }\n",
        &[("src.md", "# 模块 src\n\n文档。\n")],
    );
    // 不建任何索引相关目录

    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let c = &report.completeness;
    assert!(!c.judged, "索引缺失应降级跳过");
    assert_eq!(c.ratio, 0.0, "降级时不虚报命中率");
    let _ = std::fs::remove_dir_all(root.path());
}
