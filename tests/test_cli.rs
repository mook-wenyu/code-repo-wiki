//! CLI 集成测试：通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制
//!
//! 覆盖 §7 端到端验收中无测试覆盖的三项：
//! 1. uninstall 无 --force 拒绝、有 --force 卸载
//! 2. generate --progress-json 输出 JSONL 进度
//! 3. card 子命令（generate 后 modify 卡片）
//!
//! 每个测试使用独立临时目录（进程 pid + 自增序号）避免并行冲突；
//! LLM 指向本地 mock server（返回固定 JSON 响应，无网络边界）。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 生成唯一临时目录（进程 id + 自增序号）
fn unique_dir(name: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("repo_wiki_cli_{}_{}_{}", name, std::process::id(), seq))
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

/// 在指定目录下执行 repo-wiki 二进制（额外环境变量可选），返回完整输出
fn run_bin(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_repo-wiki"));
    cmd.args(args)
        .current_dir(dir)
        .env("RUST_LOG", "off") // 关闭 tracing 日志，保证 stdout 只有业务输出
        .env_remove("OPENAI_API_KEY"); // 避免宿主机真实 Key 被误用
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("执行 repo-wiki 二进制失败")
}

/// 启动本地 mock LLM server（返回固定卡片 JSON 响应），返回监听端口
fn spawn_mock_llm() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let content = r#"{"summary": "Mock 生成的摘要", "key_entities": []}"#;
        let body = serde_json::json!({ "choices": [{ "message": { "content": content } }] }).to_string();
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });
    port
}

/// 最小可用配置：LLM 指向本地 mock server，增量/搜索关闭，输出到相对路径 wiki
fn minimal_config(port: u16) -> String {
    format!(
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
enabled = false
index_dir = ".search"
default_engine = "text"
default_top_k = 10
"#,
        port
    )
}

/// 复制 fixture 并写入指向 mock LLM 的 config.toml，返回工作目录
fn prepare_repo(tag: &str) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    let port = spawn_mock_llm();
    std::fs::write(work_dir.join("config.toml"), minimal_config(port)).unwrap();
    work_dir
}

// ==================== 测试用例 ====================

/// uninstall-from-opencode：无 --force 必须拒绝（非 0 退出码 + 提示），
/// --force 在隔离 HOME 下成功（不触碰宿主机 OpenCode 配置）
#[test]
fn test_uninstall_requires_force() {
    let work_dir = unique_dir("uninstall");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    // 隔离 HOME/USERPROFILE：uninstall(force) 会读写 ~/.config/opencode/opencode.json
    let home_dir = unique_dir("uninstall_home");
    let _ = std::fs::remove_dir_all(&home_dir);
    std::fs::create_dir_all(&home_dir).unwrap();
    let envs: &[(&str, &str)] = &[
        ("HOME", home_dir.to_str().unwrap()),
        ("USERPROFILE", home_dir.to_str().unwrap()),
    ];

    // 1. 无 --force → 非 0 退出码且提示需要 --force
    let out = run_bin(&work_dir, &["uninstall-from-opencode"], envs);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "无 --force 时应拒绝卸载，退出码: {:?}",
        out.status.code()
    );
    assert!(
        combined.contains("请添加 --force"),
        "拒绝提示应包含 --force 指引，实际输出: {combined}"
    );
    assert!(combined.contains("卸载"), "拒绝提示应提及卸载");

    // 2. --force → 退出码 0；隔离环境下无 opencode.json/git hooks，安全返回
    let out = run_bin(&work_dir, &["uninstall-from-opencode", "--force"], envs);
    assert!(
        out.status.success(),
        "--force 应卸载成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 隔离 HOME 下不应产生任何文件（既无 opencode.json 也无插件残留）
    assert!(
        !home_dir.join(".config").join("opencode").join("opencode.json").exists(),
        "隔离 HOME 下卸载不应写入 opencode.json"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home_dir);
}

/// generate --progress-json：stdout 逐行 JSON，含 stage/progress，
/// progress 单调递增且以 done=100 结束
#[test]
fn test_progress_json_cli() {
    let work_dir = prepare_repo("progress_json");

    let out = run_bin(&work_dir, &["generate", "--config", "config.toml", "--progress-json"], &[]);
    assert!(
        out.status.success(),
        "generate --progress-json 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 逐行解析 JSON（跳过 tracing 等非 JSON 行）
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events: Vec<(String, u8)> = stdout
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            Some((
                v.get("stage")?.as_str()?.to_string(),
                v.get("progress")?.as_u64()? as u8,
            ))
        })
        .collect();

    assert!(!events.is_empty(), "应输出进度事件，实际 stdout: {stdout}");
    assert_eq!(events.first().unwrap().0, "scanning", "首个事件应为 scanning");
    // progress 单调递增
    for w in events.windows(2) {
        assert!(
            w[1].1 >= w[0].1,
            "progress 应单调递增: {} ({}) -> {} ({})",
            w[0].0, w[0].1, w[1].0, w[1].1
        );
    }
    assert_eq!(events.last().unwrap().0, "done", "末个事件应为 done");
    assert_eq!(events.last().unwrap().1, 100, "末个事件 progress 应为 100");
    // 输出产物存在
    assert!(work_dir.join("wiki").is_dir(), "应生成 wiki 输出目录");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card 子命令集成：generate 产出卡片后，card modify 修改卡片文件内容
#[test]
fn test_card_cli_commands() {
    let work_dir = prepare_repo("card");

    // 1. 全量 generate（mock LLM 返回卡片 JSON）→ 卡片文件落盘
    let out = run_bin(&work_dir, &["generate", "--config", "config.toml"], &[]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cards_dir = work_dir.join("wiki").join("cards").join("zh");
    let card_file = std::fs::read_dir(&cards_dir)
        .unwrap_or_else(|e| panic!("卡片目录应存在 {}: {}", cards_dir.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "md"))
        .expect("generate 后应有卡片文件");
    let module = card_file
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .replace('_', "::");
    let before = std::fs::read_to_string(&card_file).unwrap();
    assert!(!before.is_empty(), "卡片文件不应为空");

    // 2. card modify：卡片文件内容变化且为 LLM 响应
    let out = run_bin(
        &work_dir,
        &[
            "card", "modify", &module,
            "--instruction", "补充一段总结",
            "--config", "config.toml",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "card modify 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(&card_file).unwrap();
    assert_ne!(before, after, "card modify 后卡片内容应发生变化");
    assert!(
        after.contains("Mock 生成的摘要"),
        "修改后卡片应包含 LLM 响应内容，实际: {after}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card modify --reference：路径不存在时显式报错（非 0 退出码且不改动卡片）；
/// 路径存在时成功执行并更新卡片
#[test]
fn test_card_reference_validation() {
    let work_dir = prepare_repo("card_ref");

    let out = run_bin(&work_dir, &["generate", "--config", "config.toml"], &[]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cards_dir = work_dir.join("wiki").join("cards").join("zh");
    let card_file = std::fs::read_dir(&cards_dir)
        .unwrap_or_else(|e| panic!("卡片目录应存在 {}: {}", cards_dir.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "md"))
        .expect("generate 后应有卡片文件");
    let module = card_file
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .replace('_', "::");
    let before = std::fs::read_to_string(&card_file).unwrap();

    // 1. reference 指向不存在文件 → 非 0 退出码且报错（read_references 读取失败即失败）
    let out = run_bin(
        &work_dir,
        &[
            "card", "modify", &module,
            "--instruction", "补充总结",
            "--reference", "missing.md",
            "--config", "config.toml",
        ],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "reference 不存在时应显式失败，输出: {combined}"
    );
    assert!(
        combined.contains("os error 2"),
        "报错应包含缺失文件错误，实际: {combined}"
    );
    assert_eq!(
        std::fs::read_to_string(&card_file).unwrap(),
        before,
        "失败路径不应改动卡片"
    );

    // 2. reference 指向存在文件 → 成功且卡片更新
    std::fs::write(work_dir.join("refs.md"), "参考材料：新增设计约束").unwrap();
    let out = run_bin(
        &work_dir,
        &[
            "card", "modify", &module,
            "--instruction", "补充总结",
            "--reference", "refs.md",
            "--config", "config.toml",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "reference 存在时应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(&card_file).unwrap();
    assert_ne!(before, after, "reference 存在时卡片应被更新");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// B6:export 命令 CLI 级 E2E——generate 后执行 export,
/// 断言 HTML 产物目录生成且含主文档页(html export 仅单测覆盖,
/// 此处补真实二进制链路)
#[test]
fn test_export_produces_html_artifacts() {
    let work_dir = prepare_repo("export");

    let gen_out = run_bin(
        &work_dir,
        &["generate", "--config", "config.toml"],
        &[],
    );
    assert!(
        gen_out.status.success(),
        "generate 应成功, stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let out = run_bin(
        &work_dir,
        &["export", "--config", "config.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "export 应成功, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // html 产物目录:export_html 写入 {output.dir}/html/?
    // 以实际落盘文件断言(不臆测路径,失败时列出目录内容)
    // html 产物:export_html 在 wiki/ 目录内写 {title}.html + 根 index.html
    // (与 .md 并存,不建独立 html/ 子目录)
    let html_dir = work_dir.join("wiki");
    let html_files: Vec<_> = std::fs::read_dir(&html_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "html")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !html_files.is_empty(),
        "wiki/ 目录应包含 .html 导出文件: {}",
        html_dir.display()
    );
    assert!(
        work_dir.join("wiki").join("index.html").exists(),
        "wiki/index.html(目录页)应生成"
    );
    assert!(
        !html_files.is_empty(),
        "html 目录应包含导出文件: {}",
        html_dir.display()
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
