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
//! sample-repo 自带 config.toml 为 provider="mock"（A13 已消除触网隐患）；
//! prepare_repo 仍统一改写为 mock provider + output.dir 指向仓库内 .repo-wiki。

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::{copy_dir, mock_config, run_bin, run_bin_with_envs, unique_dir};

/// 冒烟测试共用 search 段：search 测试需要文本索引
/// （mock_config helper 已含 scope/output/llm/incremental 段，
/// v19 t04 起 output.dir 绝对路径化，不依赖进程 cwd）
const SEARCH_SECTION: &str = r#"
[search]
enabled = true
index_dir = ".search"
default_engine = "text"
default_top_k = 10
"#;

/// 复制 sample-repo 到唯一临时目录并改写 config.toml（mock provider，不触网），返回工作目录
fn prepare_repo(tag: &str) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    // v19 t04：output.dir 指向仓库内 .repo-wiki（对齐 CLI 默认约定），
    // 以绝对路径写入，杜绝 cwd 依赖导致的产物泄漏
    let config = format!(
        "{}{SEARCH_SECTION}",
        mock_config(&work_dir.join(".repo-wiki").to_string_lossy())
    );
    std::fs::write(work_dir.join("config.toml"), config).unwrap();
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

/// v17 t03：init 幂等保护——已存在的配置文件不被缺省链路径覆盖（数据破坏
/// 修复：v17 审计发现原实现无条件 write 覆盖用户配置）；显式 path 保持
/// 覆盖语义；--force 两分支都强制重写
#[test]
fn test_init_preserves_existing_config() {
    let work_dir = unique_dir("init_preserve");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(work_dir.join(".repo-wiki")).unwrap();
    // 预置用户自定义配置（内容哨兵：与默认模板区分）
    let custom = "[llm]\nprovider = \"anthropic\"\nmodel = \"claude-test\"\n";
    std::fs::write(work_dir.join(".repo-wiki").join("config.toml"), custom).unwrap();

    // 1. init 无参（缺省链命中项目级）：跳过不覆盖，用户内容保留
    let out = run_bin(&work_dir, &["init"]);
    assert!(
        out.status.success(),
        "init 跳过已存在配置应退出码 0，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(work_dir.join(".repo-wiki").join("config.toml")).unwrap();
    assert_eq!(content, custom, "已存在的配置不得被覆盖");

    // 2. --force：强制重写为默认模板
    let out = run_bin(&work_dir, &["init", "--force"]);
    assert!(out.status.success(), "--force 应成功: {}", String::from_utf8_lossy(&out.stderr));
    let content = std::fs::read_to_string(work_dir.join(".repo-wiki").join("config.toml")).unwrap();
    assert_ne!(content, custom, "--force 应重写为默认模板");
    assert!(content.contains("[llm]"), "默认模板应含 [llm] 段");

    // 3. 显式 path：保持覆盖语义（用户明确意图，不保护）
    std::fs::write(work_dir.join("custom.toml"), custom).unwrap();
    let out = run_bin(&work_dir, &["init", "custom.toml"]);
    assert!(out.status.success(), "显式 path init 应成功: {}", String::from_utf8_lossy(&out.stderr));
    let content = std::fs::read_to_string(work_dir.join("custom.toml")).unwrap();
    assert_ne!(content, custom, "显式 path 应覆盖为用户意图（重置语义）");

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
    // root 统一后产物路径绝对化（v17 F 组）：状态键为绝对路径，直接匹配
    let key = page.to_string_lossy().to_string();
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

/// T0.8 search 条数接线：v22 起 default_top_k 硬编码为 10（schema.rs 常量），
/// 配置键已移除；未传 -k 时恒用硬编码默认（≤10），显式 -k 5 覆盖默认
#[test]
fn test_search_top_k_falls_back_to_config() {
    let work_dir = prepare_repo("search_topk");

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功（构建文本索引），stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 不传 -k：结果数受硬编码默认 10 约束
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
    assert!(hits.len() <= 10, "未传 -k 应回退硬编码默认 10，实际 {} 条", hits.len());

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

/// v17 t06：mock provider 生成的产物页带占位页脚标注（防误读为真实文档）
#[test]
fn test_mock_footer_marks_placeholder_pages() {
    let work_dir = prepare_repo("mock_footer");
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 遍历产物 wiki 页面，断言页脚标注存在
    let wiki_dir = work_dir.join(".repo-wiki").join("wiki").join("zh");
    let mut found = 0;
    for entry in std::fs::read_dir(&wiki_dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).unwrap();
        assert!(
            content.contains("<!-- 本页由 mock provider 生成，非真实内容 -->"),
            "产物页应有 mock 占位页脚: {}",
            entry.path().display()
        );
        found += 1;
    }
    assert!(found > 0, "应至少有一个产物页面");
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v17 t07：lint 三态退出码——0 = 干净 / 1 = 发现问题 / 2 = 工具问题（docverity 模式）
#[test]
fn test_lint_three_state_exit_codes() {
    let work_dir = prepare_repo("lint_exit");

    // 态 1：产物未生成（目录不存在）→ 无孤儿/断链 → 0
    let out = run_bin(&work_dir, &["lint", "-c", "config.toml"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "空产物应视为干净（0），stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 生成产物（干净态）
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(out.status.success(), "generate 应成功");
    let out = run_bin(&work_dir, &["lint", "-c", "config.toml"]);
    assert_eq!(out.status.code(), Some(0), "干净产物应为 0: {}", String::from_utf8_lossy(&out.stdout));

    // 态 2：写入孤儿页 → 发现问题 → 1
    let orphan = work_dir.join(".repo-wiki").join("wiki").join("zh").join("orphan.md");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, "# 孤儿页\n").unwrap();
    let out = run_bin(&work_dir, &["lint", "-c", "config.toml"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "孤儿页应报问题（1），stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("orphan.md"),
        "应指出孤儿页路径: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // 态 3：配置加载失败 → 工具问题 → 2（不掩盖绿构建）
    let out = run_bin(&work_dir, &["lint", "-c", "missing-config.toml"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "配置失败应为 2，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v17 t07：update --dry-run 只做变更分析预览，不执行生成（无副作用）
#[test]
fn test_update_dry_run_lists_changes_without_generating() {
    let work_dir = prepare_repo("dry_run");
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(out.status.success(), "generate 应成功");
    let wiki_dir = work_dir.join(".repo-wiki").join("wiki").join("zh");
    let snapshot: Vec<(String, String)> = std::fs::read_dir(&wiki_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();

    // 修改一个源文件，触发增量变更分析
    let src_file = work_dir.join("src").join("lib.rs");
    std::fs::write(&src_file, "// 变更\npub fn touched() {}\n").unwrap();

    let out = run_bin(&work_dir, &["update", "-c", "config.toml", "--dry-run"]);
    assert!(out.status.success(), "--dry-run 应成功: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--dry-run:"), "应输出预览头: {stdout}");
    assert!(stdout.contains("未执行生成"), "应声明未执行生成: {stdout}");
    assert!(stdout.contains("个文件变更"), "应报告变更文件数: {stdout}");

    // 无副作用：产物未被改写
    let after: Vec<(String, String)> = std::fs::read_dir(&wiki_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();
    assert_eq!(snapshot, after, "--dry-run 不得改写任何产物");

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
    // watch 监听根 = scope.include[0] 的通配符前目录；mock_config 的 include 是 "**/*.rs"，
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
/// N7 回归：--root 指定不存在的目录时显式报错（此前静默通过，
/// 扫描空集/产物静默为空）
#[test]
fn test_root_missing_dir_errors() {
    let work_dir = prepare_repo("root_missing");
    let missing = work_dir.join("no-such-dir");

    let out = run_bin(&work_dir, &["generate", "-c", "config.toml", "--root", missing.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "--root 不存在应非 0 退出码, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("目录不存在"), "应报目录不存在, 实际: {combined}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

// ==================== E 组：默认配置链（v13） ====================

/// E 组：无 --config 时默认配置链取项目级 .repo-wiki/config.toml（项目级优先）。
/// 项目级存在时不触达全局目录（resolve 先查项目级），测试无需隔离 APPDATA。
#[test]
fn test_default_config_chain_prefers_project_config() {
    let work_dir = unique_dir("e-chain");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(work_dir.join(".repo-wiki")).unwrap();
    std::fs::write(
        work_dir.join(".repo-wiki").join("config.toml"),
        mock_config(&work_dir.join(".repo-wiki").to_string_lossy()),
    )
    .unwrap();

    // 不带 --config 运行 status：应命中项目级配置（未生成提示 + 配置路径）
    let out = run_bin(&work_dir, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.replace('\\', "/").contains(".repo-wiki/config.toml"),
        "默认链应命中项目级配置，实际: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v17 F 组（t09 实测修复）：增量更新不得误删未受影响模块的页面——
/// 增量只重新生成受影响模块，未受影响模块的旧页面是有效产物（源码仍在），
/// 清理须跳过（修复前 src_fs.md 被误删导致 6 页断链）
#[test]
fn test_incremental_update_keeps_unaffected_module_pages() {
    let work_dir = prepare_repo("incr_preserve");
    init_git(&work_dir, "init");
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pages_before = collect_page_stems(&work_dir);
    assert!(pages_before.len() >= 3, "应有全部模块页: {pages_before:?}");

    // 只做实现级变更（函数体内加代码行，不新增实体/签名/依赖边）：
    // 增量稳定触发"单模块受影响"；新增实体会让小样本（3 文件）社区检测
    // 翻转（Leiden 聚类边界，t09 调查确认），页面归属变化与本次修复目标
    // （未受影响模块页面保留）无关，测试须避开
    let auth = work_dir.join("src").join("auth.rs");
    let content = std::fs::read_to_string(&auth).unwrap();
    std::fs::write(
        &auth,
        content.replace(
            "if username == \"admin\" && password == \"secret\" {",
            "if username == \"admin\" && password == \"secret\" {\n        let _debug = 0; // 实现级变更",
        ),
    )
    .unwrap();
    commit_all(&work_dir, "change auth");
    let out = run_bin(&work_dir, &["update", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "增量 update 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 增量后：全部旧页面必须仍在（未受影响模块页面不得被清理）
    let pages_after = collect_page_stems(&work_dir);
    for p in &pages_before {
        assert!(
            pages_after.contains(p),
            "增量更新误删了未受影响模块的页面: {p}"
        );
    }
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// 收集产物 wiki 页面的文件 stem 集合（wiki/zh/*.md，不含合成页）
fn collect_page_stems(work_dir: &Path) -> std::collections::HashSet<String> {
    std::fs::read_dir(work_dir.join(".repo-wiki").join("wiki").join("zh"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            e.path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// 用 git2 在目录初始化仓库并提交全部文件（增量 git-diff 基线）
fn init_git(dir: &Path, message: &str) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]).unwrap();
}

/// 提交当前工作树变更（供增量场景构造）
fn commit_all(dir: &Path, message: &str) {
    let repo = git2::Repository::open(dir).unwrap();
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
        .unwrap();
}

/// v21 A 组（t03）：no-op stdout 契约——外部 AI Coding Agent 以 stdout 为
/// 事实源，无变更跳过时必须打印明确消息且不得再打印「增量更新完成」。
/// 场景：generate 建立基线后，同一 commit 下直接 update（should_skip_noop
/// 为真，lib.rs 早退返回空结果）。
#[test]
fn test_update_noop_stdout_contract() {
    let work_dir = prepare_repo("noop_stdout");
    init_git(&work_dir, "init");
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 同 head 无任何变更：update 必须走 no-op 早退
    let out = run_bin(&work_dir, &["update", "-c", "config.toml"]);
    assert!(out.status.success(), "no-op update 应成功退出");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("无文件变更，跳过更新（no-op）"),
        "stdout 必须包含跳过消息: {stdout:?}"
    );
    assert!(
        !stdout.contains("增量更新完成"),
        "no-op 不得打印「增量更新完成」: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v21 A 组（t04a）：仓库已存在 AGENTS.md 时生成端跳过注入（保护人工维护），
/// 但必须 warn 提示（含补救路径 install-wiki），不得静默。
#[test]
fn test_generate_warns_when_agents_md_exists() {
    let work_dir = prepare_repo("agents_md_warn");
    // 预置人工维护的 AGENTS.md（内容哨兵：事后必须原样保留）
    let agents = work_dir.join("AGENTS.md");
    std::fs::write(&agents, "# 人工维护的 AGENTS.md\n\n自定义内容\n").unwrap();
    // RUST_LOG=warn 捕获 tracing 警告（run_bin 默认 RUST_LOG=off 关日志）
    let out = run_bin_with_envs(&work_dir, &["generate", "-c", "config.toml"], &[("RUST_LOG", "warn")]);
    assert!(
        out.status.success(),
        "generate 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("跳过注入"),
        "已存在 AGENTS.md 时必须 warn 提示: {stderr:?}"
    );
    assert!(
        stderr.contains("install-wiki"),
        "warn 必须给出补救路径: {stderr:?}"
    );
    // 人工内容不得被覆盖
    let content = std::fs::read_to_string(&agents).unwrap();
    assert!(content.contains("自定义内容"), "人工 AGENTS.md 被覆盖: {content}");
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v17 t08：doctor 端到端——mock 配置全过（网络跳过）退出码 0；
/// 缺失配置退出码 1
#[test]
fn test_doctor_reports_and_exits() {
    let work_dir = prepare_repo("doctor");

    // mock 配置：五项全过（网络项标注跳过，不算失败）
    let out = run_bin(&work_dir, &["doctor", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "mock 配置 doctor 应全过，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in ["配置", "产物目录可写", "输出目录", "LLM Key", "网络"] {
        assert!(stdout.contains(name), "应输出检查项 {name}: {stdout}");
    }
    assert!(stdout.contains("mock provider：跳过网络检查"), "应标注网络跳过: {stdout}");

    // 配置缺失 → 失败退出码 1
    let out = run_bin(&work_dir, &["doctor", "-c", "nope.toml"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "配置缺失应退出码 1，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// v21 E 组（t10）：bench-manifest 清单批量跑分端到端冒烟——
/// 两个本地仓库 mock 生成，输出仓库×维度矩阵（含失败行标注）
#[test]
fn test_bench_manifest_smoke() {
    let base = unique_dir("benchman");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    // 两个小仓库 + 一个不存在路径（验证失败标注）
    for name in ["repo-a", "repo-b"] {
        let r = base.join(name);
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::fs::write(r.join("src").join("main.rs"), "pub fn alpha() {}\n").unwrap();
    }
    let manifest_path = base.join("manifest.txt");
    std::fs::write(
        &manifest_path,
        format!(
            "{}\n{}\n{}\n",
            base.join("repo-a").display(),
            base.join("repo-b").display(),
            base.join("missing-repo").display()
        ),
    )
    .unwrap();
    // 模板配置：mock provider（与 sample-repo 相同的生成语义，全程不触网）
    let config_path = base.join("config.toml");
    std::fs::write(
        &config_path,
        mock_config(&base.join("work").join("tpl-out").to_string_lossy()),
    )
    .unwrap();

    let out = run_bin(
        &base,
        &[
            "bench-manifest",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "bench-manifest 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("清单跑分报告"), "应输出报告标题: {stdout}");
    assert!(stdout.contains("| repo-a |"), "矩阵应含 repo-a 行: {stdout}");
    assert!(stdout.contains("**失败**"), "缺失路径应标注失败: {stdout}");

    let _ = std::fs::remove_dir_all(&base);
}
