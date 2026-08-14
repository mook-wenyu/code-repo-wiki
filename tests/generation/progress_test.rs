//! run_pipeline_with_progress 进度事件测试
//!
//! 将 fixtures/sample-repo 复制到唯一临时目录（避免污染 fixture 目录，
//! 也避免与其他测试并发写 fixture 的 mock-server.toml 冲突），LLM 指向本地
//! mock server（返回固定 JSON 响应，生成调用成功且零重试延迟），验证回调事件序列。
//!
//! root 显式注入：以 work_dir 为 ProjectRoot 传入流水线，不再依赖进程 cwd，
//! 无需 CWD 全局互斥锁，可与其余测试并行。

use std::path::Path;
use std::sync::Mutex;

use crate::common::{copy_dir, openai_compatible_config, unique_dir};

#[test]
fn test_pipeline_progress_events_monotonic_and_done() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir("progress_repo");
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);

    // root 显式注入：流水线以 work_dir 为项目根（扫描根 + git 定位），不再依赖 cwd
    let root = code_repo_wiki::project::ProjectRoot::new(work_dir.clone());

    // 本地 mock LLM server：返回固定 JSON 响应，让生成调用成功且零重试延迟
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = br#"{"choices":[{"message":{"content":"mock"}}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
        }
    });

    // v19 t04：基于 common helper（dir 绝对路径，杜绝 cwd 泄漏）；
    // 本测试内联自定义 mock server（非 SSE 形态），port 取自该 server
    let config = openai_compatible_config(port);
    std::fs::write(work_dir.join("mock-server.toml"), config).unwrap();

    let events: Mutex<Vec<code_repo_wiki::ProgressEvent>> = Mutex::new(Vec::new());
    let result = code_repo_wiki::run_pipeline_with_progress(
        Some(&work_dir.join("mock-server.toml")),
        None,
        true,
        &root,
        &code_repo_wiki::GenerationMode::Full,
        code_repo_wiki::LockOptions::default(),
        &|evt| events.lock().unwrap().push(evt),
    );
    assert!(
        result.is_ok(),
        "流水线应成功（LLM 失败被容错跳过）: {:?}",
        result.err()
    );

    let events = events.into_inner().unwrap();
    // 事件序列非空且以 scanning 开始
    assert!(!events.is_empty(), "应收到进度事件");
    assert_eq!(events.first().unwrap().stage, "scanning");
    // 百分比单调递增
    for w in events.windows(2) {
        assert!(
            w[1].percent >= w[0].percent,
            "事件百分比应单调递增: {} ({}%) -> {} ({}%)",
            w[0].stage,
            w[0].percent,
            w[1].stage,
            w[1].percent
        );
    }
    // 以 done=100 结束
    assert_eq!(events.last().unwrap().stage, "done");
    assert_eq!(events.last().unwrap().percent, 100);

    // v48：analyzing 阶段 25→27→30 单调补点（大仓图构建长黑屏期可见推进）
    assert!(
        events
            .iter()
            .any(|e| e.stage == "analyzing" && e.percent == 27),
        "应包含 analyzing 27% 进度事件（25→27→30 单调补点）"
    );

    // v46：LLM 逐项进度——cards 阶段事件带 current/total 且递增到总数
    //（mock 响应非卡片 JSON，生成全失败但失败隔离下任务仍计数——事件照发）
    let card_evts: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "cards" && e.current.is_some())
        .collect();
    assert!(!card_evts.is_empty(), "cards 阶段应有逐项进度事件");
    let last_card = card_evts.last().unwrap();
    assert_eq!(
        last_card.current, last_card.total,
        "最后一项 current 应等于 total: {:?}",
        last_card
    );
    for w in card_evts.windows(2) {
        assert!(
            w[1].current.unwrap() >= w[0].current.unwrap(),
            "current 应单调递增: {:?} -> {:?}",
            w[0],
            w[1]
        );
    }
    // wiki 阶段同构（页面失败跳过仍计数）
    let wiki_evts: Vec<_> = events
        .iter()
        .filter(|e| e.stage == "wiki" && e.current.is_some())
        .collect();
    assert!(!wiki_evts.is_empty(), "wiki 阶段应有逐项进度事件");
    assert_eq!(
        wiki_evts.last().unwrap().current,
        wiki_evts.last().unwrap().total,
        "wiki 最后一项 current 应等于 total"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
