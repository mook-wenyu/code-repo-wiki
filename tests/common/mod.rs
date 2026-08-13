//! 集成测试公共 helper（v13 B8：从 8+ 测试文件去重收敛）
//!
//! tests/ 下每个文件是独立 crate，不能共享模块——使用方在文件顶部
//! 声明 `mod common;` 后即可 use 本模块的 helper（Rust 集成测试约定：
//! tests/common/mod.rs 不会被当作独立测试目标）。
//!
//! 提供：唯一临时目录（进程 pid + 自增序号防并行冲突）、递归目录复制、
//! 真实二进制执行（两种形态：纯 args / 带环境变量注入，后者用于隔离
//! HOME/USERPROFILE 等宿主环境变量）、固定 SSE 响应的 mock LLM server。
//!
//! 注意：本模块不包含各测试特有的 fixture 构造（prepare_repo /
//! minimal_config 等）——它们与具体测试的断言语义耦合，
//! 保留在各自文件（避免为合并而参数化过度抽象）。
//!
//! common 被每个测试 crate 独立编译，各自只用 helper 子集——
//! 未使用的 helper 会产生 dead_code 警告，故模块级 allow。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 创建唯一临时目录路径（不创建目录本身；调用方自管清理）
///
/// 唯一性 = 进程 pid + 进程内自增序号（同一进程并行测试不冲突，
/// 不同测试二进制进程 pid 不同）。与原各文件实现形态一致。
pub fn unique_dir(name: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("code_repo_wiki_{}_{}_{}", name, std::process::id(), seq))
}

/// 递归复制目录（fixture 复制用）
pub fn copy_dir(src: &Path, dst: &Path) {
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

/// 在指定目录下执行 code-repo-wiki 二进制，返回完整输出
///
/// 固定行为：关闭 tracing 日志（保证 stdout 只有业务输出）、
/// 移除宿主机 OPENAI_API_KEY（避免真实 Key 被误用）。
pub fn run_bin(dir: &Path, args: &[&str]) -> Output {
    run_bin_with_envs(dir, args, &[])
}

/// 带环境变量注入的二进制执行（隔离宿主环境用，如 HOME/USERPROFILE）
pub fn run_bin_with_envs(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_code-repo-wiki"));
    cmd.args(args)
        .current_dir(dir)
        .env("RUST_LOG", "off") // 关闭 tracing 日志，保证 stdout 只有业务输出
        .env_remove("OPENAI_API_KEY"); // 避免宿主机真实 Key 被误用
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("执行 code-repo-wiki 二进制失败")
}

/// mock provider 配置模板（v19 t04：output.dir 强制绝对路径）
///
/// 调用方必须传临时目录的绝对路径（如 `unique_dir("x").join(".code-repo-wiki")`
/// 的字符串形式）。相对路径依赖进程 cwd——watch 常驻、CI 目录漂移或
/// 并行测试时可能解析到仓库根，把产物泄漏进工作区（v13 C2 曾清理过
/// 仓库根 wiki/ 泄漏）。mock provider 不触网，无需 mock server。
/// mock provider 不触网，无需 mock server；embed 同样用内置 mock
///（v30：output/incremental/search 键已硬编码，不再出现在模板中）
pub fn mock_config() -> String {
    r#"

[llm]
provider = "mock"
model = "mock-model"
api_key = "mock"
api_key_env = ""
max_concurrency = 1

[embed]
provider = "mock"
model = "mock-embed"
api_key_env = ""
"#
    .to_string()
}

/// openai-compatible 形态配置模板（走本地 mock SSE server）
///
/// 与 mock_config 同规则：output_dir 必须传绝对路径（cwd 依赖 = 泄漏
/// 隐患）。port 为 mock_llm_server() 返回的监听端口。
pub fn openai_compatible_config(port: u16) -> String {
    format!(
        r#"

[llm]
provider = "openai-compatible"
model = "gpt-4o"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock"
api_key_env = "OPENAI_API_KEY"
max_concurrency = 1

[embed]
provider = "mock"
model = "mock-embed"
api_key_env = ""
"#
    )
}

/// 启动本地 mock LLM server（返回监听端口）
///
/// 响应为 SSE 流式格式（v13 A4 后生产路径统一请求 stream:true）：
/// data: 行内 choices[0].delta.content 内嵌卡片 JSON + [DONE] 结束标记。
/// Content-Type 为 text/event-stream，Connection: close 迫使每次请求新建连接。
pub fn mock_llm_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let content = r#"{"summary": "Mock 生成的摘要", "key_entities": []}"#;
        let payload = serde_json::json!({ "choices": [{ "delta": { "content": content } }] });
        let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });
    port
}
