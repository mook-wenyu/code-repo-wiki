//! MCP server 冒烟测试（P1-3）：stdio 双通道进程级验证
//!
//! 启动真实 `code-repo-wiki mcp` 子进程，用 tokio 双工通道模拟 MCP 客户端，
//! 走完整 JSON-RPC 协议：initialize 握手 → tools/list → tools/call。
//! 验证 5 个工具全部注册、search 工具能返回 mock 索引结果、status 工具
//! 能读取配置状态。

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::{mock_config, run_bin_with_envs, unique_dir};

/// 启动 code-repo-wiki mcp 子进程，返回子进程句柄
fn spawn_mcp(dir: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_code-repo-wiki"))
        .args(["mcp", "--config", "mcp-test.toml", "--root", "."])
        .current_dir(dir)
        .env("RUST_LOG", "off")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 code-repo-wiki mcp 失败")
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
    std::fs::create_dir_all(dir.join(".code-repo-wiki")).unwrap();
    let config = mock_config();
    std::fs::write(dir.join("mcp-test.toml"), &config).unwrap();
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
    assert!(text.contains("code-repo-wiki generate"), "未生成时应有引导提示: {text}");

    // 7. tools/call search：索引不存在时给出可读引导错误（不崩溃，
    //    且错误消息精确指明索引缺失并引导运行 generate——索引缺失
    //    路径的精确断言；降级提示路径见 test_mcp_status_uses_root_and_shows_degradation）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        7,
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"query": "hello", "engine": "text"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(
        text.contains("搜索索引不存在"),
        "索引缺失应引导运行 generate，实际: {text}"
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
    std::fs::create_dir_all(dir.join(".code-repo-wiki")).unwrap();
    let config = mock_config();
    std::fs::write(dir.join("mcp-test.toml"), &config).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "pub fn hello_world() {}\n").unwrap();
    // 仓库根之外放置秘密文件（穿越攻击的目标）
    let secret = dir.parent().unwrap().join(format!("secret_{}.md", std::process::id()));
    std::fs::write(&secret, "SECRET-CONTENT").unwrap();
    // 合法产物：wiki/zh/architecture.md（穿越被拒后，合法 lang 应正常读取）
    std::fs::create_dir_all(dir.join(".code-repo-wiki").join("wiki").join("zh")).unwrap();
    std::fs::write(
        dir.join(".code-repo-wiki").join("wiki").join("zh").join("architecture.md"),
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

/// v0.6（cli-vs-mcp-03 / cli-vs-mcp-07 / FR-501）防回归：
///
/// 1. status 工具必须用注入的 --root（self.root）而非 from_cwd()——
///    跨 cwd 调用（进程 cwd ≠ 项目根）时，from_cwd 解析到启动目录，
///    lint 扫错目录、误报"Wiki 未生成"。本测试以 cwd=dir/sub 启动
///    MCP（--root . 相对 cwd 解析为 sub），产物在 dir 根：修复前
///    status 从 sub 找不到产物报未生成，修复后必须就绪。
/// 2. FR-501：语义降级标记（.search/semantic_degraded）存在时，
///    search 结果尾部与 status 报告必须显式提示降级原因（此前降级
///    仅进 tracing 日志，MCP 调用方不可见——cli-vs-mcp-07）。本测试
///    真实构建文本索引后直接断言 search 结果尾部提示行——覆盖 mcp.rs
///    search 工具的降级分支（而非仅 lib 函数级覆盖）。
#[tokio::test]
async fn test_mcp_status_uses_root_and_shows_degradation() {
    let dir = unique_dir("root_status");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".code-repo-wiki")).unwrap();
    let config = mock_config();
    std::fs::write(dir.join("mcp-test.toml"), &config).unwrap();
    // 源文件：search 命中实体（hello_world）
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "pub fn hello_world() {}\n").unwrap();
    // 真实构建搜索索引（mock provider 不触网；text_index.db 与 wiki 页面
    // 均落 dir 根）——FR-501 的 search 分支须 search 实际返回命中才能断言
    // 结果尾部提示，不能仅用索引缺失的错误路径
    let gen_out = run_bin_with_envs(&dir, &["generate", "--config", "mcp-test.toml"], &[]);
    assert!(
        gen_out.status.success(),
        "generate 应成功（构建索引），stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );
    // 语义降级标记（模拟：语义索引构建失败后由生成流程写入）。必须放在
    // generate 之后写——generate 语义构建成功会清除旧标记、失败也会写入
    // 自己的原因；手工覆盖保证原因内容确定、可精确断言
    std::fs::create_dir_all(dir.join(".code-repo-wiki").join(".search")).unwrap();
    std::fs::write(
        dir.join(".code-repo-wiki").join(".search").join("semantic_degraded"),
        "embed key 未配置",
    )
    .unwrap();

    // 关键：cwd 切到子目录启动，--root 显式指向 dir（产物在 dir 根）——
    // self.root=dir（就绪）vs from_cwd=sub（误报未生成），正是
    // cli-vs-mcp-03 的跨 cwd 场景。--config 同样必须绝对路径（cwd≠root
    // 时相对路径解析失效，与 test_search_with_root_from_subdir 同规则）
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let cfg_str = dir.join("mcp-test.toml").to_str().unwrap().to_string();
    let root_str = dir.to_str().unwrap().to_string();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_code-repo-wiki"))
        .args(["mcp", "--config", &cfg_str, "--root", &root_str])
        .current_dir(&sub)
        .env("RUST_LOG", "off")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 code-repo-wiki mcp 失败");
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

    // 1. status：cwd≠root 时仍应就绪（修复前 from_cwd → sub 找不到产物）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        3,
        "tools/call",
        serde_json::json!({"name": "status", "arguments": {}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("Wiki 就绪"), "status 应就绪（root 化），实际: {text}");
    // FR-501：status 报告显式提示降级原因
    assert!(
        text.contains("语义索引: 已降级") && text.contains("embed key 未配置"),
        "status 应显式提示降级原因, 实际: {text}"
    );

    // 2. search：降级标记存在 + 真实文本索引 → 结果尾部必须显式提示
    //    降级原因（FR-501 的 search 输出提示，cli-vs-mcp-07——此前降级
    //    仅进 tracing 日志，MCP 调用方不可见）。精确断言尾部行：
    //    提示必须位于结果之后
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        4,
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"query": "hello_world", "engine": "text"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.contains("hello_world"), "search 应命中索引实体: {text}");
    let tail = text.lines().last().map(str::trim).unwrap_or("");
    assert_eq!(
        tail,
        "提示: 语义索引已降级（原因: embed key 未配置）",
        "结果尾部应显式提示降级原因, 实际: {text}"
    );

    // 3. search 空结果 + 降级标记：提示必须无条件出现（reviewer 14.2
    //    必须项闭合——降级场景 hybrid 静默降级为纯 text 可能返回空
    //    结果，用户必须仍能看到降级原因，与 CLI 文本模式一致）
    let resp = rpc_call(
        &mut stdin,
        &mut stdout,
        5,
        "tools/call",
        serde_json::json!({"name": "search", "arguments": {"query": "no_such_symbol_xyz", "engine": "text"}}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().expect("工具结果应有 text");
    assert!(text.starts_with("未找到匹配结果"), "空结果应报告未找到: {text}");
    let tail = text.lines().last().map(str::trim).unwrap_or("");
    assert_eq!(
        tail,
        "提示: 语义索引已降级（原因: embed key 未配置）",
        "空结果尾部也应显式提示降级原因, 实际: {text}"
    );

    drop(stdin);
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}
