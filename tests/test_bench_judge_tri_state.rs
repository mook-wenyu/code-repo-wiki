//! v32 6.1「judge 三态协议增强」集成验证（覆盖 src/bench/mod.rs）
//!
//! bench 单元测试区无法注入 LLM 响应序列（create_provider 对 Mock
//! 返回固定响应），本文件用本地 mock OpenAI server（Chat 协议 SSE
//! 流式）走真实 OpenAiProvider 生产路径，端到端验证：
//!
//! 1. rubric 叶子判定 uncertain 重试协议（FR-102）：
//!    - 首次 uncertain 重试一次且不消耗判定票（votes 不推进）；
//!    - 重试后仍 uncertain → abstain（None，不计入覆盖率分母）；
//!    - 重试后 satisfied → 正常恢复多数投票定案。
//! 2. TQS tie 率升级阈值（FR-103）：
//!    - 阈值之下（tie 0.2 / flip 0.2）不升级 → repeats=5；
//!    - 超过阈值（tie 0.6）升级复测轮数 → repeats=11。
//!
//! 已知限制（测试报告发现项）：
//! - tie 率恰好 0.30 的严格边界在集成层不可独立观察——单模块 10 次
//!   调用中 3 次平局必然使 flip_rate ≥ 0.30 > 0.20（三态判定计数
//!   约束：非平局全同向时 flip 恰好等于 tie 数），升级 OR 条件的
//!   flip 分支会抢先触发；该边界只能由单元测试 module_tie_rate 覆盖。
//! - uncertain 重试「更换选项顺序」存在实现缺陷（重试轮与首轮 prompt
//!   相同），见 test_rubric_uncertain_retry_swaps_variant（#[ignore]）。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use repo_wiki::bench::run_rubrics_only;
use repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiSection, WikiConfig};
use repo_wiki::project::ProjectRoot;

// ================= 本地 mock OpenAI server（Chat 协议 SSE 流式） =================
// 与 src/generate/llm.rs 测试区同模式：std TcpListener + 线程，
// 响应带 Connection: close 迫使 reqwest 每次请求新建连接。

/// mock 服务器响应
struct MockResponse {
    status: u16,
    body: String,
}

fn header_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

/// 读取一个完整 HTTP 请求，返回请求体（响应分发只依赖 body 中的
/// prompt 内容；path/headers 解析仅用于 content-length 边界）
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

/// 把 LLM 文本包装成 OpenAI SSE 流式响应（content 内嵌 JSON 需转义）
fn sse_response(content: &str) -> String {
    let encoded = serde_json::to_string(content).unwrap();
    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{encoded}}}}}]}}\n\ndata: [DONE]\n\n")
}

// ================= 响应构造 =================

/// rubric 树（1 个叶子）：生成×3 与合并×1 共用同一棵树
fn rubric_tree() -> String {
    r#"{"rubrics": [{"requirement": "文档应描述认证流程", "weight": 1}]}"#.into()
}

/// 叶子判定三态响应
fn verdict(v: &str) -> String {
    format!(r#"{{"verdict": "{v}"}}"#)
}

/// TQS 五维分数响应（A/B 各 5 维 0-10）
fn tqs_score(a: [f64; 5], b: [f64; 5]) -> String {
    let dims = ["clarity", "readability", "conciseness", "richness", "structure"];
    let doc = |s: [f64; 5]| {
        let mut m = serde_json::Map::new();
        for (d, v) in dims.iter().zip(s.iter()) {
            m.insert(d.to_string(), serde_json::Value::from(*v));
        }
        serde_json::Value::Object(m)
    };
    serde_json::json!({ "A": doc(a), "B": doc(b) }).to_string()
}

/// 启动脚本化 server：按请求顺序消费响应队列（空队列 → 500），
/// 统计 rubric 叶子判定调用次数并记录每次判定调用的 system prompt。
/// 返回 (判定调用计数, prompts, base_url)。
fn spawn_scripted(responses: Vec<String>) -> (Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>, String) {
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let jc = judge_calls.clone();
    let pr = prompts.clone();
    let base_url = spawn_mock_server(move |body| {
        // 叶子判定请求的 system prompt 含「文档质量裁判」（区分于
        // 生成/合并请求，后两者不计数）
        if body.contains("文档质量裁判") {
            jc.fetch_add(1, Ordering::Relaxed);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(sys) = v["messages"][0]["content"].as_str()
            {
                pr.lock().unwrap().push(sys.to_string());
            }
        }
        let next = queue.lock().unwrap().pop_front();
        match next {
            Some(content) => MockResponse { status: 200, body: sse_response(&content) },
            None => MockResponse { status: 500, body: "preset exhausted".into() },
        }
    });
    (judge_calls, prompts, base_url)
}

// ================= 临时仓库 =================

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("repo_wiki_judge_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造被测仓库：
/// - `with_readme`：README.md 存在 → measure_rubrics 执行（docs 非空）；
///   缺失 → rubric 跳过，server 只服务 TQS 请求。
/// - `with_tqs`：导出快照 + 产物页存在 → measure_tqs 执行；缺失 →
///   TQS 跳过（snapshot 早退），server 只服务 rubric 请求。
/// - 产物与快照均手工构造（确定性内容，不依赖生成流水线）。
fn bench_setup(tag: &str, base_url: &str, with_readme: bool, with_tqs: bool) -> (ProjectRoot, WikiConfig) {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha(x: u32) -> u32 { x + 1 }\n").unwrap();
    if with_readme {
        std::fs::write(dir.join("README.md"), "# 示例仓库\n\n需要认证与授权。\n").unwrap();
    }
    if with_tqs {
        let wiki_zh = dir.join(".repo-wiki").join("wiki").join("zh");
        std::fs::create_dir_all(&wiki_zh).unwrap();
        std::fs::write(wiki_zh.join("mod_a.md"), "# 模块 mod_a\n\n新文档内容。\n").unwrap();
        let state = dir.join(".repo-wiki").join(".state");
        std::fs::create_dir_all(&state).unwrap();
        let snapshot = serde_json::json!({
            "version": 1,
            "documents": [{
                "title": "mod_a",
                "kind": "WikiPage",
                "content": "旧文档内容。",
                "language": "zh",
                "module_path": [],
                "references": [],
                "last_updated": "2026-01-01T00:00:00Z",
                "fingerprint": null
            }],
            "cards": [],
            "modules": []
        });
        std::fs::write(state.join("export_snapshot.json"), snapshot.to_string()).unwrap();
    } else {
        // 空产物目录（lint 对空目录无检查项，比缺失目录更稳妥）
        std::fs::create_dir_all(dir.join(".repo-wiki").join("wiki").join("zh")).unwrap();
    }
    let config = WikiConfig {
        output_dir: Some(dir.join(".repo-wiki").to_string_lossy().into_owned().into()),
        wiki: WikiSection { language: "zh".into() },
        llm: LlmSection {
            provider: LlmProviderType::OpenAiCompatible,
            model: "mock-model".into(),
            base_url: Some(format!("{base_url}/v1")),
            api_key: Some("test-key".into()),
            api_key_env: "NONE".into(),
        },
        ..Default::default()
    };
    std::fs::write(dir.join("config.toml"), toml::to_string_pretty(&config).unwrap()).unwrap();
    let root = ProjectRoot::new(dir);
    (root, config)
}

// ================= 测试 =================

/// FR-102：叶子判定全 uncertain —— 首次 uncertain 重试一次（不消耗票），
/// 重试后仍 uncertain 记 abstain（None，不计入覆盖率分母）。
///
/// 判定调用数 = 1 次重试 + 5 票 abstain = 6：
/// 若无重试路径（首 uncertain 直接 abstain）则仅 5 次——计数断言区分。
#[test]
fn test_rubric_uncertain_retry_abstains_e2e() {
    let responses: Vec<String> = vec![
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        verdict("uncertain"),
        verdict("uncertain"),
        verdict("uncertain"),
        verdict("uncertain"),
        verdict("uncertain"),
        verdict("uncertain"),
    ];
    let (judge_calls, _, base_url) = spawn_scripted(responses);
    let (root, config) = bench_setup("rub_abstain", &base_url, true, false);
    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let rubric = report.rubric.expect("rubric 应执行（README + LLM 可用）");
    assert_eq!(rubric.leaf_count, 1, "1 个叶子");
    assert_eq!(rubric.abstain_leaves, 1, "重试后仍 uncertain 应记 abstain");
    assert_eq!(rubric.satisfied_leaves, 0);
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        6,
        "1 次 uncertain 重试 + 5 票 abstain = 6 次判定调用（无重试实现为 5）"
    );
    let _ = std::fs::remove_dir_all(root.path());
}

/// FR-102：uncertain 重试后 satisfied —— 重试不消耗判定票，3 票多数
/// 正常定案（1 重试 + 3 票 = 4 次调用；若首 uncertain 直接 abstain
/// 则仅 3 次——计数断言区分）。
#[test]
fn test_rubric_uncertain_retry_recovers_e2e() {
    let responses: Vec<String> = vec![
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        verdict("uncertain"),
        verdict("satisfied"),
        verdict("satisfied"),
        verdict("satisfied"),
    ];
    let (judge_calls, _, base_url) = spawn_scripted(responses);
    let (root, config) = bench_setup("rub_recover", &base_url, true, false);
    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let rubric = report.rubric.expect("rubric 应执行（README + LLM 可用）");
    assert_eq!(rubric.satisfied_leaves, 1, "重试后 3 票 satisfied 应判满足");
    assert_eq!(rubric.abstain_leaves, 0);
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        4,
        "1 次 uncertain 重试 + 3 票 = 4 次判定调用（重试不消耗票）"
    );
    let _ = std::fs::remove_dir_all(root.path());
}

/// 已知缺陷（v32 6.1）：uncertain 重试未更换选项顺序。
///
/// option_variant 的 call_idx 取 votes.len()，而 uncertain 分支 continue
/// 不推进 votes——重试轮与首轮的 satisfied/unsatisfied 选项顺序相同，
/// 与 judge_leaf 注释「下轮换 variant 重试」及任务验证要点不符
/// （FR-102 契约语义「重试一次、仍不确定记 abstain」不受影响）。
/// 修复（重试轮使用推进的调用计数）后移除此 #[ignore]。
#[ignore = "已知缺陷：uncertain 重试未更换选项顺序（call_idx=votes.len() 在 continue 时不推进）"]
#[test]
fn test_rubric_uncertain_retry_swaps_variant() {
    let responses: Vec<String> = vec![
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        rubric_tree(),
        verdict("uncertain"),
        verdict("satisfied"),
        verdict("satisfied"),
        verdict("satisfied"),
    ];
    let (_, prompts, base_url) = spawn_scripted(responses);
    let (root, config) = bench_setup("rub_variant", &base_url, true, false);
    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    report.rubric.expect("rubric 应执行（README + LLM 可用）");
    let ps = prompts.lock().unwrap();
    assert!(ps.len() >= 2, "至少应有两次判定调用（首轮 + 重试轮）: {}", ps.len());
    assert_ne!(
        ps[0], ps[1],
        "uncertain 重试应更换选项顺序（satisfied/unsatisfied 对调）；当前实现重试轮 prompt 与首轮相同"
    );
    let _ = std::fs::remove_dir_all(root.path());
}

/// FR-103：tie 率低于阈值（0.2）+ flip 恰为 0.2（严格 > 不触发）→
/// 不升级复测轮数（repeats = 5）。对照基线。
#[test]
fn test_tqs_no_escalation_below_thresholds_e2e() {
    let a_win = tqs_score([8.0; 5], [7.0; 5]);
    let tie = tqs_score([7.0; 5], [7.0; 5]);
    // v32 6.2 起 run_rubrics_only 在 TQS 前执行 Doc Info LLM 判定
    // （measure_doc_info_llm 逐页一次调用），先消费队列首个响应；
    // 其后 10 个 = TQS 基础 5 轮 × AB/BA 两次。
    let mut responses = vec!["{\"score\": 8}".to_string()];
    responses.extend(vec![a_win.clone(); 8]);
    responses.extend(vec![tie; 2]);
    let (_, _, base_url) = spawn_scripted(responses);
    let (root, config) = bench_setup("tqs_base", &base_url, false, true);
    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let tqs = report.tqs.expect("TQS 应执行（snapshot + 产物页 + LLM 可用）");
    assert_eq!(tqs.judged_modules, 1);
    assert_eq!(tqs.repeats, 5, "tie 0.2/flip 0.2 均不超阈值，不应升级");
    assert!((tqs.tie_rate - 0.2).abs() < 1e-9, "tie_rate 应为 0.2: {}", tqs.tie_rate);
    let _ = std::fs::remove_dir_all(root.path());
}

/// FR-103：tie 率超过阈值（前 10 次调用 6 平 4 胜 = 0.6 > 0.3）→
/// 升级复测轮数至 11 轮（22 次调用），repeats = 11。
#[test]
fn test_tqs_escalates_on_high_tie_rate_e2e() {
    let a_win = tqs_score([8.0; 5], [7.0; 5]);
    let tie = tqs_score([7.0; 5], [7.0; 5]);
    // 前 10 次：6 平（i<6）+ 4 A 胜（6≤i<10）→ tie 0.6 触发升级；
    // 升级后补 12 次全 A 胜 → 共 22 次调用（11 轮）
    let mut responses = Vec::new();
    // v32 6.2：Doc Info LLM 判定先消费队列首个响应（见
    // test_tqs_no_escalation_below_thresholds_e2e 注释）；
    // 其后 22 个 = TQS 升级后 11 轮 × AB/BA 两次。
    responses.push("{\"score\": 8}".to_string());
    for i in 0..22 {
        responses.push(if i < 6 { tie.clone() } else { a_win.clone() });
    }
    let (_, _, base_url) = spawn_scripted(responses);
    let (root, config) = bench_setup("tqs_esc", &base_url, false, true);
    let report = run_rubrics_only(&root, &config, "demo").unwrap();
    let tqs = report.tqs.expect("TQS 应执行（snapshot + 产物页 + LLM 可用）");
    assert_eq!(tqs.judged_modules, 1);
    assert_eq!(
        tqs.repeats, 11,
        "tie 率 0.6 > 0.3 应升级复测轮数至 11（22 次调用）"
    );
    let _ = std::fs::remove_dir_all(root.path());
}
