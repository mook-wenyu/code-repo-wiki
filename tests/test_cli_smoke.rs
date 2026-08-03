//! CLI 未覆盖子命令冒烟测试（演进计划 T4.2）
//!
//! 通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制，覆盖：
//! 1. status：就绪状态输出
//! 2. note：Karpathy log 追加式知识记录（日期节 + 序号递增）
//! 3. init：默认配置生成（schema 8 段键，无死键 [project]/[generate]）
//! 4. sync：Git 内容合入指纹库（工作区手工修改不被覆盖、指纹以工作区为准）
//! 5. search -e text：文本引擎 JSON 命中
//!    （semantic 引擎需 embed 启用 + 真实 embedding API key，违反"generate 不触网"
//!    约束，仅以 test_search_semantic_without_embed_errors 断言其无索引时的边界报错）
//! 6. watch 不做真实监听（阻塞型命令），跳过
//!
//! 每个测试使用独立临时目录（进程 pid + 自增序号）避免并行冲突；
//! LLM 使用内置 mock provider（schema 原生支持，返回固定摘要，全程无网络调用——
//! 比复制 test_cli.rs 的 mock HTTP server 更轻，产物内容与之一致）。
//!
//! sample-repo 自带 config.toml 为 provider="openai"（base_url 缺省会触网），
//! 因此 prepare_repo 一律改写为 mock provider + output.dir 指向仓库内 .repo-wiki。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 冒烟测试共用配置：内置 mock LLM provider（generate 不触网），
/// output.dir 指向仓库内 .repo-wiki（对齐 CLI 默认约定）；
/// incremental 开启（sync 测试需要指纹状态库）、search 开启（search 测试需要文本索引）。
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

/// 生成唯一临时目录（进程 id + 自增序号）
fn unique_dir(name: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("repo_wiki_smoke_{}_{}_{}", name, std::process::id(), seq))
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

/// 在指定目录下执行 repo-wiki 二进制，返回完整输出
fn run_bin(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_repo-wiki"));
    cmd.args(args)
        .current_dir(dir)
        .env("RUST_LOG", "off") // 关闭 tracing 日志，保证 stdout 只有业务输出
        .env_remove("OPENAI_API_KEY"); // 避免宿主机真实 Key 被误用
    cmd.output().expect("执行 repo-wiki 二进制失败")
}

/// 复制 sample-repo 到唯一临时目录并改写 config.toml（mock provider，不触网），返回工作目录
fn prepare_repo(tag: &str) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    std::fs::write(work_dir.join("config.toml"), TEST_CONFIG).unwrap();
    work_dir
}

// ==================== 测试用例 ====================

/// status：无产物时报告未生成；generate 后就绪并输出页面/卡片统计与配置文件路径
/// （main.rs Status 分支行为，T1）
#[test]
fn test_status_reports_ready() {
    let work_dir = prepare_repo("status");

    // 未生成时（无产物）：提示运行 generate
    let out = run_bin(&work_dir, &["status", "-c", "config.toml"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status 应成功退出，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("Wiki 状态: 未生成"),
        "无产物时应提示未生成，实际 stdout: {stdout}"
    );
    assert!(
        stdout.contains("配置文件: config.toml"),
        "应输出配置文件路径，实际 stdout: {stdout}"
    );

    // generate 后：就绪 + 页面/卡片统计
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run_bin(&work_dir, &["status", "-c", "config.toml"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "generate 后 status 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("Wiki 状态: 就绪"),
        "应输出就绪状态行，实际 stdout: {stdout}"
    );
    assert!(
        stdout.contains("配置文件: config.toml"),
        "应输出配置文件路径，实际 stdout: {stdout}"
    );
    assert!(
        stdout.contains("页面:") && stdout.contains("卡片:"),
        "应输出页面/卡片统计，实际 stdout: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// note：追加式 Karpathy log（{output.dir}/wiki/zh/_log.md），
/// 当天节内序号递增，历史不覆盖
#[test]
fn test_note_appends_karpathy_log() {
    let work_dir = prepare_repo("note");

    // 第一条：新建日期节并编号 1
    let out = run_bin(&work_dir, &["note", "第一条测试记录", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "note 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log_path = work_dir.join(".repo-wiki").join("wiki").join("zh").join("_log.md");
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("_log.md 应存在 {}: {}", log_path.display(), e));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(
        log.contains(&format!("## {today}")),
        "应含当天日期节，实际: {log}"
    );
    assert!(
        log.contains("- 1. 第一条测试记录"),
        "第一条应编号 1，实际: {log}"
    );

    // 第二条：同日期节内序号递增为 2，且不产生第二条日期节
    let out = run_bin(&work_dir, &["note", "第二条测试记录", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "第二条 note 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("- 2. 第二条测试记录"),
        "第二条应编号 2，实际: {log}"
    );
    assert_eq!(
        log.matches(&format!("## {today}")).count(),
        1,
        "同一日期节不应重复，实际: {log}"
    );
    assert_eq!(
        log.lines().filter(|l| l.trim_start().starts_with("- ")).count(),
        2,
        "应恰好 2 条记录，实际: {log}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// init：生成 schema 对齐的默认配置（8 段键，无死键 [project]/[generate]），
/// 且产物可被真实二进制加载
#[test]
fn test_init_writes_schema_config() {
    let work_dir = unique_dir("init");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();

    // 路径含子目录：验证父目录自动创建（create_default_config 行为）
    let out = run_bin(&work_dir, &["init", "configs/default.toml"]);
    assert!(
        out.status.success(),
        "init 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg_path = work_dir.join("configs").join("default.toml");
    let content = std::fs::read_to_string(&cfg_path)
        .unwrap_or_else(|e| panic!("配置应生成 {}: {}", cfg_path.display(), e));
    // schema 8 段键逐一存在
    for section in ["[wiki]", "[scope]", "[llm]", "[embed]", "[output]", "[incremental]", "[search]", "[plan]"] {
        assert!(
            content.contains(section),
            "默认配置应含 {section} 段，实际:\n{content}"
        );
    }
    // 无死键：schema 无 [project]/[generate]，生成的配置不得出现
    assert!(
        !content.contains("[project]"),
        "默认配置不应含死键 [project]，实际:\n{content}"
    );
    assert!(
        !content.contains("[generate]"),
        "默认配置不应含死键 [generate]，实际:\n{content}"
    );

    // 产物可被真实二进制加载（status 只加载配置不触网）
    let out = run_bin(&work_dir, &["status", "-c", "configs/default.toml"]);
    assert!(
        out.status.success(),
        "init 产物应可被加载，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// sync：generate 后手工修改 wiki 页，sync 以工作区内容合入指纹库——
/// 页面内容保留（不触发 LLM 重生成）、状态文件存在、
/// doc_fingerprints 中该页指纹更新为修改后内容（工作区为准）
#[test]
fn test_sync_merges_manual_edit_into_state() {
    let work_dir = prepare_repo("sync");

    // 1. 全量 generate（内置 mock provider，不触网）
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. 选一个 wiki 页手工修改（取 wiki/zh 下任一 .md，动态选取避免对模块名耦合）
    let wiki_zh = work_dir.join(".repo-wiki").join("wiki").join("zh");
    let page = std::fs::read_dir(&wiki_zh)
        .unwrap_or_else(|e| panic!("wiki/zh 目录应存在 {}: {}", wiki_zh.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .min()
        .expect("generate 后 wiki/zh 下应有页面文件");
    let state_path = work_dir.join(".repo-wiki").join(".state").join("generation_state.json");
    let state_before = std::fs::read_to_string(&state_path)
        .unwrap_or_else(|e| panic!("generate 应写状态文件 {}: {}", state_path.display(), e));
    let before: serde_json::Value = serde_json::from_str(&state_before).unwrap();
    let key = page.strip_prefix(&work_dir).unwrap().to_string_lossy().to_string();
    let fp_before = before["doc_fingerprints"][&key].as_str()
        .unwrap_or_else(|| panic!("状态应含文档指纹 {key}: {state_before}"));

    // 3. 手工修改页面（与生成内容显著不同）
    let manual = "# 手工编辑标记\n\nsync 后应保留此内容\n";
    std::fs::write(&page, manual).unwrap();

    // 4. sync：Git 内容合入指纹库，不触发 LLM
    let out = run_bin(&work_dir, &["sync", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "sync 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 内容保留（sync 不重写产物）
    assert_eq!(
        std::fs::read_to_string(&page).unwrap(),
        manual,
        "sync 后手工修改内容应原样保留"
    );
    // 指纹库更新：该页指纹从生成值变为修改后内容的 SHA256（工作区为准）
    let state_after = std::fs::read_to_string(&state_path).unwrap();
    let after: serde_json::Value = serde_json::from_str(&state_after).unwrap();
    let fp_after = after["doc_fingerprints"][&key].as_str()
        .unwrap_or_else(|| panic!("sync 后状态应含文档指纹 {key}: {state_after}"));
    assert_ne!(fp_before, fp_after, "sync 应更新 {key} 的指纹（工作区内容为准）");
    let expected = repo_wiki::incremental::state::GenerationState::compute_file_fingerprint(&page).unwrap();
    assert_eq!(fp_after, expected, "指纹应等于修改后文件内容 SHA256");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// search text 引擎：generate 构建文本索引后，-e text --json 输出合法 JSON
/// 且命中 authenticate（auth.rs 定义）
#[test]
fn test_search_text_engine_json() {
    let work_dir = prepare_repo("search_text");

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功（构建文本索引），stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        work_dir.join(".repo-wiki").join(".search").join("text_index.db").exists(),
        "文本索引应生成"
    );

    let out = run_bin(
        &work_dir,
        &["search", "-q", "authenticate", "-k", "3", "-e", "text", "--json", "-c", "config.toml"],
    );
    assert!(
        out.status.success(),
        "search text 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("应输出合法 JSON: {e}\n实际: {stdout}"));
    assert!(!hits.is_empty(), "应至少一个命中，实际: {stdout}");
    let auth_hit = hits.iter().find(|h| {
        h.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n == "authenticate")
    });
    assert!(auth_hit.is_some(), "应命中 authenticate，实际: {stdout}");
    assert!(
        auth_hit.unwrap().get("file").and_then(|f| f.as_str()).is_some_and(|f| f.contains("auth.rs")),
        "命中应定位到 auth.rs，实际: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// T0.8 search.default_top_k 接线：配置 default_top_k=3 且 CLI 不传 -k 时，
/// search 结果数回退到配置值（≤3）；对照 -k 5 显式传参不受配置影响（>3）
#[test]
fn test_search_top_k_falls_back_to_config() {
    let work_dir = prepare_repo("search_topk");
    // 覆写配置：search.default_top_k 从 10 改为 3（T0.8 接线验证）
    let cfg = std::fs::read_to_string(work_dir.join("config.toml")).unwrap();
    std::fs::write(
        work_dir.join("config.toml"),
        cfg.replace("default_top_k = 10", "default_top_k = 3"),
    )
    .unwrap();

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功（构建文本索引），stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 不传 -k：结果数受配置 default_top_k=3 约束
    let out = run_bin(
        &work_dir,
        &["search", "-q", "pub", "-e", "text", "--json", "-c", "config.toml"],
    );
    assert!(
        out.status.success(),
        "search 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hits: Vec<serde_json::Value> = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .unwrap_or_else(|e| panic!("应输出合法 JSON: {e}\n实际: {}", String::from_utf8_lossy(&out.stdout)));
    assert!(!hits.is_empty(), "应至少一个命中，实际: {:?}", hits);
    assert!(hits.len() <= 3, "未传 -k 应回退配置 default_top_k=3，实际 {} 条", hits.len());

    // 对照：显式 -k 5 应超过配置值（证明候选多于 3，回退确实生效）
    let out = run_bin(
        &work_dir,
        &["search", "-q", "pub", "-k", "5", "-e", "text", "--json", "-c", "config.toml"],
    );
    let hits5: Vec<serde_json::Value> = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(hits5.len() > 3, "显式 -k 5 应返回 5 条（候选多于 3），实际 {} 条", hits5.len());

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// search semantic 引擎边界：embed 未启用时无语义索引，
/// 显式请求 semantic 应报错指引（非 0 退出码）。
/// 注：完整语义检索需 embed.enabled=true + 真实 embedding API key，
/// 违反本文件"generate 不触网"约束，故跳过（仅断言无索引时的降级报错）。
#[test]
fn test_search_semantic_without_embed_errors() {
    let work_dir = prepare_repo("search_semantic");

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = run_bin(
        &work_dir,
        &["search", "-q", "authenticate", "-e", "semantic", "--json", "-c", "config.toml"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "embed 未启用时应显式失败，输出: {combined}"
    );
    assert!(
        combined.contains("语义搜索未启用"),
        "应提示语义索引缺失并指引启用 embed，实际: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
// ==================== T5.2/T5.3：update 与 watch CLI 冒烟 ====================

/// update：generate 后修改源文件，update 命令应成功退出并只重建受影响模块
/// （演进计划 T5.2；真实差分路径由 test_incremental_git_e2e 覆盖，这里只做 CLI 冒烟）
#[test]
fn test_update_command_smoke() {
    let work_dir = prepare_repo("update_smoke");
    std::fs::create_dir_all(work_dir.join("src")).unwrap();
    std::fs::write(work_dir.join("src").join("extra.rs"), "pub fn extra() -> u32 { 7 }\n").unwrap();

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 修改一个源文件后增量更新
    std::fs::write(work_dir.join("src").join("extra.rs"), "pub fn extra() -> u32 { 8 }\n").unwrap();
    let out = run_bin(&work_dir, &["update", "-c", "config.toml"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "update 应成功退出，输出: {combined}"
    );
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// watch：真实监听 + 文件变更驱动增量更新（演进计划 T5.3）
///
/// 阻塞型命令的测试模式：spawn 子进程 → 轮询 try_wait（100ms 间隔）→
/// 写入源文件触发监听 → 轮询产物出现（≤5s）→ kill 子进程。
/// 子进程持有 stdout 管道，轮询期间必须持续排空管道，否则管道缓冲
/// 填满后子进程阻塞、无法继续处理事件（watch 单测的死锁陷阱）。
#[test]
fn test_watch_command_detects_change() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let work_dir = prepare_repo("watch_smoke");
    // watch 监听根 = scope.include[0] 的通配符前目录；TEST_CONFIG 的 include 是 "**/*.rs"，
    // 监听根为仓库根，src/extra.rs 在其下
    std::fs::create_dir_all(work_dir.join("src")).unwrap();
    let extra = work_dir.join("src").join("extra.rs");
    std::fs::write(&extra, "pub fn extra() -> u32 { 7 }\n").unwrap();

    // 先全量生成一次（增量更新才有基线）
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(out.status.success(), "generate 应成功");
    assert!(
        work_dir.join(".repo-wiki").join("wiki").join("zh").join("api.md").exists(),
        "基线产物应存在"
    );

    // 启动 watch（阻塞监听）
    let mut child = Command::new(env!("CARGO_BIN_EXE_repo-wiki"))
        .args(["watch", "-c", "config.toml"])
        .current_dir(&work_dir)
        .env("RUST_LOG", "off")
        .env_remove("OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 watch 失败");
    // 排空 stdout（watch 输出量小，这里只排空 stderr；stdout 管道保持打开）
    let mut stderr_reader = BufReader::new(child.stderr.take().unwrap());
    let drain_thread = std::thread::spawn(move || {
        let mut line = String::new();
        while let Ok(n) = stderr_reader.read_line(&mut line) {
            if n == 0 { break; }
            line.clear();
        }
    });

    // 等待 watch 完成启动（监听目录就绪）后写入变更
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(&extra, "pub fn extra() -> u32 { 9 }\n").unwrap();

    // 轮询：变更应触发增量更新（产物 mtime 更新；以 api.md 的修改时间变化为信号）
    let before = std::fs::metadata(work_dir.join(".repo-wiki").join("wiki").join("zh").join("api.md"))
        .and_then(|m| m.modified())
        .ok();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut detected = false;
    while Instant::now() < deadline {
        // 每轮排空 stderr 管道（防止缓冲填满阻塞子进程）
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(m) = std::fs::metadata(work_dir.join(".repo-wiki").join("wiki").join("zh").join("api.md"))
            .and_then(|m| m.modified())
            && before.map(|b| m > b + Duration::from_millis(500)).unwrap_or(false)
        {
            detected = true;
            break;
        }
    }
    assert!(detected, "watch 应检测到文件变更并触发增量更新");

    // 收尾：kill 子进程（watch 是阻塞监听，必须显式终止）
    let _ = child.kill();
    let _ = child.wait();
    drain_thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&work_dir);
}