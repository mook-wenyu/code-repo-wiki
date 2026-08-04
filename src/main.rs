use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "repo-wiki", about = "代码仓库 Wiki 自动生成系统", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 全量生成 Wiki 文档
    Generate {
        /// 配置文件路径（默认 .repo-wiki/config.toml）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 输出目录（覆盖配置文件中的 output.dir）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 清空人工修改保护集，强制覆盖所有文档
        #[arg(long)]
        force: bool,
        /// 以 JSON 行输出流水线进度（供插件解析，如 {"stage":"scanning","progress":10}）
        #[arg(long)]
        progress_json: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 增量更新 Wiki 文档
    Update {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 输出目录（覆盖配置文件中的 output.dir）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 清空人工修改保护集，强制覆盖所有文档（与 generate --force 语义一致）
        #[arg(long)]
        force: bool,
        /// 以 JSON 行输出流水线进度（供插件解析，如 {"stage":"scanning","progress":10}）
        #[arg(long)]
        progress_json: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 同步产物目录内容到指纹库（Git 内容合入，不触发 LLM 生成）
    Sync {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 查看当前 Wiki 状态
    Status {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 检查 Wiki 产物健康（孤儿页/断链/过时），供 CI 使用；有问题时退出码非 0
    Lint {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 追加一条知识沉淀记录到 _log.md（Karpathy log 模式，人工可读可 grep）
    Note {
        /// 记录文本
        text: String,
        /// 配置文件路径（取主语言写日志）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 导出 Wiki 为 HTML
    Export {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 输出目录（覆盖配置文件中的 output.dir，仅 skip_generate=false 时生效）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 跳过生成，直接从导出快照导出（需先运行过 generate/update 落盘快照）
        #[arg(long)]
        skip_generate: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 初始化配置文件
    Init {
        /// 输出路径（相对 root 解析；缺省走默认配置链——项目级存在则
        /// 用项目级，否则创建全局默认配置；v13 E 组）
        path: Option<PathBuf>,
        /// 强制覆盖已存在的配置文件（缺省路径已存在时跳过不覆盖）
        #[arg(long)]
        force: bool,
        /// 项目根目录（路径定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 监听文件变更并自动增量更新 Wiki
    Watch {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（扫描根/监听根基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 搜索代码实体
    Search {
        /// 搜索关键词
        #[arg(short, long)]
        query: String,
        /// 返回结果数量（未传时取配置 search.default_top_k）
        #[arg(short = 'k', long)]
        top_k: Option<usize>,
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 以 JSON 格式输出
        #[arg(long)]
        json: bool,
        /// 搜索引擎选择: text / semantic / hybrid（默认取配置文件中的 default_engine）
        #[arg(short, long)]
        engine: Option<String>,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// AST 精确符号查找：扫描源文件定位符号定义（文件+行号+签名，不依赖搜索索引）
    AstSearch {
        /// 要查找的符号名（函数/结构体/trait/类等）
        symbol: String,
        /// 源语言（rust/python/go/...）；省略时按文件扩展名自动推断
        #[arg(short, long)]
        language: Option<String>,
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 以 JSON 格式输出
        #[arg(long)]
        json: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 知识卡片操作（Qoder /knowledge 对等）
    Card {
        #[command(subcommand)]
        action: CardAction,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 将 repo-wiki 注册为 OpenCode 插件
    InstallToOpencode {
        /// 项目根目录（插件/hook 安装基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 从 OpenCode 卸载 repo-wiki 插件
    UninstallFromOpencode {
        /// 跳过确认（卸载将移除集成配置）
        #[arg(long)]
        force: bool,
        /// 项目根目录（插件/hook 移除基准，默认当前目录；U02 root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 向项目根 AGENTS.md 注入 wiki 引用块（标记对 <!-- REPO-WIKI:START/END --> 之间）
    InstallWiki {
        /// 同时将注入块写入 CLAUDE.md（与 AGENTS.md 同一套标记约定）
        #[arg(long)]
        also_claude: bool,
        /// 项目根目录（默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 移除 AGENTS.md 中的 wiki 引用块（含标记本身；未安装时提示并退出码 0）
    UninstallWiki {
        /// 项目根目录（默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 启动 MCP (Model Context Protocol) stdio server（供 Claude Code/Cline 等客户端连接）
    Mcp {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 评测基准：对目标仓库运行五维自动评测（Coverage/Doc Info/lint/Update Recall/Time）
    ///
    /// 注意：Update Recall 维度会回放 git commit（reset --hard 工作区），
    /// 评测前工作区必须干净（有未提交改动会被拒绝——安全闸）。
    Bench {
        /// 目标仓库根目录（必填；git 回放/扫描基准）
        #[arg(long)]
        root: PathBuf,
        /// 仓库名（报告标识，缺省取 root 目录名）
        #[arg(long)]
        repo_name: Option<String>,
        /// 配置文件路径（缺省 root/.repo-wiki/config.toml）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 以 JSON 格式输出报告
        #[arg(long)]
        json: bool,
        /// 追加 TQS LLM 裁判打分维度（需配置 LLM API key；快照缺失或
        /// LLM 不可用时该维度跳过）
        #[arg(long)]
        judge: bool,
    },
}

/// 知识卡片操作子命令（业务动作定义在 lib 的 generate::card::CardAction）
#[derive(Subcommand)]
enum CardAction {
    /// 为单个模块生成卡片（重新生成）
    Generate {
        /// 模块名（如 src::config）
        module: String,
        /// 配置文件路径
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// 按指令修改已有卡片
    Modify {
        /// 模块名（如 src::config）
        module: String,
        /// 修改指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（可重复传 --reference，可选）
        #[arg(long)]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// 在已有卡片上追加内容
    Supplement {
        /// 模块名（如 src::config）
        module: String,
        /// 补充指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（可重复传 --reference，可选）
        #[arg(long)]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// 忽略现有内容全量重写
    Rewrite {
        /// 模块名（如 src::config）
        module: String,
        /// 重写指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（可重复传 --reference，可选）
        #[arg(long)]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// 解析 --root 参数（缺省当前目录）
///
/// ProjectRoot 是扫描根/git 定位/watch 根的注入载体（票 15）；
/// output.dir 等产物路径仍按配置原样解析（相对 cwd），--root 只管
/// "代码从哪扫"而非"产物写哪"。
fn resolve_root(root: Option<&Path>) -> anyhow::Result<repo_wiki::project::ProjectRoot> {
    match root {
        // N7 修复：--root 指定的目录不存在时显式报错——此前静默通过，
        // 扫描产出空集，流水线报"未找到任何源文件"（方向误导）或产物
        // 静默为空。
        Some(p) if !p.is_dir() => {
            anyhow::bail!("--root 指定的目录不存在: {}", p.display())
        }
        Some(p) => Ok(repo_wiki::project::ProjectRoot::new(p.to_path_buf())),
        None => repo_wiki::project::ProjectRoot::from_cwd(),
    }
}

/// 解析 --config 参数：显式指定原样使用；缺省走默认配置链
/// （项目级 .repo-wiki/config.toml → 全局用户级目录 → 创建全局，
/// 见 config::resolve_default_config_path；E 组 v13）
fn resolve_config_path(
    config: Option<&Path>,
    root: &repo_wiki::project::ProjectRoot,
) -> anyhow::Result<PathBuf> {
    repo_wiki::config::resolve_config_path(config, root)
}

fn main() -> anyhow::Result<()> {    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Generate { config, output, force, progress_json, root } => {
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let result = if progress_json {
                // JSONL 进度输出：插件 wiki_generate 流式解析
                repo_wiki::run_pipeline_with_progress(
                    &config, output.as_deref(), force, &root,
                    &repo_wiki::GenerationMode::Full,
                    &|evt| {
                        println!(r#"{{"stage":"{}","progress":{}}}"#, evt.stage, evt.percent);
                    },
                )?
            } else {
                repo_wiki::run_pipeline(
                    &config, output.as_deref(), force, &root,
                    &repo_wiki::GenerationMode::Full,
                )?
            };
            tracing::info!(
                "生成完成: 扫描 {} 个文件, 发现 {} 个实体",
                result.stats.files_scanned,
                result.stats.total_entities
            );
        }
        Commands::Update { config, output, force, progress_json, root } => {
            // update 命令无外部 watch 事件，watch_paths 传空、change_kind 传 None
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let result = if progress_json {
                // JSONL 进度输出：与 generate --progress-json 同构，供插件流式解析
                repo_wiki::run_pipeline_with_progress(
                    &config, output.as_deref(), force, &root,
                    &repo_wiki::GenerationMode::Incremental {
                        watch_paths: Vec::new(),
                        change_kind: None,
                    },
                    &|evt| {
                        println!(r#"{{"stage":"{}","progress":{}}}"#, evt.stage, evt.percent);
                    },
                )?
            } else {
                repo_wiki::run_pipeline(
                    &config, output.as_deref(), force, &root,
                    &repo_wiki::GenerationMode::Incremental {
                        watch_paths: Vec::new(),
                        change_kind: None,
                    },
                )?
            };
            tracing::info!(
                "增量更新完成: 扫描 {} 个文件, {} 个模块受影响",
                result.stats.files_scanned,
                result.stats.modules_detected
            );

            // D2（N2）：update 尾部一致性校验——复用 lint 全部检查对产物做
            // 全量复核（本轮受影响页 + 存量页）。增量更新只重建受影响模块，
            // 跨页一致性问题（断链/引用漂移/符号漂移等）可能残留，此处让
            // 用户立即可见；只告警不改变退出码（"失败只告警"策略——产物
            // 缺陷由 lint 门禁兜底拦截，update 主流程语义不受影响）。
            let cfg = repo_wiki::config::load_config(&config)?;
            let output_dir = Path::new(&cfg.output.dir);
            let source_roots = repo_wiki::commands::source_roots_from_include(&cfg.scope.include);
            let issues = repo_wiki::output::lint::lint(output_dir, &source_roots);
            // v14 D 组（t05 拍板）：语义一致性检查（LLM 跨页矛盾，变更驱动——
            // 只查本次 update 生成的受影响页；LLM 不可用/失败时"只告警"跳过，
            // 语义检查是增强项，静态 lint 已覆盖机械问题）
            if let Err(e) = repo_wiki::output::semantic_lint::check_semantic_consistency(
                &cfg,
                &result.documents,
            )
            .map(|semantic| {
                for issue in &semantic {
                    tracing::warn!("  [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
            }) {
                tracing::warn!("语义一致性检查跳过（LLM 不可用或调用失败）: {e}");
            }
            if !issues.is_empty() {
                tracing::warn!(
                    "update 完成后产物检查发现 {} 个问题（不阻断本次更新，详情可用 `repo-wiki lint` 查看）:",
                    issues.len()
                );
                for issue in &issues {
                    tracing::warn!("  [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
            } else {
                tracing::info!("update 完成后产物检查通过（全部检查无问题）");
            }
        }
        Commands::Sync { config, root } => {
            // sync = Git 内容 → 指纹库（不触发 LLM）；与 update = 代码变更 → 增量生成 边界分离。
            // 产物目录相对 cwd 解析（与 generate 的 output.dir 同基准，指纹键形态一致——
            // 若以 root 重定向会破坏与生成状态键的一致性，人工修改保护检测失效）。
            // --root 仅用于默认配置链的项目级定位（v13 E 组），产物基准语义不变。
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let cfg = repo_wiki::config::load_config(&config)?;
            repo_wiki::commands::sync_from_git(Path::new(&cfg.output.dir))?;
            tracing::info!("同步完成 (--config {})", config.display());
        }
        Commands::Status { config, root } => {
            // --root 提供时以 root 为产物目录基准（跨 cwd 运行 status 能定位正确产物；
            // 缺省 root=cwd 行为不变）
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let mut cfg = repo_wiki::config::load_config(&config)?;
            cfg.output.dir = root.path().join(&cfg.output.dir).to_string_lossy().into_owned();
            tracing::info!("配置加载成功: {}", config.display());
            let report = repo_wiki::commands::status_report(&cfg);
            // ready 才报告页面统计与 lint 结果；未生成时引导运行 generate
            if report.ready {
                println!("Wiki 状态: 就绪");
                println!("配置文件: {}", config.display());
                println!("页面: {} 张，卡片: {} 张", report.wiki_pages, report.cards);
                // lint 产物健康检查结果（与 lint 命令同格式，问题退出码非 0）
                for issue in &report.issues {
                    println!("- [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
                if !report.issues.is_empty() {
                    anyhow::bail!("status: 发现 {} 个问题", report.issues.len());
                }
            } else {
                println!("Wiki 状态: 未生成（运行 repo-wiki generate）");
                println!("配置文件: {}", config.display());
            }
        }
        Commands::Lint { config, root } => {
            // lint 检查产物健康:孤儿页/断链/过时;发现问题时以非 0 退出码结束
            // (供 CI 门禁使用:git hook 或流水线可据此拒绝合并)
            // --root 提供时以 root 为产物目录基准（同 status）
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let mut cfg = repo_wiki::config::load_config(&config)?;
            cfg.output.dir = root.path().join(&cfg.output.dir).to_string_lossy().into_owned();
            let output_dir = Path::new(&cfg.output.dir);
            // 源码根从 scope.include 派生(取通配符前的目录前缀,如 "src/**" → "src")：
            // 过时检查需要对比源文件 mtime,空根会导致检查静默跳过(缺陷修复前行为)
            let source_roots = repo_wiki::commands::source_roots_from_include(&cfg.scope.include);
            let issues = repo_wiki::output::lint::lint(output_dir, &source_roots);
            if issues.is_empty() {
                println!("lint: 通过，无孤儿页/断链/过时问题");
            } else {
                for issue in &issues {
                    println!("lint [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
                anyhow::bail!("lint: 发现 {} 个问题", issues.len());
            }
        }
        Commands::AstSearch { symbol, language, config, json, root } => {
            // AST 精确符号查找：不依赖搜索索引，直接扫描源文件解析 AST 定位定义
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let results = repo_wiki::execute_ast_search(&config, &root, &symbol, language.as_deref())?;
            if json {
                let json_results: Vec<serde_json::Value> = results.iter().map(|hit| {
                    serde_json::json!({
                        "name": hit.node.name,
                        "kind": hit.node.kind.as_str(),
                        "file": hit.node.file_path,
                        "lines": hit.node.line_range,
                        "signature": hit.node.signature,
                        "source": hit.source,
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&json_results)?);
            } else {
                if results.is_empty() {
                    println!("未找到符号 \"{symbol}\" 的定义");
                }
                for (i, hit) in results.iter().enumerate() {
                    let sig = hit.node.signature.as_deref().unwrap_or(&hit.node.name);
                    let loc = match (&hit.node.file_path, hit.node.line_range) {
                        (Some(f), Some((s, e))) => format!("{f}:{s}-{e}"),
                        (Some(f), None) => f.clone(),
                        _ => "(unknown)".to_string(),
                    };
                    println!("{}. {sig} — {loc}", i + 1);
                }
            }
        }
        Commands::Export { config, output, skip_generate, root } => {
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let cfg = repo_wiki::config::load_config(&config)?;
            if skip_generate {
                // 从导出快照恢复导出（票 06）：不重跑生成流水线。
                // render_all 每次写盘后同步写 .state/export_snapshot.json，
                // 快照缺失时明确报错（不静默回退重生成——回退会掩盖
                // 快照契约被破坏的事实）。
                let snapshot_path =
                    repo_wiki::output::export_snapshot_path(Path::new(&cfg.output.dir));
                // 票 04 陈旧检测：快照 mtime 早于任一 wiki 页 mtime = 产物在
                // 快照之后被更新（快照写入失败/被外部改动/产物被手动编辑），
                // 继续导出会静默输出过期内容——显式报错引导重新生成。
                if let (Ok(snapshot_mtime), Some(latest_page)) = (
                    std::fs::metadata(&snapshot_path).and_then(|m| m.modified()),
                    repo_wiki::output::latest_wiki_page_mtime(Path::new(&cfg.output.dir)),
                ) && snapshot_mtime < latest_page
                {
                    anyhow::bail!(
                        "导出快照过期（快照写入时间早于最新 wiki 页），请重新运行 `repo-wiki generate` 或 `repo-wiki update` 后再导出"
                    );
                }
                let content = std::fs::read_to_string(&snapshot_path).with_context(|| {
                    format!(
                        "导出快照不存在，请先运行 `repo-wiki generate` 或 `repo-wiki update`: {}",
                        snapshot_path.display()
                    )
                })?;
                let snapshot: repo_wiki::output::ExportSnapshot =
                    serde_json::from_str(&content).with_context(|| "解析导出快照失败")?;
                // 票 10：快照版本契约校验——未来格式演进时旧版本可被
                // 显式拒绝（当前仅版本 1；缺失字段的旧文件会被 serde
                // 默认值补齐后误读，故版本不符必须硬性报错而非容错）
                if snapshot.version != 1 {
                    anyhow::bail!(
                        "导出快照版本 {} 不受支持（当前支持: 1），请重新运行 `repo-wiki generate` 或 `repo-wiki update`",
                        snapshot.version
                    );
                }
                repo_wiki::output::html::export_html(
                    &snapshot.documents,
                    &snapshot.cards,
                    &snapshot.modules,
                    &cfg,
                )?;
            } else {
                let result = repo_wiki::run_pipeline(
                    &config, output.as_deref(), false, &root,
                    &repo_wiki::GenerationMode::Full,
                )?;
                repo_wiki::output::html::export_html(
                    &result.documents,
                    &result.cards,
                    &repo_wiki::output::export_modules(&result.graph, &result.cards),
                    &cfg,
                )?;
            }
            tracing::info!("HTML 导出完成 (--config {})", config.display());
        }
        Commands::Note { text, config, root } => {
            // --root 提供时以 root 为产物目录基准（同 status/lint）
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let mut cfg = repo_wiki::config::load_config(&config)?;
            cfg.output.dir = root.path().join(&cfg.output.dir).to_string_lossy().into_owned();
            repo_wiki::commands::append_note(
                Path::new(&cfg.output.dir),
                &cfg.wiki.language,
                &text,
            )?;
            tracing::info!("知识记录已写入 (--config {})", config.display());
        }
        Commands::Init { path, force, root } => {
            // --root 提供时 path 相对 root 解析（与产物目录基准一致）；
            // path 缺省走默认配置链：项目级 .repo-wiki/config.toml 存在则
            // 复用（不重复创建），否则创建全局默认配置（E 组引导语义）
            let root = resolve_root(root.as_deref())?;
            let via_default_chain = path.is_none();
            let path = match path {
                Some(p) if p.is_absolute() => p,
                Some(p) => root.path().join(p),
                None => repo_wiki::config::resolve_default_config_path(&root)?,
            };
            // v17 t03：仅**缺省链**路径已存在时跳过不覆盖（防数据破坏——
            // v17 审计发现原实现无条件 write 覆盖用户配置，与注释"复用"
            // 语义矛盾）；显式 path 是用户明确意图，保持覆盖语义；
            // --force 对缺省链的跳过生效（强制重写）。
            if via_default_chain && path.exists() && !force {
                tracing::info!(
                    "配置文件已存在，跳过创建（使用 --force 覆盖）: {}",
                    path.display()
                );
                return Ok(());
            }
            repo_wiki::config::create_default_config(&path)?;
            tracing::info!("默认配置文件已创建: {}", path.display());
        }
        Commands::Watch { config, root } => {
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            repo_wiki::run_watch(&config, &root)?;
        }
        Commands::Search { query, top_k, config, json, engine, root } => {
            // 解析引擎类型：优先用 CLI 参数，否则取配置文件中的 default_engine
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let cfg = repo_wiki::config::load_config(&config)?;
            let engine_type = match engine.as_deref() {
                Some("text") => repo_wiki::config::schema::SearchEngineType::Text,
                Some("semantic") => repo_wiki::config::schema::SearchEngineType::Semantic,
                Some("hybrid") => repo_wiki::config::schema::SearchEngineType::Hybrid,
                Some(other) => anyhow::bail!("不支持的搜索引擎: {other}（可选: text/semantic/hybrid）"),
                None => cfg.search.default_engine.clone(),
            };
            // CLI 显式 -k 优先，未传时回退配置 search.default_top_k
            // N17：top_k 下限收敛到 1（top_k=0 的搜索调用无意义，返回空结果）
            let top_k = top_k.unwrap_or(cfg.search.default_top_k).max(1);
            let results = repo_wiki::execute_search(&config, &root, &query, top_k, &engine_type)?;
            if json {
                // JSON 格式输出（供 OpenCode 插件解析）
                let json_results: Vec<serde_json::Value> = results.iter().map(|hit| {
                    serde_json::json!({
                        "name": hit.node.name,
                        "kind": hit.node.kind.as_str(),
                        "score": hit.score,
                        "file": hit.node.file_path,
                        "lines": hit.node.line_range,
                        "signature": hit.node.signature,
                        "source": hit.source,
                        "callers": hit.callers,
                        "callees": hit.callees,
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&json_results)?);
            } else {
                // 表格格式输出（人类可读）
                if results.is_empty() {
                    println!("未找到匹配结果");
                } else {
                    println!("{:<4} {:<30} {:<12} {:<8} 文件", "#", "名称", "类型", "分数");
                    println!("{}", "-".repeat(80));
                    for (i, hit) in results.iter().enumerate() {
                        let file = hit.node.file_path.as_deref().unwrap_or("-");
                        println!("{:<4} {:<30} {:<12} {:<8.2} {}",
                            i + 1, hit.node.name, hit.node.kind.as_str(), hit.score, file);
                    }
                }
            }
        }
        Commands::InstallToOpencode { root } => {
            let root = resolve_root(root.as_deref())?;
            repo_wiki::commands::install("opencode", &root)?;
        }
        Commands::UninstallFromOpencode { force, root } => {
            let root = resolve_root(root.as_deref())?;
            repo_wiki::commands::uninstall(force, &root)?;
        }
        Commands::InstallWiki { also_claude, root } => {
            // AGENTS.md 注入 wiki 引用块（--also-claude 双写 CLAUDE.md）；
            // 注入逻辑在 commands::install_wiki，此处只做 --root 解析与调用
            let root = resolve_root(root.as_deref())?;
            repo_wiki::commands::install_wiki(&root, also_claude)?;
        }
        Commands::UninstallWiki { root } => {
            let root = resolve_root(root.as_deref())?;
            repo_wiki::commands::uninstall_wiki(&root)?;
        }
        Commands::Mcp { config, root } => {
            // MCP stdio server：阻塞直到客户端断开。异步运行时由库内
            // get_global_runtime 提供（与流水线共用，避免二次初始化）。
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            let rt = repo_wiki::get_global_runtime();
            rt.block_on(repo_wiki::mcp::serve_stdio(config, root))?;
        }
        Commands::Card { action, root } => {
            use repo_wiki::generate::card as card_cmd;
            // CLI 枚举转业务枚举（config 路径在匹配时提取，供 run_card_command 使用）
            let (config, action) = match action {
                CardAction::Generate { module, config } => {
                    (config, card_cmd::CardAction::Generate { module })
                }
                CardAction::Modify { module, instruction, reference, config } => (
                    config,
                    card_cmd::CardAction::Modify { module, instruction, references: reference },
                ),
                CardAction::Supplement { module, instruction, reference, config } => (
                    config,
                    card_cmd::CardAction::Supplement { module, instruction, references: reference },
                ),
                CardAction::Rewrite { module, instruction, reference, config } => (
                    config,
                    card_cmd::CardAction::Rewrite { module, instruction, references: reference },
                ),
            };
            let root = resolve_root(root.as_deref())?;
            let config = resolve_config_path(config.as_deref(), &root)?;
            repo_wiki::run_card_command(&config, &root, &action)?;
        }
        Commands::Bench { root, repo_name, config, json, judge } => {
            // 评测基准（U10）：五维自动评测。root 必填（评测对象仓库根），
            // ProjectRoot::new 会校验目录存在性（N7）。config 缺省走默认
            // 配置链（E 组：项目级 → 全局 → 创建全局）；repo_name 缺省取
            // root 目录名。
            // Update Recall 回放前有工作区干净检查（安全闸，事故教训），
            // 脏工作区会明确报错拒绝评测。
            let root = repo_wiki::project::ProjectRoot::new(root);
            let config_path = resolve_config_path(config.as_deref(), &root)?;
            let cfg = repo_wiki::config::load_config(&config_path)?;
            let repo_name = repo_name.unwrap_or_else(|| {
                root.path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string())
            });
            let report = repo_wiki::bench::run_bench(&config_path, &root, &cfg, &repo_name, judge)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", repo_wiki::bench::render_markdown(&report));
            }
        }
    }

    Ok(())
}
