//! CLI 集成测试：通过 env!("CARGO_BIN_EXE_code-repo-wiki") 调用真实二进制
//!
//! 覆盖 §7 端到端验收中无测试覆盖的三项：
//! 1. uninstall 无 --force 拒绝、有 --force 卸载
//! 2. generate --progress-json 输出 JSONL 进度
//! 3. card 子命令（generate 后 modify 卡片）
//!
//! 每个测试使用独立临时目录（进程 pid + 自增序号）避免并行冲突；
//! LLM 指向本地 mock server（返回固定 SSE 流式响应，无网络边界）。
//! 公共 helper（unique_dir/copy_dir/run_bin_with_envs/mock_llm_server）
//! 收敛于 common 模块（v13 B8）。

mod common;
use code_repo_wiki::config::schema::WikiConfig;
use code_repo_wiki::fs::acquire_run_lock;
use common::{copy_dir, mock_llm_server, openai_compatible_config, run_bin_with_envs, unique_dir};
use std::path::{Path, PathBuf};

/// 最小可用配置：LLM 指向本地 mock server，输出硬编码 .code-repo-wiki
/// （v30：output/incremental/search 键已硬编码，配置仅 scope/llm/embed 三段）
fn minimal_config(port: u16) -> String {
    openai_compatible_config(port)
}

/// 复制 fixture 并写入指向 mock LLM 的 mock-server.toml，返回工作目录
fn prepare_repo(tag: &str) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    let port = mock_llm_server();
    std::fs::write(work_dir.join("mock-server.toml"), minimal_config(port)).unwrap();
    work_dir
}

// ==================== 测试用例 ====================

/// uninstall：无 --force 必须拒绝（非 0 退出码 + 提示），
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
    let out = run_bin_with_envs(&work_dir, &["uninstall"], envs);
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
    let out = run_bin_with_envs(&work_dir, &["uninstall", "--force"], envs);
    assert!(
        out.status.success(),
        "--force 应卸载成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 隔离 HOME 下不应产生任何文件（既无 opencode.json 也无插件残留）
    assert!(
        !home_dir
            .join(".config")
            .join("opencode")
            .join("opencode.json")
            .exists(),
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

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "generate",
            "--config",
            "mock-server.toml",
            "--progress-json",
        ],
        &[],
    );
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
    assert_eq!(
        events.first().unwrap().0,
        "scanning",
        "首个事件应为 scanning"
    );
    // progress 单调递增
    for w in events.windows(2) {
        assert!(
            w[1].1 >= w[0].1,
            "progress 应单调递增: {} ({}) -> {} ({})",
            w[0].0,
            w[0].1,
            w[1].0,
            w[1].1
        );
    }
    assert_eq!(events.last().unwrap().0, "done", "末个事件应为 done");
    assert_eq!(events.last().unwrap().1, 100, "末个事件 progress 应为 100");
    // T09a 回归防线：progress_json 模式下 stdout 每行都必须可解析为 JSON
    //（旧纯文本摘要污染会被 filter_map 静默丢弃而漏检，此处逐行强制解析）
    let parsed: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("progress_json 输出行应可解析为 JSON: {e}\n行内容: {line}\n完整 stdout: {stdout}")
            })
        })
        .collect();
    // 完成摘要行（stage:"done" 且无 progress 字段，与事件行区分）
    let done_v = parsed
        .iter()
        .find(|v| {
            v.get("stage").and_then(|s| s.as_str()) == Some("done") && v.get("progress").is_none()
        })
        .unwrap_or_else(|| panic!("应存在 stage=done 完成摘要行，实际 stdout: {stdout}"));
    // files/entities/documents/elapsed_secs 均为数字（锚定 main.rs:502-508 摘要格式）
    for field in ["files", "entities", "documents", "elapsed_secs"] {
        assert!(
            done_v.get(field).is_some_and(|v| v.is_number()),
            "done 摘要行 {field} 应为数字: {done_v}"
        );
    }
    // 输出产物存在
    assert!(
        work_dir.join(".code-repo-wiki").is_dir(),
        "应生成 wiki 输出目录"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --root 搜索路径 root 化防回归（v0.6 P0，审计 cli-vs-mcp-02）：
///
/// 从 cwd ≠ root 的嵌套子目录以 --root 调用 search，必须命中 root
/// 下的搜索索引而非回退到 cwd 的相对目录。修复前 execute_search 用
/// 裸 config 加载，output_dir=None 时 schema.rs::output_dir() 回退
/// 相对 cwd 的 .code-repo-wiki，子目录运行会读错索引目录。
#[test]
fn test_search_with_root_from_subdir() {
    let work_dir = prepare_repo("search_root_subdir");
    // 建索引（产物落在 work_dir/.code-repo-wiki 下）
    let gen_out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        gen_out.status.success(),
        "generate 应成功, stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    // cwd ≠ root：从 work_dir 的嵌套子目录运行（最小实现：创建 sub 目录）
    let subdir = work_dir.join("sub");
    std::fs::create_dir_all(&subdir).unwrap();
    let root_str = work_dir.to_str().unwrap();
    // --config 与 --root 必须绝对路径（cwd 是 subdir 时相对路径解析失效）
    let cfg_path = work_dir.join("mock-server.toml");
    let cfg_str = cfg_path.to_str().unwrap();

    let out = run_bin_with_envs(
        &subdir,
        &[
            "search",
            "-q",
            "authenticate",
            "-k",
            "3",
            "--engine",
            "text",
            "--json",
            "--root",
            root_str,
            "--config",
            cfg_str,
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "search --root 应从子目录成功, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("应输出合法 JSON: {e}\n实际: {stdout}"));
    assert!(
        hits.iter().any(|h| h
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| n.contains("authenticate"))
            .unwrap_or(false)),
        "应命中 authenticate（证明读的是 root 索引而非 cwd 索引）, 实际 hits: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

#[test]
fn test_watch_exits_immediately_on_live_lock() {
    // Phase 15.5 回归防线（适配 Phase 15.1 fd-lock 内核锁语义）：
    // watch 遇真并发（另一实例持有内核写锁）必须立即退出（非成功退出码）
    // 而不是退避重试——锁冲突属于另一实例正在运行，重试只会浪费轮次
    //（v51 67 次连续锁错误的教训）。
    // 旧方案「预置含当前 PID 的 run.lock 文件」模拟并发已失效：新语义下
    //「存在锁文件」≠「持有内核锁」——watch 能正常获取内核锁并进入监听，
    // 断言非成功退出码会永远等待（测试挂起）。改真实持锁者场景：主测试
    // 进程经 acquire_run_lock 真正持有内核写锁，watch 子进程作为独立进程
    // 打开同一锁文件必然撞锁（fd-lock 锁绑定打开句柄而非路径）。
    let work_dir = prepare_repo("watch_live_lock");

    // 主测试进程持有内核写锁。output_dir 注入与 watch 子进程锁路径一致：
    // 子进程 cwd=work_dir、config 无 output.dir → output_dir() 回退
    // .code-repo-wiki 相对 cwd = work_dir/.code-repo-wiki。
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    // 持锁状态下 spawn watch 子进程 → 启动即撞内核锁 → 非成功退出码 +
    // 报「另一实例正在运行」。Windows 上 LockFileEx 阻止子进程读锁定区域，
    // 错误信息会退化为「PID 未知」——断言只依赖「正在运行」而非持锁者身份。
    let out = run_bin_with_envs(&work_dir, &["watch", "--config", "mock-server.toml"], &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "watch 撞内核锁应立即退出，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("正在运行"),
        "撞锁报错应含「正在运行」提示，实际输出: {combined}"
    );

    // 释放锁后 watch 能正常启动（阻塞监听：spawn + 轮询 try_wait + kill，
    // 参考 test_cli_smoke.rs 的 watch 子进程处理模式）。撞锁路径在流水线
    // 入口约 1s 内退出，观察窗口 10s 远超该值：若提前退出即撞锁（失败）；
    // 存活超过窗口即证明已获取内核锁进入生成/监听，随后 kill 收尾。
    drop(_lock);
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(env!("CARGO_BIN_EXE_code-repo-wiki"))
        .args(["watch", "--config", "mock-server.toml"])
        .current_dir(&work_dir)
        .env("RUST_LOG", "off")
        .env_remove("OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 watch 失败");
    // 排空 stdout/stderr 管道：子进程持有管道，缓冲填满会阻塞其继续处理
    //（watch 单测的死锁陷阱）
    let mut out_pipe = child.stdout.take().expect("取 stdout 管道失败");
    let mut err_pipe = child.stderr.take().expect("取 stderr 管道失败");
    let drain_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
    });
    let drain_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut early_exit: Option<std::process::ExitStatus> = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(status) = child.try_wait().expect("try_wait 失败") {
            early_exit = Some(status);
            break;
        }
    }
    assert!(
        early_exit.is_none(),
        "释放锁后 watch 不应提前退出（撞锁即失败），退出码: {:?}",
        early_exit.map(|s| s.code())
    );
    // 收尾：kill 子进程（watch 是阻塞监听，必须显式终止）
    let _ = child.kill();
    let _ = child.wait();
    drain_out.join().unwrap();
    drain_err.join().unwrap();

    let _ = std::fs::remove_dir_all(&work_dir);
}

// ==================== Phase 15.2：--skip-if-locked / --wait ====================
//
// 与 fs.rs 单测互补：单测覆盖 LockOptions 组合逻辑（同进程时序），此处覆盖
// 真实二进制跨进程锁冲突——主测试进程经 acquire_run_lock 持有内核写锁，子
// 进程（独立进程）打开同一锁文件必然撞锁（fd-lock 锁绑定打开句柄而非路径）。
// 锁路径对齐：子进程 cwd=work_dir、config 无 output.dir → output_dir() 回退
// .code-repo-wiki 相对 cwd = work_dir/.code-repo-wiki（同 watch 撞锁测试）。

/// --skip-if-locked：generate 撞锁时退出码 0 跳过，不打印「生成完成」误导文案
#[test]
fn test_generate_skip_if_locked_exits_zero() {
    let work_dir = prepare_repo("lock_skip_cli");
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "generate",
            "--config",
            "mock-server.toml",
            "--skip-if-locked",
        ],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "--skip-if-locked 撞锁应退出码 0 跳过，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("跳过"),
        "跳过提示应含「跳过」，实际输出: {combined}"
    );
    assert!(
        !combined.contains("生成完成"),
        "跳过时不应打印「生成完成」误导文案，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --skip-if-locked：update 撞锁同样退出码 0 跳过（update 分发处有独立
/// skipped 处理分支，需独立覆盖防回归）
#[test]
fn test_update_skip_if_locked_exits_zero() {
    let work_dir = prepare_repo("lock_skip_cli_update");
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(
        &work_dir,
        &["update", "--config", "mock-server.toml", "--skip-if-locked"],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "update --skip-if-locked 撞锁应退出码 0 跳过，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("跳过"),
        "跳过提示应含「跳过」，实际输出: {combined}"
    );
    assert!(
        !combined.contains("增量更新完成") && !combined.contains("无文件变更"),
        "跳过时不应打印完成/no-op 误导文案，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --wait：持锁期间 `--wait 1` 等待 1 秒后仍冲突 → 超时报错（非 0 退出码 +
/// 含「正在运行」）；对应"超时仍报错"规格
#[test]
fn test_wait_timeout_still_errors() {
    let work_dir = prepare_repo("lock_wait_cli");
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(
        &work_dir,
        &["update", "--config", "mock-server.toml", "--wait", "1"],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "update --wait 1 持锁期间应超时失败，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("正在运行"),
        "超时报错应含「正在运行」，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// update --progress-json（P2-11）：增量更新命令同样输出 JSONL 进度事件
///（U08 给 update 补上 progress 输出后一直无冒烟覆盖——generate 已有
/// test_progress_json_cli，update 的 progress-json 分支必须同等验证：
/// 事件非空、以 done=100 结束。增量禁用/非 git 时 update 回退全量路径，
/// run_pipeline_with_progress 的进度事件照常输出，断言不依赖具体模式）
#[test]
fn test_update_progress_json_cli() {
    let work_dir = prepare_repo("update_progress_json");

    // 先全量 generate 建立基线（增量路径需要产物与状态）
    let gen_out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        gen_out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    // 修改一个源文件触发变更（update 以 changed_files 判定有无更新）
    let src = work_dir.join("src").join("main.rs");
    let mut content = std::fs::read_to_string(&src).unwrap();
    content.push_str("\n// update --progress-json 触发注释\n");
    std::fs::write(&src, content).unwrap();

    let out = run_bin_with_envs(
        &work_dir,
        &["update", "--config", "mock-server.toml", "--progress-json"],
        &[],
    );
    assert!(
        out.status.success(),
        "update --progress-json 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

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
    assert!(
        !events.is_empty(),
        "update 应输出进度事件，实际 stdout: {stdout}"
    );
    assert_eq!(events.last().unwrap().0, "done", "末个事件应为 done");
    assert_eq!(events.last().unwrap().1, 100, "末个事件 progress 应为 100");
    // T09a 回归防线：progress_json 模式下 stdout 每行都必须可解析为 JSON
    //（旧纯文本摘要污染会被 filter_map 静默丢弃而漏检，此处逐行强制解析）
    let parsed: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("progress_json 输出行应可解析为 JSON: {e}\n行内容: {line}\n完整 stdout: {stdout}")
            })
        })
        .collect();
    // 终态摘要行（无 progress 字段，与事件行区分）：真实更新为 done、no-op 为 noop
    //（本场景修改了源文件 → 走真实更新路径触达 done；noop 分支防回归）
    let summary = parsed
        .iter()
        .find(|v| {
            v.get("progress").is_none()
                && matches!(
                    v.get("stage").and_then(|s| s.as_str()),
                    Some("done") | Some("noop")
                )
        })
        .unwrap_or_else(|| panic!("应存在 done/noop 终态摘要行，实际 stdout: {stdout}"));
    match summary.get("stage").and_then(|s| s.as_str()).unwrap() {
        // 真实更新摘要（锚定 main.rs:596-601）：files/documents/elapsed_secs 为数字
        //（update 摘要行无 entities 字段，与 generate 不同）
        "done" => {
            for field in ["files", "documents", "elapsed_secs"] {
                assert!(
                    summary.get(field).is_some_and(|v| v.is_number()),
                    "done 摘要行 {field} 应为数字: {summary}"
                );
            }
        }
        // no-op 摘要行（锚定 main.rs:588-590）仅含 stage 字段，存在性断言已覆盖
        "noop" => {}
        _ => unreachable!("终态摘要行 stage 仅可能为 done 或 noop"),
    }

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card 子命令集成：generate 产出卡片后，card modify 修改卡片文件内容
#[test]
fn test_card_cli_commands() {
    let work_dir = prepare_repo("card");

    // 1. 全量 generate（mock LLM 返回卡片 JSON）→ 卡片文件落盘
    let out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cards_dir = work_dir.join(".code-repo-wiki").join("cards").join("zh");
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
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "modify",
            &module,
            "--instruction",
            "补充一段总结",
            "--config",
            "mock-server.toml",
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

    let out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cards_dir = work_dir.join(".code-repo-wiki").join("cards").join("zh");
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
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "modify",
            &module,
            "--instruction",
            "补充总结",
            "--reference",
            "missing.md",
            "--config",
            "mock-server.toml",
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
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "modify",
            &module,
            "--instruction",
            "补充总结",
            "--reference",
            "refs.md",
            "--config",
            "mock-server.toml",
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

    let gen_out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        gen_out.status.success(),
        "generate 应成功, stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let out = run_bin_with_envs(&work_dir, &["export", "--config", "mock-server.toml"], &[]);
    assert!(
        out.status.success(),
        "export 应成功, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // html 产物目录:export_html 写入 {output.dir}/html/?
    // 以实际落盘文件断言(不臆测路径,失败时列出目录内容)
    // html 产物:export_html 在 wiki/ 目录内写 {title}.html + 根 index.html
    // (与 .md 并存,不建独立 html/ 子目录)
    let html_dir = work_dir.join(".code-repo-wiki");
    let html_files: Vec<_> = std::fs::read_dir(&html_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "html").unwrap_or(false))
        .collect();
    assert!(
        !html_files.is_empty(),
        "wiki/ 目录应包含 .html 导出文件: {}",
        html_dir.display()
    );
    assert!(
        work_dir.join(".code-repo-wiki").join("index.html").exists(),
        "wiki/index.html(目录页)应生成"
    );
    assert!(
        !html_files.is_empty(),
        "html 目录应包含导出文件: {}",
        html_dir.display()
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// Hybrid CLI 搜索集成测试：generate(索引+调用图) 后 search --engine hybrid --json，
/// 断言命中结果含 callers/callees 调用链补全字段（验证 execute_search 的
/// call_index 注入链路从 CLI 端到端可用）
#[test]
fn test_search_hybrid_includes_callchain() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir("search_hybrid");
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);

    // 覆盖 mock-server.toml：启用搜索索引（fixture 自带 config 可能未开）
    // v19 t04：基于 helper 模板 + 追加 search 段（搜索特例，dir 绝对路径）
    let port = mock_llm_server();
    let cfg = openai_compatible_config(port);
    std::fs::write(work_dir.join("mock-server.toml"), cfg).unwrap();

    let gen_out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        gen_out.status.success(),
        "generate 应成功, stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "search",
            "-q",
            "authenticate",
            "-k",
            "3",
            "--engine",
            "hybrid",
            "--json",
            "--config",
            "mock-server.toml",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "hybrid 搜索应成功, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("应输出合法 JSON: {e}\n实际: {stdout}"));

    // main 调用 authenticate（真实调用边），命中 authenticate 时应补全调用者 main
    let auth_hit = hits.iter().find(|h| {
        h.get("name")
            .and_then(|n| n.as_str())
            .map(|n| n.contains("authenticate"))
            .unwrap_or(false)
    });
    assert!(
        auth_hit.is_some(),
        "应命中 authenticate, 实际 hits: {stdout}"
    );
    let hit = auth_hit.unwrap();
    let callers = hit
        .get("callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !callers.is_empty(),
        "authenticate 的调用链补全应含调用者 main, 实际 callers: {callers:?} (hit: {hit})"
    );
    assert!(hit.get("callees").is_some(), "命中结果应含 callees 字段");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// lint 命令:干净产物通过(exit 0);人为制造孤儿页/断链后失败(非 0)并列出问题。
/// 注:直接构造产物 fixture(不依赖 generate 的 output.dir 嵌套语义),保证断言确定性。
#[test]
fn test_lint_detects_issues_in_artifacts() {
    let work_dir = unique_dir("lint");
    let _ = std::fs::remove_dir_all(&work_dir);
    // 构造"干净"产物:单个 wiki 页 + 目录页(链接指向 core → core 有入链非孤儿)。
    // 产物布局遵循 render_all 规则:config.output_dir() 下再建 wiki/{lang}/ 子目录
    let wiki = work_dir.join(".code-repo-wiki").join("wiki").join("zh");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(wiki.join("core.md"), "# Core\n\n模块页\n").unwrap();
    std::fs::write(
        work_dir.join(".code-repo-wiki").join("_toc.md"),
        "# 目录\n\n- [Core](wiki/zh/core.md)\n",
    )
    .unwrap();
    std::fs::write(
        work_dir.join("mock-server.toml"),
        r#"
[llm]
provider = "mock"
model = "mock"
base_url = "x"
api_key = "mock"
api_key_env = ""
max_concurrent = 1
"#,
    )
    .unwrap();

    // 干净产物 → lint 通过(无孤儿页)
    let clean = run_bin_with_envs(&work_dir, &["lint", "--config", "mock-server.toml"], &[]);
    assert!(
        clean.status.success(),
        "干净产物 lint 应通过, stderr: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // 人为制造孤儿页(无人链接的新页面)与断链(链接到不存在文件)
    std::fs::write(
        wiki.join("orphan_page.md"),
        "# 孤儿页\n\n- [不存在](wiki/zh/missing.md)\n",
    )
    .unwrap();

    let dirty = run_bin_with_envs(&work_dir, &["lint", "--config", "mock-server.toml"], &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&dirty.stdout),
        String::from_utf8_lossy(&dirty.stderr)
    );
    assert!(
        !dirty.status.success(),
        "孤儿页+断链应导致 lint 失败, 输出: {combined}"
    );
    assert!(
        combined.contains("orphan"),
        "应报告孤儿页问题, 输出: {combined}"
    );
    assert!(
        combined.contains("missing.md"),
        "应报告断链问题, 输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// AST 精确符号查找：扫描源文件定位符号定义(文件+行号+签名)，
/// 不依赖搜索索引。sample-repo 的 auth.rs 定义 authenticate 函数。
#[test]
fn test_ast_search_finds_definition() {
    let work_dir = prepare_repo("ast_search");

    let out = run_bin_with_envs(
        &work_dir,
        &["ast-search", "authenticate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "ast-search 应成功, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("pub fn authenticate"),
        "应输出 authenticate 函数签名, 实际: {combined}"
    );
    assert!(
        combined.contains("auth.rs"),
        "应定位到 auth.rs, 实际: {combined}"
    );

    // 不存在的符号 → 明确提示
    let miss = run_bin_with_envs(
        &work_dir,
        &[
            "ast-search",
            "definitely_missing",
            "--config",
            "mock-server.toml",
        ],
        &[],
    );
    let miss_out = format!(
        "{}{}",
        String::from_utf8_lossy(&miss.stdout),
        String::from_utf8_lossy(&miss.stderr)
    );
    assert!(
        miss_out.contains("未找到符号"),
        "缺失符号应提示, 实际: {miss_out}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

// ==================== A7.10 audit-cli 系列（CLI 层修复回归防线） ====================

/// audit-cli-02：ast-search --language 非法值必须显式报错（非 0 退出码）——
/// 修复前 execute_ast_search 内 AstQuery::new 失败被 continue 静默跳过，
/// 退出码 0 且误报「未找到符号」（假阴性）。get_language 与 AST 扫描同源。
#[test]
fn test_ast_search_invalid_language_errors() {
    let work_dir = prepare_repo("ast_search_invalid_lang");

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "ast-search",
            "authenticate",
            "--language",
            "bogus",
            "--config",
            "mock-server.toml",
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
        "非法 language 应非 0 退出码，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("不支持的语言"),
        "应报不支持的语言，实际: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// audit-cli-01：update --output 尾部复核必须扫流水线实际写盘的输出目录——
/// 修复前 load_config_rooted 漏传 --output，复核扫默认 .code-repo-wiki，
/// --output 下产物有 lint 问题时静默假阴性。制造孤儿页证明复核目录正确。
#[test]
fn test_update_output_tail_lint_scans_output_dir() {
    let work_dir = prepare_repo("update_output_tail");

    // 1. generate --output 自定义目录（产物落在 work_dir/custom-out）
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "generate",
            "--config",
            "mock-server.toml",
            "--output",
            "custom-out",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "generate --output 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out_dir = work_dir.join("custom-out");
    assert!(
        out_dir.join("wiki").join("zh").is_dir(),
        "产物应落在 --output 目录"
    );

    // 2. 在 --output 目录制造孤儿页（lint 必报问题；若复核扫默认目录则静默漏检）
    std::fs::write(
        out_dir.join("wiki").join("zh").join("orphan.md"),
        "# 孤儿页\n",
    )
    .unwrap();

    // 3. 修改源文件触发真实增量更新（非 git 仓库回退全量语义，仍会执行）
    let src = work_dir.join("src").join("main.rs");
    let mut content = std::fs::read_to_string(&src).unwrap();
    content.push_str("\n// update --output 尾部复核触发\n");
    std::fs::write(&src, content).unwrap();

    // 4. update --output：尾部复核应扫 custom-out 并发现孤儿页（RUST_LOG=warn 捕获）
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "update",
            "--config",
            "mock-server.toml",
            "--output",
            "custom-out",
        ],
        &[("RUST_LOG", "warn")],
    );
    assert!(
        out.status.success(),
        "update --output 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("orphan.md"),
        "尾部复核应发现 --output 目录的孤儿页，实际 stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// audit-cli-09：sync 写指纹库（generation_state.json），与 generate/update/watch
/// 并发会互相覆盖状态——纳入运行锁后撞锁必须拒绝（非 0 + 「正在运行」）。
/// 主测试进程持内核写锁，sync 子进程（独立进程）打开同一锁文件必然撞锁。
#[test]
fn test_sync_rejected_while_locked() {
    let work_dir = prepare_repo("sync_lock_conflict");

    // 锁路径对齐：子进程 cwd=work_dir、config 无 output.dir → output_dir()
    // 回退 .code-repo-wiki 相对 cwd = work_dir/.code-repo-wiki
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(&work_dir, &["sync", "--config", "mock-server.toml"], &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "sync 撞内核锁应拒绝，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("正在运行"),
        "撞锁报错应含「正在运行」提示，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card generate spec / tech-stack：项目级卡片重生成（分别写 project/spec.md
/// 与 project/tech-stack.md）。tech-stack 确定性解析清单（零 LLM）；
/// spec 无规约文件/notes 时不生成（输入驱动防幻觉）。
#[test]
fn test_card_generate_project_card() {
    let work_dir = unique_dir("card_project");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(work_dir.join("src")).unwrap();
    std::fs::write(
        work_dir.join("src").join("main.rs"),
        "pub fn main_fn() {}\n",
    )
    .unwrap();
    std::fs::write(
        work_dir.join("Cargo.toml"),
        "[package]\nname=\"cli-demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
    )
    .unwrap();
    // mock provider 配置（tech-stack 确定性解析不需要 LLM；output.dir 硬编码
    // .code-repo-wiki，v30 语义）
    let config = r#"
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
"#;
    std::fs::write(work_dir.join("mock.toml"), config).unwrap();

    // 1. card generate tech-stack：确定性解析 Cargo.toml → project/tech-stack.md
    let out = run_bin_with_envs(
        &work_dir,
        &["card", "generate", "tech-stack", "--config", "mock.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "card generate tech-stack 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stack_path = work_dir
        .join(".code-repo-wiki")
        .join("cards")
        .join("zh")
        .join("project")
        .join("tech-stack.md");
    let stack = std::fs::read_to_string(&stack_path).unwrap_or_else(|_| {
        panic!(
            "project/tech-stack.md 应生成, 实际: {}",
            stack_path.display()
        )
    });
    assert!(
        stack.contains("serde@1.0"),
        "tech-stack 卡应含解析出的依赖, 实际:\n{stack}"
    );

    // 2. card generate spec：无规约文件 → 不生成（防幻觉，退出码 0）
    let out = run_bin_with_envs(
        &work_dir,
        &["card", "generate", "spec", "--config", "mock.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "card generate spec 无规约文件应成功退出（提示未生成），stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !work_dir
            .join(".code-repo-wiki")
            .join("cards")
            .join("zh")
            .join("project")
            .join("spec.md")
            .exists(),
        "无规约文件时 Spec 卡不应生成（防幻觉）"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// audit-cli-04：card --root 需 global=true——`card generate <module> --root X`
/// 在动作子命令后传参可解析（与 --wait/--skip-if-locked 同构）。修复前 --root
/// 仅卡级，动作后传报 unexpected argument（跨 cwd 运行静默失败）。
#[test]
fn test_card_generate_root_after_action_parses() {
    let work_dir = prepare_repo("card_root_global");

    let out = run_bin_with_envs(
        &work_dir,
        &["generate", "--config", "mock-server.toml"],
        &[],
    );
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cards_dir = work_dir.join(".code-repo-wiki").join("cards").join("zh");
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
    let root_str = work_dir.to_str().unwrap();

    // --root 放在动作子命令之后：修复前 clap 解析期拒绝（unexpected argument）
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "generate",
            &module,
            "--root",
            root_str,
            "--config",
            "mock-server.toml",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "card generate --root 动作后传参应可解析并成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
