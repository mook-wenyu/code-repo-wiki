//! 票 14：本地方案验证（新建测试文件，仅新增内容，不修改 src/ 与既有 tests/）
//!
//! 三项验证：
//! 1. watch 端到端：监听 → 文件变更 → 增量更新 → 产物变化
//!    （tests::watch_e2e_file_change_triggers_incremental）
//! 2. insights 缓存占用实测：10 文件规模输出缓存字节数
//!    （tests::insights_cache_size_reports）
//! 3. watch 事件 ./ 前缀路径边界：记录当前行为（未相对化 + 传播不命中），
//!    不修 src，只写测试固化行为（tests::watch_path_dot_slash_prefix_boundary）
//!
//! 临时仓库构造参考（只读）tests/test_incremental_git_e2e.rs 的 build_git_repo
//! 模式：临时目录 + src 文件 + config.toml。本文件按需简化（见各测试注释）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig, WikiSection};
use code_repo_wiki::ingest::parser::{Entity, FileInsight};
use code_repo_wiki::incremental::state::GenerationState;

/// 轮询等待条件成立，90s 上限，超时 panic（watch 防抖 300ms，轮询间隔按场景 250~500ms）
///
/// 30s 在 CI（windows-latest）实测超时：watch 进程启动 + 初始生成 + 事件检测 +
/// 增量生成全链路在慢 IO runner 上超过 30s；本地/ubuntu 均在 5s 内完成。
fn wait_until(mut cond: impl FnMut() -> bool, interval: Duration, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(interval);
    }
    panic!("等待超时（90s）: {what}");
}

/// watch 端到端配置：provider=mock（不发起网络）+ FileWatch 增量策略 + 输出到临时仓库
///
/// v30+：监听根恒为仓库根（全量监听，事件按支持语言扩展名过滤）。
fn watch_config(repo: &Path) -> WikiConfig {
    WikiConfig {
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection { language: "zh".into(), guide: Default::default() },
        llm: LlmSection { provider: LlmProviderType::Mock, ..Default::default() },
        // v30：embed 默认真实阵营（百炼）且 EmbedSection 无 mock 通道——
        // 环境有 BAILIAN_API_KEY 时嵌入会真实触网拖慢全量。api_key_env=""
        // 让 resolve_api_key 立即失败（不做网络重试）→语义索引/特征聚类
        // 降级跳过，测试保持全离线（与 smoke watch 回归同因）。
        embed: code_repo_wiki::config::schema::EmbedSection {
            api_key_env: String::new(),
            ..Default::default()
        },
    }
}

fn read_opt(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 验证 1：watch 端到端全链路
///
/// 链路：spawn 线程跑 run_watch（同步阻塞：先全量生成，再 run_watch_loop
/// 死循环监听）→ 主线程轮询初始产物 → 修改 src/alpha.rs 追加新函数 →
/// 防抖 300ms 后触发增量更新 → 轮询 api.md 出现新函数名（api.md 由 graph
/// 渲染、不经 LLM，是产物变化的最可靠观测点）。
///
/// 临时仓库构造相对 build_git_repo 的简化：省略 git init——FileWatch 策略
/// 不依赖 git（get_head_commit_hash_at 非 git 仓库返回空串，状态照常保存），
/// 省略可减少失败面。
///
/// 诚实边界标注：
/// - v14 F 组已实现 Ctrl-C 优雅退出（lib.rs run_watch 内部 spawn ctrl_c
///   线程置 stop_flag，run_watch_loop 500ms 轮询退出）——但本测试进程
///   无法向自身线程注入 SIGINT（Windows 无 POSIX kill 语义），join 会
///   死锁（run_watch 阻塞在监听循环且无外部停止入口暴露）。
///   JoinHandle drop = detach，watch 线程随测试进程退出终止（同 v14
///   前行为；stop_flag 的单元级退出语义由 incremental::watch 模块测试
///   test_watch_loop_exits_on_pre_set_stop_flag 覆盖）。
/// - FileWatch 指纹比对读盘依赖相对路径 + 进程 cwd（GenerationState::
///   compute_file_fingerprint 直接 open(insight.path)），测试进程 cwd 是
///   仓库根而非临时仓库 → 全量生成时指纹表为空 → 增量阶段 is_file_changed
///   视所有文件为新文件 → 事件触发后退化为全量重生成。本测试因此验证的是
///   "监听事件 → 流水线重跑 → 产物变化"链路；changed_files 为空时的跳过
///   短路不在本测试覆盖内（不影响本测试断言的有效性：产物确实随事件更新）。
#[test]
fn watch_e2e_file_change_triggers_incremental() {
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_watch_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("src")).expect("创建临时仓库失败");
    std::fs::write(repo.join("src").join("alpha.rs"), "pub fn alpha_fn(x: u32) -> u32 { x + 1 }\n")
        .expect("写入 alpha.rs 失败");
    std::fs::write(repo.join("src").join("beta.rs"), "pub fn beta_fn(x: u32) -> u32 { x + 2 }\n")
        .expect("写入 beta.rs 失败");

    let config = watch_config(&repo);
    std::fs::write(repo.join("config.toml"), toml::to_string_pretty(&config).expect("序列化配置失败"))
        .expect("写入 config.toml 失败");

    let config_path = repo.join("config.toml");
    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());

    // run_watch 是同步阻塞函数；tokio 运行时由 get_global_runtime 惰性创建，
    // 在非 runtime 线程调用 block_on 合法（watch 回调同样如此）。
    let thread_root = root.clone();
    let thread_config_path = config_path.clone();
    let handle = std::thread::spawn(move || {
        code_repo_wiki::run_watch(Some(&thread_config_path), &thread_root).expect("run_watch 启动失败");
    });
    // 设计说明：不 join（见上方诚实边界——测试进程无法注入 SIGINT；
    // stop_flag 退出语义由模块级单测覆盖）。drop JoinHandle = detach。
    drop(handle);

    let api_path = repo.join(".code-repo-wiki").join("wiki").join("zh").join("api.md");

    // 第一步：等待初始全量生成完成（产物存在且含基线实体）
    wait_until(
        || read_opt(&api_path).is_some_and(|s| s.contains("alpha_fn")),
        Duration::from_millis(250),
        "初始全量生成产物（.code-repo-wiki/wiki/zh/api.md 含 alpha_fn）",
    );

    // 第二步：修改 src/alpha.rs，追加新函数（新增实体 → api.md 变化）。
    // 竞态说明：wait 初始产物在 run_watch 的全量阶段（监听建立之前）即可
    // 满足，此时立即改文件会落在 notify 注册窗口内（Windows 目录句柄未
    // 建立）→ 事件丢失、增量永不触发（实测复现：03:06 全量完成与监听
    // 启动同毫秒，事件未到）。固定等待 500ms 跨过注册窗口（notify 注册
    // 毫秒级，500ms 余量足够，慢机亦然）。
    std::thread::sleep(std::time::Duration::from_millis(500));
    std::fs::write(
        repo.join("src").join("alpha.rs"),
        "pub fn alpha_fn(x: u32) -> u32 { x + 1 }\npub fn alpha_fn_v2(x: u32) -> u32 { x + 100 }\n",
    )
    .expect("修改 alpha.rs 失败");

    // 第三步：等待增量更新产物（watch 防抖 300ms + 增量流水线耗时，轮询 500ms）
    wait_until(
        || read_opt(&api_path).is_some_and(|s| s.contains("alpha_fn_v2")),
        Duration::from_millis(500),
        "增量更新产物（api.md 出现 alpha_fn_v2）",
    );

    // 清理：notify 在 Windows 上用 ReadDirectoryChangesW 持有目录句柄，
    // 可能导致 remove_dir_all 失败——失败仅告警（进程退出即释放），不影响结论
    if let Err(e) = std::fs::remove_dir_all(&repo) {
        eprintln!("临时目录清理失败（watch 句柄可能未释放，进程退出即回收）: {e}");
    }
}

/// 验证 2：insights 缓存占用实测（真实仓库规模的 1/6 采样）
///
/// 10 个 .rs 文件跑 scan_and_parse_cached_at 两次（首次写缓存、二次命中复用），
/// 输出 .state/insights_cache.json 字节数。缓存条目是独立 JSON 对象
///（相对路径 + 定长 SHA256 指纹 + 完整解析结果），条目大小与文件数近似
/// 线性，真实 60 文件仓库 ≈ 10 文件实测 × 6（线性外推，测试注释与输出
/// 中均标注）。决策：测试内 10 文件版（秒级完成），真实仓库规模在测试里
/// 跑全量解析过慢。
#[test]
fn insights_cache_size_reports() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_cache_size_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");

    for i in 0..10 {
        let content = format!("pub fn m{i}(x: u32) -> u32 {{ x + {i} }}\npub struct S{i} {{ pub v: u32 }}\n");
        std::fs::write(dir.join(format!("m{i}.rs")), content).expect("写入源文件失败");
    }

    let root = code_repo_wiki::project::ProjectRoot::new(dir.clone());
    let cache_path = dir.join(".code-repo-wiki").join(".state").join("insights_cache.json");
    let empty_changed = std::collections::HashSet::new();

    // 第一次：写缓存
    let first = code_repo_wiki::ingest::scan_and_parse_cached_at(
        &root,
        &Some(cache_path.clone()),
        &empty_changed,
    )
    .expect("首次扫描解析失败")
    .insights;
    assert_eq!(first.len(), 10, "应解析出 10 个文件");

    // 第二次：缓存复用路径（不崩溃、结果一致）
    let second = code_repo_wiki::ingest::scan_and_parse_cached_at(
        &root,
        &Some(cache_path.clone()),
        &empty_changed,
    )
    .expect("缓存命中扫描失败")
    .insights;
    assert_eq!(second.len(), 10);

    let bytes = std::fs::metadata(&cache_path).expect("缓存文件应存在").len();
    assert!(bytes > 0, "缓存文件不应为空");
    // 线性外推依据：每条目为独立 JSON（路径 + 定长指纹 + 解析结果），
    // 文件数翻倍 ≈ 字节数近似翻倍（条目内无跨文件共享状态）
    println!(
        "insights_cache 实测: 10 文件 -> {} bytes（真实 60 文件仓库线性外推 ≈ {} bytes）",
        bytes,
        bytes * 6
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 验证 3：watch 事件 ./ 前缀路径边界（固化当前行为，不修 src）
///
/// 已查证（src/lib.rs:201-204，写作时行为）：
/// ```text
/// watch_list.iter().map(|p| p.strip_prefix(root.path())
///     .map(|r| r.to_path_buf()).unwrap_or_else(|_| p.clone()))
/// ```
/// strip_prefix 按路径组件比较：`./src/foo.rs` 的首组件是 CurDir，与 root
///（绝对路径）的组件不相等 → strip_prefix 失败 → unwrap_or_else 原样保留
/// `./src/foo.rs`，即「./ 前缀路径未被相对化」。绝对路径 strip_prefix 成功
/// → 相对化（候选修复方向：相对化前剥离 ./ 前缀或过滤 CurDir 组件，
/// 本文件只记录行为，不改 src）。
///
/// 影响（FileWatch 策略）：未相对化的路径一路透传到 changed_files；影响
/// 传播起点匹配（impact.rs find_start_nodes）是子串匹配
/// （norm_sep(file_path).contains(norm_sep(changed_path))），`./src/foo.rs`
/// 作为 changed_path 对 `src/foo.rs` 恒不命中 → 该路径不参与传播。功能上
/// 由指纹比对兜底（指纹命中的 insight.path 是相对形态，传播不受损），
/// 即当前行为无功能损失，但存在路径形态隐患。
#[test]
fn watch_path_dot_slash_prefix_boundary() {
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_dot_prefix_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("src")).expect("创建临时仓库失败");
    let src_file = repo.join("src").join("foo.rs");
    std::fs::write(&src_file, "pub fn foo_fn(x: u32) -> u32 { x + 1 }\n").expect("写入 foo.rs 失败");

    // ---- 1) 复现 lib.rs:201-204 的相对化表达式，确认当前行为 ----
    let root_path = repo.clone();
    let dot_slash = PathBuf::from("./src/foo.rs");
    let relativized = dot_slash
        .strip_prefix(&root_path)
        .map(|r| r.to_path_buf())
        .unwrap_or_else(|_| dot_slash.clone());
    assert_eq!(
        relativized,
        PathBuf::from("./src/foo.rs"),
        "当前行为确认：./ 前缀路径未被相对化（strip_prefix 组件比较失败，原样保留）"
    );

    let abs = src_file.clone();
    let relativized_abs = abs
        .strip_prefix(&root_path)
        .map(|r| r.to_path_buf())
        .unwrap_or_else(|_| abs.clone());
    assert_eq!(relativized_abs, PathBuf::from("src/foo.rs"), "绝对路径应被相对化");

    // ---- 2) 真实链路观测：FileWatch 增量下 watch 路径的透传与传播行为 ----
    // insight.path 用绝对路径（与真实流水线的相对形态不同，注释说明）：
    // GenerationState::compute_file_fingerprint 直接 open(path) 读盘，
    // 相对路径依赖进程 cwd（cwd 是仓库根，读不到临时文件）；绝对路径
    // 保证指纹计算与 is_file_changed 的读盘确定性，本测试只关心路径
    // 透传与子串匹配，不关心模块名派生。
    let insight = FileInsight {
        path: src_file.clone(),
        language: "rust".into(),
        entities: vec![Entity {
            name: "foo_fn".into(),
            kind: "function".into(),
            line_start: 1,
            line_end: 1,
            doc_comment: None,
            signature: Some("pub fn foo_fn(x: u32) -> u32 { x + 1 }".into()), visibility: None,
        }],
        imports: Vec::new(),
        doc_comments: Vec::new(),
        source: std::fs::read_to_string(&src_file).unwrap(),
    };
    let graph = code_repo_wiki::analysis::build_graph(std::slice::from_ref(&insight)).expect("构建 graph 失败");
    let config = watch_config(&repo);
    let state_dir = config.output_dir().join(".state");

    // 预存状态：文件指纹与磁盘当前内容一致 → is_file_changed 返回 Ok(false)，
    // 指纹比对分支不命中，changed_files 只来自 watch_paths 透传（隔离观测点）
    let fp = GenerationState::compute_file_fingerprint(&src_file).expect("计算文件指纹失败");
    let state = GenerationState {
        last_commit_hash: Some("test".into()),
        file_fingerprints: HashMap::from([(src_file.to_string_lossy().to_string(), fp)]),
        doc_fingerprints: HashMap::new(),
        doc_modules: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: "test".into(),
        tool_version: None,
        failed_modules: vec![],
    };
    state.save(&state_dir).expect("保存状态失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());

    // 实验组：./ 前缀路径 → 原样透传 + 传播不命中（当前行为记录）
    let dot_result = code_repo_wiki::incremental::run_incremental_update_at(
        &root,
        std::slice::from_ref(&insight),
        &graph,
        &config,
        &[PathBuf::from("./src/foo.rs")],
    )
    .expect("增量分析失败");
    assert_eq!(
        dot_result.changed_files,
        vec![PathBuf::from("./src/foo.rs")],
        "当前行为：./ 前缀路径未相对化、未归一化，原样透传到 changed_files"
    );
    assert!(
        dot_result.affected_modules.is_empty(),
        "当前行为：未相对化路径在 find_start_nodes 子串匹配中不命中 → 不参与影响传播: {:?}",
        dot_result.affected_modules
    );

    // 对照组：相对化成功形态（lib.rs 相对化后的结果）→ 传播命中
    let ok_result = code_repo_wiki::incremental::run_incremental_update_at(
        &root,
        &[insight],
        &graph,
        &config,
        &[PathBuf::from("src/foo.rs")],
    )
    .expect("增量分析失败");
    assert!(
        !ok_result.affected_modules.is_empty(),
        "相对化成功形态应命中影响传播（对照，证明 ./ 前缀是传播不命中的原因）"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
