use std::path::PathBuf;

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
    }

    Ok(())
}
