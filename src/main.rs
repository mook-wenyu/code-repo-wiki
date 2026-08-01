use std::path::{Path, PathBuf};

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
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
        /// 输出目录（覆盖配置文件中的 output.dir）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 清空人工修改保护集，强制覆盖所有文档
        #[arg(long)]
        force: bool,
        /// 以 JSON 行输出流水线进度（供插件解析，如 {"stage":"scanning","progress":10}）
        #[arg(long)]
        progress_json: bool,
    },
    /// 增量更新 Wiki 文档
    Update {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
        /// 输出目录（覆盖配置文件中的 output.dir）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 清空人工修改保护集，强制覆盖所有文档（与 generate --force 语义一致）
        #[arg(long)]
        force: bool,
    },
    /// 同步产物目录内容到指纹库（Git 内容合入，不触发 LLM 生成）
    Sync {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 查看当前 Wiki 状态
    Status {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 检查 Wiki 产物健康（孤儿页/断链/过时），供 CI 使用；有问题时退出码非 0
    Lint {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 导出 Wiki 为 HTML
    Export {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 初始化配置文件
    Init {
        /// 输出路径
        #[arg(default_value = ".repo-wiki/config.toml")]
        path: PathBuf,
    },
    /// 监听文件变更并自动增量更新 Wiki
    Watch {
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 搜索代码实体
    Search {
        /// 搜索关键词
        #[arg(short, long)]
        query: String,
        /// 返回结果数量
        #[arg(short = 'k', long, default_value = "10")]
        top_k: usize,
        /// 配置文件路径
        #[arg(short, long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
        /// 以 JSON 格式输出
        #[arg(long)]
        json: bool,
        /// 搜索引擎选择: text / semantic / hybrid（默认取配置文件中的 default_engine）
        #[arg(short, long)]
        engine: Option<String>,
    },
    /// 知识卡片操作（Qoder /knowledge 对等）
    Card {
        #[command(subcommand)]
        action: CardAction,
    },
    /// 将 repo-wiki 注册为 OpenCode 插件
    InstallToOpencode,
    /// 从 OpenCode 卸载 repo-wiki 插件
    UninstallFromOpencode {
        /// 跳过确认（卸载将移除集成配置）
        #[arg(long)]
        force: bool,
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
        #[arg(long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 按指令修改已有卡片
    Modify {
        /// 模块名（如 src::config）
        module: String,
        /// 修改指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（逗号分隔，可选）
        #[arg(long, value_delimiter = ',')]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 在已有卡片上追加内容
    Supplement {
        /// 模块名（如 src::config）
        module: String,
        /// 补充指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（逗号分隔，可选）
        #[arg(long, value_delimiter = ',')]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
    /// 忽略现有内容全量重写
    Rewrite {
        /// 模块名（如 src::config）
        module: String,
        /// 重写指令
        #[arg(long)]
        instruction: String,
        /// 参考文件路径（逗号分隔，可选）
        #[arg(long, value_delimiter = ',')]
        reference: Vec<PathBuf>,
        /// 配置文件路径
        #[arg(long, default_value = ".repo-wiki/config.toml")]
        config: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Generate { config, output, force, progress_json } => {
            let result = if progress_json {
                // JSONL 进度输出：插件 wiki_generate 流式解析
                repo_wiki::run_pipeline_with_progress(&config, output.as_deref(), force, &|evt| {
                    println!(r#"{{"stage":"{}","progress":{}}}"#, evt.stage, evt.percent);
                })?
            } else {
                repo_wiki::run_pipeline(&config, output.as_deref(), force)?
            };
            tracing::info!(
                "生成完成: 扫描 {} 个文件, 发现 {} 个实体",
                result.stats.files_scanned,
                result.stats.total_entities
            );
        }
        Commands::Update { config, output, force } => {
            // update 命令无外部 watch 事件，watch_paths 传空、change_kind 传 None
            let result = repo_wiki::run_incremental_pipeline(&config, output.as_deref(), force, &[], None)?;
            tracing::info!(
                "增量更新完成: 扫描 {} 个文件, {} 个模块受影响",
                result.stats.files_scanned,
                result.stats.modules_detected
            );
        }
        Commands::Sync { config } => {
            // sync = Git 内容 → 指纹库（不触发 LLM）；与 update = 代码变更 → 增量生成 边界分离
            let cfg = repo_wiki::config::load_config(&config)?;
            repo_wiki::commands::sync_from_git(Path::new(&cfg.output.dir))?;
            tracing::info!("同步完成 (--config {})", config.display());
        }
        Commands::Status { config } => {
            let _cfg = repo_wiki::config::load_config(&config)?;
            tracing::info!("配置加载成功: {}", config.display());
            println!("Wiki 状态: 就绪");
            println!("配置文件: {}", config.display());
        }
        Commands::Lint { config } => {
            // lint 检查产物健康:孤儿页/断链/过时;发现问题时以非 0 退出码结束
            // (供 CI 门禁使用:git hook 或流水线可据此拒绝合并)
            let cfg = repo_wiki::config::load_config(&config)?;
            let output_dir = Path::new(&cfg.output.dir);
            let issues = repo_wiki::output::lint::lint(output_dir, &[]);
            if issues.is_empty() {
                println!("lint: 通过，无孤儿页/断链/过时问题");
            } else {
                for issue in &issues {
                    println!("lint [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
                anyhow::bail!("lint: 发现 {} 个问题", issues.len());
            }
        }
        Commands::Export { config } => {
            let cfg = repo_wiki::config::load_config(&config)?;
            let result = repo_wiki::run_pipeline(&config, None, false)?;
            repo_wiki::output::html::export_html(&result.documents, &result.cards, &result.graph, &cfg)?;
            tracing::info!("HTML 导出完成 (--config {})", config.display());
        }
        Commands::Init { path } => {
            repo_wiki::config::create_default_config(&path)?;
            tracing::info!("默认配置文件已创建: {}", path.display());
        }
        Commands::Watch { config } => {
            repo_wiki::run_watch(&config)?;
        }
        Commands::Search { query, top_k, config, json, engine } => {
            // 解析引擎类型：优先用 CLI 参数，否则取配置文件中的 default_engine
            let cfg = repo_wiki::config::load_config(&config)?;
            let engine_type = match engine.as_deref() {
                Some("text") => repo_wiki::config::schema::SearchEngineType::Text,
                Some("semantic") => repo_wiki::config::schema::SearchEngineType::Semantic,
                Some("hybrid") => repo_wiki::config::schema::SearchEngineType::Hybrid,
                Some(other) => anyhow::bail!("不支持的搜索引擎: {other}（可选: text/semantic/hybrid）"),
                None => cfg.search.default_engine.clone(),
            };
            let results = repo_wiki::execute_search(&config, &query, top_k, &engine_type)?;
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
        Commands::InstallToOpencode => {
            repo_wiki::commands::install("opencode")?;
        }
        Commands::UninstallFromOpencode { force } => {
            repo_wiki::commands::uninstall(force)?;
        }
        Commands::Card { action } => {
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
            repo_wiki::run_card_command(&config, &action)?;
        }
    }

    Ok(())
}
