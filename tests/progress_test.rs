//! run_pipeline_with_progress 进度事件测试
//!
//! 将 fixtures/sample-repo 复制到唯一临时目录（避免污染 fixture 目录，
//! 也避免与其他测试并发写 fixture 的 config.toml 冲突），LLM 指向本地
//! mock server（返回固定 JSON 响应，生成调用成功且零重试延迟），验证回调事件序列。

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 串行化依赖当前工作目录的用例（cargo 并行跑测试时互斥 cwd）
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// 生成唯一临时目录（进程 id + 自增序号）
fn unique_dir(name: &str) -> std::path::PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("repo_wiki_{}_{}_{}", name, std::process::id(), seq))
}

/// 递归复制目录（构造 fixture 副本）
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// 在指定目录下运行闭包：切换 cwd → 执行 → 恢复（panic 时也恢复）
fn with_cwd<F: FnOnce()>(dir: &Path, f: F) {
    let _guard = CWD_LOCK.lock().unwrap();
    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::set_current_dir(orig).unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn test_pipeline_progress_events_monotonic_and_done() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir("progress_repo");
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);

    with_cwd(&work_dir, || {
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

        let config = format!(
            r#"
[scope]
include = ["**/*.rs"]
exclude = []

[output]
dir = "wiki"
format = "markdown"

[llm]
provider = "openai"
model = "gpt-4o"
base_url = "http://127.0.0.1:{}/v1"
api_key = "mock"
api_key_env = "OPENAI_API_KEY"
max_concurrent = 1

[incremental]
enabled = false
strategy = "git-diff"

[search]
enabled = true
index_dir = ".search"
default_engine = "text"
default_top_k = 10
"#,
            port
        );
        std::fs::write("config.toml", config).unwrap();

        let events: Mutex<Vec<repo_wiki::ProgressEvent>> = Mutex::new(Vec::new());
        let result = repo_wiki::run_pipeline_with_progress(
            Path::new("config.toml"),
            None,
            true,
            &repo_wiki::project::ProjectRoot::from_cwd().unwrap(),
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
    });

    let _ = std::fs::remove_dir_all(&work_dir);
}
