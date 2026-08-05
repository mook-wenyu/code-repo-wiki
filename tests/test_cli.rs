//! CLI 集成测试：通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制
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
use common::{copy_dir, mock_llm_server, openai_compatible_config, run_bin_with_envs, unique_dir};
use std::path::{Path, PathBuf};

/// 最小可用配置：LLM 指向本地 mock server，增量/搜索关闭，输出到绝对临时路径
/// （v19 t04：output.dir 用绝对路径，消除 cwd 依赖的泄漏隐患）
fn minimal_config(port: u16, out_dir: &Path) -> String {
    let cfg = openai_compatible_config(port, out_dir.to_str().unwrap());
    // 与 helper 的差异：搜索段关闭（helper 无 search 段，默认开）
    format!("{cfg}\n[search]\nenabled = false\nindex_dir = \".search\"\ndefault_engine = \"text\"\ndefault_top_k = 10\n")
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
    std::fs::write(
        work_dir.join("mock-server.toml"),
        minimal_config(port, &work_dir.join("wiki")),
    )
    .unwrap();
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
    let out = run_bin_with_envs(&work_dir, &["uninstall-from-opencode"], envs);
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
    let out = run_bin_with_envs(&work_dir, &["uninstall-from-opencode", "--force"], envs);
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

    let out = run_bin_with_envs(&work_dir, &["generate", "--config", "mock-server.toml", "--progress-json"], &[]);
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

/// update --progress-json（P2-11）：增量更新命令同样输出 JSONL 进度事件
///（U08 给 update 补上 progress 输出后一直无冒烟覆盖——generate 已有
/// test_progress_json_cli，update 的 progress-json 分支必须同等验证：
/// 事件非空、以 done=100 结束。增量禁用/非 git 时 update 回退全量路径，
/// run_pipeline_with_progress 的进度事件照常输出，断言不依赖具体模式）
#[test]
fn test_update_progress_json_cli() {
    let work_dir = prepare_repo("update_progress_json");

    // 先全量 generate 建立基线（增量路径需要产物与状态）
    let gen_out = run_bin_with_envs(&work_dir, &["generate", "--config", "mock-server.toml"], &[]);
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

    let out = run_bin_with_envs(&work_dir, &["update", "--config", "mock-server.toml", "--progress-json"], &[]);
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
    assert!(!events.is_empty(), "update 应输出进度事件，实际 stdout: {stdout}");
    assert_eq!(events.last().unwrap().0, "done", "末个事件应为 done");
    assert_eq!(events.last().unwrap().1, 100, "末个事件 progress 应为 100");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card 子命令集成：generate 产出卡片后，card modify 修改卡片文件内容
#[test]
fn test_card_cli_commands() {
    let work_dir = prepare_repo("card");

    // 1. 全量 generate（mock LLM 返回卡片 JSON）→ 卡片文件落盘
    let out = run_bin_with_envs(&work_dir, &["generate", "--config", "mock-server.toml"], &[]);
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
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card", "modify", &module,
            "--instruction", "补充一段总结",
            "--config", "mock-server.toml",
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

    let out = run_bin_with_envs(&work_dir, &["generate", "--config", "mock-server.toml"], &[]);
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
    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card", "modify", &module,
            "--instruction", "补充总结",
            "--reference", "missing.md",
            "--config", "mock-server.toml",
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
            "card", "modify", &module,
            "--instruction", "补充总结",
            "--reference", "refs.md",
            "--config", "mock-server.toml",
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

    let out = run_bin_with_envs(
        &work_dir,
        &["export", "--config", "mock-server.toml"],
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
    let cfg = openai_compatible_config(port, work_dir.join("wiki").to_str().unwrap());
    let config = format!("{cfg}[search]\nenabled = true\nindex_dir = \".search\"\ndefault_engine = \"hybrid\"\ndefault_top_k = 10\n");
    std::fs::write(work_dir.join("mock-server.toml"), config).unwrap();

    let gen_out = run_bin_with_envs(&work_dir, &["generate", "--config", "mock-server.toml"], &[]);
    assert!(
        gen_out.status.success(),
        "generate 应成功, stderr: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let out = run_bin_with_envs(
        &work_dir,
        &["search", "-q", "authenticate", "-k", "3", "--engine", "hybrid", "--json", "--config", "mock-server.toml"],
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
    let callers = hit.get("callers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    assert!(
        !callers.is_empty(),
        "authenticate 的调用链补全应含调用者 main, 实际 callers: {callers:?} (hit: {hit})"
    );
    assert!(
        hit.get("callees").is_some(),
        "命中结果应含 callees 字段"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// lint 命令:干净产物通过(exit 0);人为制造孤儿页/断链后失败(非 0)并列出问题。
/// 注:直接构造产物 fixture(不依赖 generate 的 output.dir 嵌套语义),保证断言确定性。
#[test]
fn test_lint_detects_issues_in_artifacts() {
    let work_dir = unique_dir("lint");
    let _ = std::fs::remove_dir_all(&work_dir);
    // 构造"干净"产物:单个 wiki 页 + 目录页(链接指向 core → core 有入链非孤儿)。
    // 产物布局遵循 render_all 规则:config.output.dir 下再建 wiki/{lang}/ 子目录
    let wiki = work_dir.join("wiki").join("wiki").join("zh");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(wiki.join("core.md"), "# Core\n\n模块页\n").unwrap();
    std::fs::write(
        work_dir.join("wiki").join("_toc.md"),
        "# 目录\n\n- [Core](wiki/zh/core.md)\n",
    )
    .unwrap();
    std::fs::write(work_dir.join("mock-server.toml"), "\
[scope]
include = [\"**/*.rs\"]
exclude = []

[output]
dir = \"wiki\"

[llm]
provider = \"mock\"
model = \"mock\"
base_url = \"x\"
api_key = \"mock\"
api_key_env = \"\"
max_concurrent = 1

[incremental]
enabled = false
strategy = \"git-diff\"

[search]
enabled = false
index_dir = \".search\"
default_engine = \"text\"
default_top_k = 10
").unwrap();

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
        &["ast-search", "definitely_missing", "--config", "mock-server.toml"],
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
