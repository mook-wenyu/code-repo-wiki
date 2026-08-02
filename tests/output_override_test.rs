//! -o/--output 覆盖 output.dir 的集成测试
//!
//! 将 fixtures/sample-repo 复制到唯一临时目录（避免与其他测试并发写 fixture 的
//! config.toml 冲突），LLM 指向本地 mock server（返回固定 JSON 响应，
//! 生成调用成功且零重试延迟）。断言 run_pipeline(cfg, Some(out_dir), false)
//! 的输出落在 out_dir 下，而非配置默认的 .repo-wiki。

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
fn test_output_override_writes_to_given_dir() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir("output_override_repo");
    let out_dir = unique_dir("output_override_out");
    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&out_dir);
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

        // 配置不写 [output] 段（默认 .repo-wiki），验证 --output 覆盖生效
        let config = format!(
            r#"
[scope]
include = ["**/*.rs"]
exclude = []

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

        let result = repo_wiki::run_pipeline(Path::new("config.toml"), Some(&out_dir), false, &repo_wiki::project::ProjectRoot::from_cwd().unwrap(), &repo_wiki::GenerationMode::Full);
        assert!(result.is_ok(), "流水线应成功（LLM 失败被容错跳过）: {:?}", result.err());

        // 输出落在覆盖目录下：wiki 页面目录（主语言 zh）+ 全局文档
        assert!(out_dir.join("wiki").join("zh").is_dir(), "wiki 输出应落在覆盖目录下");
        assert!(out_dir.join("wiki").join("zh").join("api.md").exists());
        assert!(out_dir.join("_toc.md").exists());
        // 覆盖后默认输出目录 .repo-wiki 不应被创建
        assert!(!work_dir.join(".repo-wiki").exists(), "覆盖后不应写默认 .repo-wiki");
    });

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&out_dir);
}
