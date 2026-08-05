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

mod common;
use common::{copy_dir, openai_compatible_config, unique_dir};

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
    let root = repo_wiki::project::ProjectRoot::new(work_dir.clone());

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
    let config = format!(
        "{}[search]\nenabled = true\nindex_dir = \".search\"\ndefault_engine = \"text\"\ndefault_top_k = 10\n",
        openai_compatible_config(port, work_dir.join("wiki").to_str().unwrap())
    );
    std::fs::write(work_dir.join("mock-server.toml"), config).unwrap();

    let events: Mutex<Vec<repo_wiki::ProgressEvent>> = Mutex::new(Vec::new());
    let result = repo_wiki::run_pipeline_with_progress(
        Some(&work_dir.join("mock-server.toml")),
        None,
        true,
        &root,
        &repo_wiki::GenerationMode::Full,
        &|evt| events.lock().unwrap().push(evt),
    );
    assert!(result.is_ok(), "流水线应成功（LLM 失败被容错跳过）: {:?}", result.err());

    let events = events.into_inner().unwrap();
    // 事件序列非空且以 scanning 开始
    assert!(!events.is_empty(), "应收到进度事件");
    assert_eq!(events.first().unwrap().stage, "scanning");
    // 百分比单调递增
    for w in events.windows(2) {
        assert!(
            w[1].percent >= w[0].percent,
            "事件百分比应单调递增: {} ({}%) -> {} ({}%)",
            w[0].stage, w[0].percent, w[1].stage, w[1].percent
        );
    }
    // 以 done=100 结束
    assert_eq!(events.last().unwrap().stage, "done");
    assert_eq!(events.last().unwrap().percent, 100);

    let _ = std::fs::remove_dir_all(&work_dir);
}
