//! v32 6.2「DocInformationReport（LLM 判定维度）」集成验证
//! （覆盖 src/bench/mod.rs 与 src/bench/manifest.rs 的公共行为）
//!
//! 单元测试区（src/bench/mod.rs 的 #[cfg(test)]）已覆盖私有函数
//! parse_doc_info_score 的三态边界与 measure_doc_info_llm 的 mock
//! 行为；本文件从公共 API 端到端验证集成点：
//!
//! 1. render_markdown 的 LLM 判定两分支（FR-101 不静默）：
//!    - llm_judged=true → 输出「LLM 信息性评分」行（评分/判定页/abstain 数）
//!    - llm_judged=false → 输出「未执行（LLM 不可用，降级跳过）」显式标注
//! 2. run_rubrics_only 调用点合并（FR-101 并存）：
//!    - Mock provider（LLM 可用但响应不可解析）→ llm_judged=true、
//!      全部页面计 abstain、评分 0.0（空集约定不除零）
//!    - OpenAI provider 无 key（LLM 不可用）→ llm_judged=false 降级
//! 3. run_manifest 错误行的 empty_doc_info 契约（llm_judged=false 全零）

use std::path::PathBuf;

use code_repo_wiki::bench::manifest::{run_manifest, RepoEntry};
use code_repo_wiki::bench::{
    render_markdown, run_rubrics_only, BenchReport, CompletenessReport, CoverageReport,
    DocInfoReport, LintReport, TimeReport, UpdateRecallReport,
};
use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiSection, WikiConfig};
use code_repo_wiki::project::ProjectRoot;

// ============ 本地 mock OpenAI server（Chat 协议 SSE 流式） ============
// 与 src/generate/llm.rs 及 tests/test_bench_judge_tri_state.rs 同模式：
// std TcpListener + 线程；响应带 Connection: close 迫使 reqwest 新建连接。
// 用于脚本化 measure_doc_info_llm 的逐页判定响应（单元测试区无法注入
// LLM 响应序列——create_provider 对 Mock 返回固定文本）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct MockResponse {
    status: u16,
    body: String,
}

fn header_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    while !header_complete(&buf) {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let headers: Vec<(String, String)> = head
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    const HEADER_SEP: usize = 4;
    while buf.len() < head_end + HEADER_SEP + content_length {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    String::from_utf8_lossy(&buf[head_end + HEADER_SEP..head_end + HEADER_SEP + content_length])
        .to_string()
}

fn spawn_mock_server(
    handler: impl Fn(String) -> MockResponse + Send + Sync + 'static,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let body = read_request(&mut stream);
                let resp = handler(body);
                let reason = if resp.status == 200 { "OK" } else { "Error" };
                let raw = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp.status, reason, resp.body.len(), resp.body
                );
                let _ = stream.write_all(raw.as_bytes());
            });
        }
    });
    base_url
}

fn sse_response(content: &str) -> String {
    let encoded = serde_json::to_string(content).unwrap();
    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{encoded}}}}}]}}\n\ndata: [DONE]\n\n")
}

/// 临时目录（复用既有集成测试的命名与清理模式）
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_docinfo_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造被测仓库：src/a.rs（coverage 实体）+ 手工产物页 wiki/zh/a.md
/// （确定性内容，不依赖生成流水线；TQS/Rubric 无快照/README 自动跳过）
fn bench_setup(tag: &str, llm: LlmSection) -> (ProjectRoot, WikiConfig) {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha(x: u32) -> u32 { x + 1 }\n").unwrap();
    let wiki_zh = dir.join(".code-repo-wiki").join("wiki").join("zh");
    std::fs::create_dir_all(&wiki_zh).unwrap();
    std::fs::write(wiki_zh.join("a.md"), "# 模块 a\n\n文档内容。\n").unwrap();
    let config = WikiConfig {
        output_dir: Some(dir.join(".code-repo-wiki").to_string_lossy().into_owned().into()),
        wiki: WikiSection { language: "zh".into(), guide: Default::default() },
        llm,
        ..Default::default()
    };
    std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();
    (ProjectRoot::new(dir), config)
}

/// FR-101：渲染两分支——llm_judged=true 输出评分行（含判定/abstain 数），
/// false 输出「未执行」显式标注；两分支互斥（不静默降级）
#[test]
fn test_render_markdown_doc_info_llm_branches() {
    let base = |llm_judged: bool, llm_score: f64, judged: usize, abstain: usize| BenchReport {
        repo_name: "demo".into(),
        generated_at: "2026-08-03T00:00:00Z".into(),
        coverage: CoverageReport { total_entities: 0, covered_entities: 0, ratio: 1.0 },
        doc_info: DocInfoReport {
            pages: 2,
            words: 10,
            cross_references: 0,
            code_blocks: 0,
            diagrams: 0,
            llm_judged,
            llm_score,
            llm_judged_modules: judged,
            llm_abstain_modules: abstain,
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
            total_entities: 0,
            hit_entities: 0,
            k: 10,
            ratio: 1.0,
            judged: false,
        },
    };

    let md_on = render_markdown(&base(true, 7.5, 3, 1));
    assert!(
        md_on.contains("LLM 信息性评分（0-10）: 7.50（判定 3 页，abstain 1 页）"),
        "执行分支应输出评分行（{:.2} 格式）: {md_on}",
        7.5
    );
    assert!(
        !md_on.contains("LLM 信息性判定: 未执行"),
        "执行分支不得出现 Doc Info 降级标注: {md_on}"
    );

    let md_off = render_markdown(&base(false, 0.0, 0, 0));
    assert!(
        md_off.contains("- LLM 信息性判定: 未执行（LLM 不可用，降级跳过）"),
        "降级分支应显式标注（FR-101 不静默）: {md_off}"
    );
    assert!(!md_off.contains("LLM 信息性评分"), "降级分支不得输出评分行: {md_off}");
}

/// FR-101/FR-102：调用点合并——run_rubrics_only 的 doc_info 携带 LLM
/// 判定结果。Mock provider（LLM 可用）下产物页解析失败全部计 abstain：
/// llm_judged=true、judged_modules=0、abstain_modules=页面数、评分 0.0
/// （judged_n==0 空集约定，无除零），渲染输出评分行而非降级标注。
#[test]
fn test_run_rubrics_only_doc_info_llm_abstains_with_mock() {
    let llm = LlmSection { provider: LlmProviderType::Mock, ..Default::default() };
    let (root, config) = bench_setup("mock_abstain", llm);
    let report = run_rubrics_only(&root, &config, "demo", &[]).unwrap();
    let doc_info = &report.doc_info;
    assert!(doc_info.llm_judged, "mock provider 可用，判定应执行（judged=true）");
    assert_eq!(doc_info.llm_judged_modules, 0, "mock 响应不含 score → 无成功判定页");
    assert_eq!(doc_info.llm_abstain_modules, 1, "1 个产物页全部计 abstain");
    assert_eq!(doc_info.llm_score, 0.0, "无判定页时评分为 0（空集约定）");
    // 渲染端到端：评分行以 0.00/0/1 呈现，不出现降级标注
    let md = render_markdown(&report);
    assert!(
        md.contains("LLM 信息性评分（0-10）: 0.00（判定 0 页，abstain 1 页）"),
        "渲染应输出执行分支评分行: {md}"
    );
    let _ = std::fs::remove_dir_all(root.path());
}

/// FR-101：LLM 不可用降级——OpenAI provider 无 key（env 指向不存在
/// 变量）→ create_provider 失败 → llm_judged=false 全零字段，
/// 渲染显式标注「未执行」。
#[test]
fn test_run_rubrics_only_doc_info_degraded_without_llm() {
    let llm = LlmSection {
        provider: LlmProviderType::OpenAI,
        api_key: None,
        api_key_env: "RW_DOCINFO_TEST_UNSET_ENV_9F3A".into(),
        ..Default::default()
    };
    let (root, config) = bench_setup("degraded", llm);
    let report = run_rubrics_only(&root, &config, "demo", &[]).unwrap();
    let doc_info = &report.doc_info;
    assert!(!doc_info.llm_judged, "LLM 不可用应降级 judged=false");
    assert_eq!(doc_info.llm_score, 0.0);
    assert_eq!(doc_info.llm_judged_modules, 0);
    assert_eq!(doc_info.llm_abstain_modules, 0, "降级不调用 LLM，无 abstain");
    let md = render_markdown(&report);
    assert!(
        md.contains("- LLM 信息性判定: 未执行（LLM 不可用，降级跳过）"),
        "降级渲染应显式标注: {md}"
    );
    let _ = std::fs::remove_dir_all(root.path());
}

/// v32 6.2：manifest 错误行的 empty_doc_info 契约——本地路径不存在时
/// 该仓库行 doc_info 为全零且 llm_judged=false（serde 兼容，矩阵渲染
/// 不受新字段影响）
#[test]
fn test_run_manifest_error_row_doc_info_empty() {
    let base = temp_dir("manifest_empty");
    let missing = base.join("missing-repo");
    let work_dir = base.join("work");
    let template = WikiConfig {
        llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
        ..Default::default()
    };
    let entries = vec![RepoEntry {
        name: "missing".into(),
        url: None,
        local: Some(missing),
        commit: None,
    }];
    let report = run_manifest(&entries, &template, &work_dir).unwrap();
    assert_eq!(report.repos.len(), 1);
    let row = &report.repos[0];
    assert!(row.error.is_some(), "缺失路径应标注失败");
    assert!(!row.doc_info.llm_judged, "错误行 doc_info 应为 empty_doc_info（llm_judged=false）");
    assert_eq!(row.doc_info.llm_score, 0.0);
    assert_eq!(row.doc_info.llm_judged_modules, 0);
    assert_eq!(row.doc_info.llm_abstain_modules, 0);
    let _ = std::fs::remove_dir_all(&base);
}

/// 脚本化仓库：3 个产物页（a/b/c）+ OpenAI 兼容 provider 指向本地
/// mock server。无 README（rubric 跳过）无快照（TQS 跳过）→
/// server 只服务 measure_doc_info_llm 的逐页判定请求。
fn bench_setup_scripted(tag: &str, base_url: &str) -> (ProjectRoot, WikiConfig) {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha() {}\n").unwrap();
    let wiki_zh = dir.join(".code-repo-wiki").join("wiki").join("zh");
    std::fs::create_dir_all(&wiki_zh).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(wiki_zh.join(format!("{name}.md")), format!("# 模块 {name}\n\n内容。\n")).unwrap();
    }
    let config = WikiConfig {
        output_dir: Some(dir.join(".code-repo-wiki").to_string_lossy().into_owned().into()),
        wiki: WikiSection { language: "zh".into(), guide: Default::default() },
        llm: LlmSection {
            provider: LlmProviderType::OpenAiCompatible,
            model: "mock-model".into(),
            base_url: Some(format!("{base_url}/v1")),
            api_key: Some("test-key".into()),
            api_key_env: "NONE".into(),
            thinking: None,
            reasoning_effort: None,
            max_concurrency: None,
        },
        ..Default::default()
    };
    std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();
    (ProjectRoot::new(dir), config)
}

/// FR-101/FR-102 端到端：脚本化逐页判定（按 user message 中的模块名
/// 路由，避免 read_dir 顺序不确定性）验证：
/// - uncertain 重试一次（a 页两次调用），仍 uncertain → abstain；
/// - score 优先于 verdict（b 页 {"score": 11, "verdict": "uncertain"}
///   → 评分且 clamp 到 10）；
/// - 越界下界 clamp（c 页 {"score": -2} → 0）；
/// - 评分 = 已判页平均，abstain 页不计入分母（(10+0)/2 = 5.0）；
/// - 判定调用总数 = 4（a 重试 2 + b/c 各 1）——无重试实现为 3，计数区分。
#[test]
fn test_doc_info_llm_retry_abstain_average_e2e() {
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let jc = judge_calls.clone();
    let base_url = spawn_mock_server(move |body| {
        if body.contains("信息性裁判") {
            jc.fetch_add(1, Ordering::Relaxed);
        }
        // 按 user message 的「模块：X」路由（file_stem 作模块名）
        let content = if body.contains("模块：a") {
            r#"{"verdict": "uncertain"}"#
        } else if body.contains("模块：b") {
            r#"{"score": 11, "verdict": "uncertain"}"#
        } else {
            r#"{"score": -2}"#
        };
        MockResponse { status: 200, body: sse_response(content) }
    });
    let (root, config) = bench_setup_scripted("e2e_retry", &base_url);
    let report = run_rubrics_only(&root, &config, "demo", &[]).unwrap();
    let d = &report.doc_info;
    assert!(d.llm_judged, "provider 可用应执行判定");
    assert_eq!(d.llm_judged_modules, 2, "b/c 两页判定成功（a 页 abstain）");
    assert_eq!(d.llm_abstain_modules, 1, "a 页重试后仍 uncertain 计 abstain");
    assert!(
        (d.llm_score - 5.0).abs() < 1e-9,
        "评分 = (clamp(11)→10 + clamp(-2)→0)/2 = 5.0，实际: {}",
        d.llm_score
    );
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        4,
        "a 页 1 次 uncertain 重试 + b/c 各 1 = 4 次判定调用（无重试实现为 3）"
    );
    let md = render_markdown(&report);
    assert!(
        md.contains("LLM 信息性评分（0-10）: 5.00（判定 2 页，abstain 1 页）"),
        "渲染应输出执行分支评分行: {md}"
    );
    let _ = std::fs::remove_dir_all(root.path());
}
