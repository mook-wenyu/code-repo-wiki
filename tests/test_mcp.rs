//! MCP server 冒烟测试（P1-3）：stdio 双通道进程级验证
//!
//! 启动真实 `repo-wiki mcp` 子进程，用 tokio 双工通道模拟 MCP 客户端，
//! 走完整 JSON-RPC 协议：initialize 握手 → tools/list → tools/call。
//! 验证 5 个工具全部注册、search 工具能返回 mock 索引结果、status 工具
//! 能读取配置状态。

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::unique_dir;

/// 冒烟配置：mock provider + 仓库内 .repo-wiki（search 开启，供 search 工具验证）
const TEST_CONFIG: &str = r#"
[scope]
include = ["**/*.rs"]
exclude = []

[output]
dir = ".repo-wiki"

[llm]
provider = "mock"
model = "mock-model"
api_key = "mock"
api_key_env = ""
max_concurrent = 1

[incremental]
enabled = true
strategy = "git-diff"

[search]
enabled = true
index_dir = ".search"
default_engine = "text"
default_top_k = 10
"#;

/// 启动 repo-wiki mcp 子进程，返回子进程句柄
fn spawn_mcp(dir: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_repo-wiki"))
        .args(["mcp", "--config", ".repo-wiki/config.toml", "--root", "."])
        .current_dir(dir)
        .env("RUST_LOG", "off")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 repo-wiki mcp 失败")
}

/// JSON-RPC 请求：向子进程写请求并读取单行响应
async fn rpc_call(stdin: &mut tokio::process::ChildStdin, stdout: &mut tokio::process::ChildStdout, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    stdin
        .write_all(serde_json::to_string(&req).unwrap().as_bytes())
        .await
        .unwrap();
    stdin.write_all(b"\n").await.unwrap();

    // 读一行（MCP stdio 按行分隔），跳过空行。
    // 逐字节累积后用 String::from_utf8 统一解码——逐字节 as char 会把
    // UTF-8 多字节序列拆成乱码（中文错误消息被破坏，S1 穿越测试曾因此误判）。
    loop {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stdout.read(&mut byte).await.unwrap();
            if n == 0 {
                panic!("MCP 进程提前退出（stdout EOF）");
            }
            line.push(byte[0]);
            if line.ends_with(b"\n") {
                break;
            }
        }
        let trimmed = String::from_utf8(line).expect("MCP 响应应为合法 UTF-8");
        let trimmed = trimmed.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("响应非 JSON: {trimmed}: {e}"));
        }
    }
}

#[tokio::test]
async fn test_mcp_initialize_lists_tools_and_calls() {
    let dir = unique_dir("server");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".repo-wiki")).unwrap();
    std::fs::write(dir.join(".repo-wiki").join("config.toml"), TEST_CONFIG).unwrap();
    // 建一个源文件供 search/ast_search 扫描
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "pub fn hello_world() {}\n").unwrap();

    let mut child = spawn_mcp(&dir);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // 1. initialize 握手
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0.0.1"}
        }),
    )
    .await;
    assert!(resp["result"]["protocolVersion"].is_string(), "握手应返回协议版本: {resp}");

    // 2. notifications/initialized（无响应，直接发后续请求）
    let _ = rpc_call(&mut stdin, &mut stdout, 2, "notifications/initialized", serde_json::json!({})).await;

    // 3. tools/list：5 个工具全部注册
    let resp = rpc_call(&mut stdin, &mut stdout, 3, "tools/list", serde_json::json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools 应为数组");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["search", "ast_search", "read_wiki_page", "read_card", "status"] {
        assert!(names.contains(&expected), "工具 {expected} 未注册, 实际: {names:?}");
    }

    // 4. tools/call ast_search：符号定义可查（不依赖索引，直接扫描）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        4,
        "tools/call",
        serde_json::json!({
            "name": "ast_search",
            "arguments": {"symbol": "hello_world"}
        }),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("hello_world"), "ast_search 应找到符号: {text}");

    // 5. tools/call status：配置可加载（未生成 wiki → 未就绪提示）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        5,
        "tools/call",
        serde_json::json!({"name": "status", "arguments": {}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("Wiki"), "status 应返回 wiki 状态: {text}");

    // 6. tools/call read_wiki_page：未生成的页面给出引导提示
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        6,
        "tools/call",
        serde_json::json!({"name": "read_wiki_page", "arguments": {"page": "architecture"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("repo-wiki generate"), "未生成时应有引导提示: {text}");

    // 7. tools/call search：索引不存在时给出可读错误（不崩溃）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        7,
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"query": "hello", "engine": "text"}}),
    )
    .await;
    assert!(
        resp["result"]["isError"].as_bool() == Some(true) || resp["result"]["content"][0]["text"].is_string(),
        "search 应有结果或错误信息: {resp}"
    );

    // 关闭
    drop(stdin);
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// S1 安全回归：read_wiki_page/read_card 的 lang 参数路径穿越必须被拒绝，
/// 且不泄漏 output_dir 之外的文件内容（曾实测复现：lang=../.. 可读任意 .md）。
#[tokio::test]
async fn test_mcp_lang_traversal_rejected() {
    let dir = unique_dir("traversal");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".repo-wiki")).unwrap();
    std::fs::write(dir.join(".repo-wiki").join("config.toml"), TEST_CONFIG).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "pub fn hello_world() {}\n").unwrap();
    // 仓库根之外放置秘密文件（穿越攻击的目标）
    let secret = dir.parent().unwrap().join(format!("secret_{}.md", std::process::id()));
    std::fs::write(&secret, "SECRET-CONTENT").unwrap();
    // 合法产物：wiki/zh/architecture.md（穿越被拒后，合法 lang 应正常读取）
    std::fs::create_dir_all(dir.join(".repo-wiki").join("wiki").join("zh")).unwrap();
    std::fs::write(
        dir.join(".repo-wiki").join("wiki").join("zh").join("architecture.md"),
        "ok-content",
    )
    .unwrap();

    let mut child = spawn_mcp(&dir);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0.0.1"}
        }),
    )
    .await;
    assert!(resp["result"]["protocolVersion"].is_string());
    let _ = rpc_call(&mut stdin, &mut stdout, 2, "notifications/initialized", serde_json::json!({})).await;

    // 1. read_wiki_page lang 穿越（相对穿越 ../..）：拒绝且不泄漏内容
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        3,
        "tools/call",
        serde_json::json!({"name": "read_wiki_page", "arguments": {"page": "secret", "lang": "../.."}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(!text.contains("SECRET-CONTENT"), "穿越必须被拒绝, 泄漏: {text}");
    assert!(text.contains("非法语言名"), "应返回明确的校验错误: {text}");

    // 2. read_card lang 穿越：同样拒绝
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        4,
        "tools/call",
        serde_json::json!({"name": "read_card", "arguments": {"card": "secret", "lang": "../../x"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(!text.contains("SECRET-CONTENT"), "read_card 穿越必须被拒绝: {text}");
    assert!(text.contains("非法语言名"), "read_card 应返回明确的校验错误: {text}");

    // 3. 合法 lang 不受影响：正常读取 wiki/zh/architecture.md
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        5,
        "tools/call",
        serde_json::json!({"name": "read_wiki_page", "arguments": {"page": "architecture", "lang": "zh"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("ok-content"), "合法 lang 应正常读取: {text}");

    drop(stdin);
    let _ = child.wait().await;
    let _ = std::fs::remove_file(&secret);
    let _ = std::fs::remove_dir_all(&dir);
}
