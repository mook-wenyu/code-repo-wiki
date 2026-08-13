//! -o/--output 覆盖 output.dir 的集成测试
//!
//! 将 fixtures/sample-repo 复制到唯一临时目录（避免与其他测试并发写 fixture 的
//! mock-server.toml 冲突），LLM 指向本地 mock server（返回固定 JSON 响应，
//! 生成调用成功且零重试延迟）。断言 run_pipeline(cfg, Some(out_dir), false)
//! 的输出落在 out_dir 下，而非配置默认的 .code-repo-wiki。
//!
//! root 显式注入：以 work_dir 为 ProjectRoot 传入流水线，不再依赖进程 cwd，
//! 无需 CWD 全局互斥锁，可与其余测试并行。

use std::path::Path;

mod common;
use common::{copy_dir, unique_dir};

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

    // 配置不写 [output] 段（默认 .code-repo-wiki），验证 --output 覆盖生效
    let config = format!(
        r#"

[llm]
provider = "openai-compatible"
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
    std::fs::write(work_dir.join("mock-server.toml"), config).unwrap();

    let result = code_repo_wiki::run_pipeline(
        Some(&work_dir.join("mock-server.toml")),
        Some(&out_dir),
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    );
    assert!(
        result.is_ok(),
        "流水线应成功（LLM 失败被容错跳过）: {:?}",
        result.err()
    );

    // 输出落在覆盖目录下：wiki 页面目录（主语言 zh）+ 全局文档
    assert!(
        out_dir.join("wiki").join("zh").is_dir(),
        "wiki 输出应落在覆盖目录下"
    );
    assert!(out_dir.join("wiki").join("zh").join("api.md").exists());
    assert!(out_dir.join("_toc.md").exists());
    // 覆盖后默认输出目录 .code-repo-wiki 不应被创建
    assert!(
        !work_dir.join(".code-repo-wiki").exists(),
        "覆盖后不应写默认 .code-repo-wiki"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&out_dir);
}
