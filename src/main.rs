use std::path::PathBuf;

mod commands;

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
    },
    /// 增量更新 Wiki 文档
    Update {
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
    /// 将 repo-wiki 注册为 OpenCode 插件
    InstallToOpencode,
    /// 从 OpenCode 卸载 repo-wiki 插件
    UninstallFromOpencode,
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
        Commands::Generate { config, output: _ } => {
            let result = repo_wiki::run_pipeline(&config)?;
            tracing::info!(
                "生成完成: 扫描 {} 个文件, 发现 {} 个实体",
                result.stats.files_scanned,
                result.stats.total_entities
            );
        }
        Commands::Update { config } => {
            let result = repo_wiki::run_incremental_pipeline(&config)?;
            tracing::info!(
                "增量更新完成: 扫描 {} 个文件, {} 个模块受影响",
                result.stats.files_scanned,
                result.stats.modules_detected
            );
        }
        Commands::Status { config } => {
            let _cfg = repo_wiki::config::load_config(&config)?;
            tracing::info!("配置加载成功: {}", config.display());
            println!("Wiki 状态: 就绪");
            println!("配置文件: {}", config.display());
        }
        Commands::Export { config } => {
            let cfg = repo_wiki::config::load_config(&config)?;
            let result = repo_wiki::run_pipeline(&config)?;
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
            commands::install("opencode")?;
        }
        Commands::UninstallFromOpencode => {
            commands::uninstall(false)?;
        }
    }

    Ok(())
}
