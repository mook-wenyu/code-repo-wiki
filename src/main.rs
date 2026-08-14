use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

/// Phase 16.3：顶层 help 分组静态文本（override_help）。
///
/// clap 4.6.4 无原生多组子命令 help（官方 issue #1553 仍 open；
/// next_help_heading 只作用于参数不作用于子命令列表），故用 override_help
/// 静态文本替代，顶层 `-h/--help` 与 `help` 子命令输出此文本；子命令自身
/// `-h` 仍走 clap 自动帮助。此文本是第二真源，tests/test_cli_smoke.rs 的
/// test_help_shows_grouped_commands 守卫防漂移（4 分组标题 + 18 命令各一次）。
const GROUPED_HELP: &str = "\
代码仓库 Wiki 自动生成系统

Usage: code-repo-wiki <COMMAND>

查询命令:
  search         搜索代码实体（BM25/语义/混合）
  ast-search     AST 精确符号查找
  status         查看当前 Wiki 状态
  note           追加知识沉淀记录
  card           知识卡片操作

生成命令:
  generate       全量生成 Wiki 文档
  update         增量更新 Wiki 文档
  sync           同步产物目录到指纹库
  export         导出 Wiki 为 HTML
  watch          监听文件变更并自动增量更新

维护命令:
  install        安装集成（插件/MCP/AGENTS.md/hooks）
  uninstall      卸载集成
  key            交互式配置 LLM API key
  doctor         环境诊断（六项检查）
  lint           检查 Wiki 产物健康
  mcp            启动 MCP stdio server

评测命令:
  bench          对目标仓库运行五维自动评测
  bench-manifest 清单批量跑分

Options:
  -h, --help     Print help
  -V, --version  Print version
";

#[derive(Parser)]
#[command(name = "code-repo-wiki", about = "代码仓库 Wiki 自动生成系统", version, override_help = GROUPED_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 全量生成 Wiki 文档
    Generate {
        /// 配置文件路径（默认缺省链：项目级 config.toml → 用户级 config.toml → 创建用户级）
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
        /// 锁被占用时等待的秒数（超时仍报错）
        #[arg(long)]
        wait: Option<u64>,
        /// 锁被占用时跳过本次操作（退出码 0），供 hook/CI 非阻塞使用
        #[arg(long)]
        skip_if_locked: bool,
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
        /// 只分析并预览将更新的页面清单，不执行生成（无副作用）
        /// 与 --force/--progress-json 互斥：dry-run 不走流水线，两者在
        /// 预览模式下无意义（audit-cli-07：此前静默忽略）
        #[arg(long, conflicts_with_all = ["force", "progress_json"])]
        dry_run: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
        /// 锁被占用时等待的秒数（超时仍报错）
        #[arg(long)]
        wait: Option<u64>,
        /// 锁被占用时跳过本次操作（退出码 0），供 hook/CI 非阻塞使用
        #[arg(long)]
        skip_if_locked: bool,
    },
    /// 同步产物目录内容到指纹库（Git 内容合入，不触发 LLM 生成）
    Sync {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 查看当前 Wiki 状态
    Status {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 检查 Wiki 产物健康（孤儿页/断链/过时），供 CI 使用；有问题时退出码非 0
    Lint {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（产物目录定位基准，默认当前目录；root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 环境诊断：配置可解析/产物目录可写/输出目录状态/LLM Key/网络/
    /// 版本漂移 六查，逐项输出 ✓/✗；全过退出码 0，任一失败退出码 1
    Doctor {
        /// 配置文件路径（缺省走默认配置链）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（路径定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 交互式配置 LLM API key（写入用户级 config.toml，不随 Git 共享）
    ///
    /// 安全底线（用户拍板）：明文 key 只写用户级配置；--env 改写入建议的
    /// 环境变量名引用（openai→DEEPSEEK_API_KEY、anthropic→ANTHROPIC_API_KEY），
    /// key 本体由 shell 环境提供。非交互终端（管道/CI/Agent）打印引导并退出 0。
    Key {
        /// 改用环境变量方式：写 api_key_env 引用而非明文
        #[arg(long)]
        env: bool,
        /// 配置文件路径（仅用于读取 provider 判定；写入目标始终是用户级配置）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 项目根目录（路径定位基准，默认当前目录）
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
        /// 项目根目录（产物目录定位基准，默认当前目录；root 补齐族）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 导出 Wiki 为 HTML
    Export {
        /// 配置文件路径
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 输出目录（覆盖配置文件中的 output.dir；--skip-generate 时给出会显式报错——
        /// 快照绑定配置文件 output.dir，--output 无落点）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 跳过生成，直接从导出快照导出（需先运行过 generate/update 落盘快照）
        #[arg(long)]
        skip_generate: bool,
        /// 项目根目录（扫描根/git 定位基准，默认当前目录）
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 安装 code-repo-wiki 集成（OpenCode 插件 + 多 Agent MCP + AGENTS.md + git hooks）
    ///
    /// 默认执行：① 确保用户级默认配置（config.toml）存在，缺失时自动创建
    /// （init 并入 install，配置链=项目级 config.toml 覆盖用户级）；
    /// ② 注册 OpenCode 插件（用户级 ~/.config/opencode/plugins/code-repo-wiki.ts）；
    /// ③ 注册 OpenCode MCP（用户级全局 opencode.json 的 mcp 块）；
    /// ④ 注入 AGENTS.md wiki 引用块；⑤ 安装 git post-commit/post-merge hooks。
    /// --claude 额外注册 Claude Code MCP（用户级 ~/.claude.json 顶层 mcpServers，
    /// User scope——不再写项目根 .mcp.json）并同步注入 CLAUDE.md
    /// （--also-claude 并入——Claude Code 不读 AGENTS.md，注册 MCP
    /// 时必然需要文档指引，两个开关分离无意义）；--codex 额外注册 Codex
    /// CLI MCP（用户级 ~/.codex/config.toml）；--dsh 额外注册 DeepSeek
    /// Harness MCP（项目根 cordis.patch.yml 的 patch 层——dsh 不读
    /// .mcp.json，MCP 必须显式配置在 patch 层；AGENTS.md/CLAUDE.md 由 dsh
    /// 自动读取，文档指引零成本）。
    /// 全部幂等；已存在的非 code-repo-wiki 内容（用户自定义 hook/其他 MCP server）保留。
    Install {
        /// 额外注册 Claude Code MCP（用户级 ~/.claude.json）并同步注入 CLAUDE.md
        #[arg(long)]
        claude: bool,
        /// 额外注册 Codex CLI MCP（用户级 ~/.codex/config.toml，[mcp_servers.code-repo-wiki]）
        #[arg(long)]
        codex: bool,
        /// 额外注册 DeepSeek Harness MCP（项目根 cordis.patch.yml 的 - insert: 块）
        #[arg(long)]
        dsh: bool,
        /// 项目根目录：插件/hook 安装基准，默认当前目录
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
        /// 搜索引擎选择（默认 hybrid；hybrid 无嵌入 key 时自动降级纯 text）
        #[arg(short, long)]
        engine: Option<code_repo_wiki::config::schema::SearchEngineType>,
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
        /// global=true（audit-cli-04）：允许 `card generate <module> --root X`
        /// 在动作子命令后传参（与 --wait/--skip-if-locked 同构）——
        /// 此前仅卡级可解析，跨 cwd 运行 `card generate ... --root` 报
        /// unexpected argument 静默失败。
        #[arg(long, global = true)]
        root: Option<PathBuf>,
        /// 锁被占用时等待的秒数（超时仍报错）
        #[arg(long, global = true)]
        wait: Option<u64>,
        /// 锁被占用时跳过本次操作（退出码 0），供 hook/CI 非阻塞使用
        #[arg(long, global = true)]
        skip_if_locked: bool,
    },
    /// 卸载 code-repo-wiki 集成（OpenCode MCP + 插件 + AGENTS.md + hooks
    /// + Claude/Codex MCP 条目；--force 确认）
    ///
    /// 清理 install 写入的全部集成痕迹（幂等，缺省即跳过）；保留用户级
    /// config.toml 与产物数据 .code-repo-wiki/。原 install-wiki/uninstall-wiki/
    /// install-to-opencode/uninstall-from-opencode 四命令合并于此。
    Uninstall {
        /// 跳过确认（卸载将移除集成配置）
        #[arg(long)]
        force: bool,
        /// 项目根目录（插件/hook/AGENTS.md 移除基准，默认当前目录；root 补齐族）
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
        /// 配置文件路径（缺省 root/config.toml，见 load_default_config）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 以 JSON 格式输出报告
        #[arg(long)]
        json: bool,
        /// 追加 TQS LLM 裁判打分维度（需配置 LLM API key；快照缺失或
        /// LLM 不可用时该维度跳过）
        #[arg(long)]
        judge: bool,
        /// RepoDocBench 对齐五维报告——强制 LLM 裁判
        /// （与 --judge 正交，隐含 judge=true）并输出五维聚合摘要
        /// （Coverage / Doc Information / Completeness@K / TQS /
        /// Update Recall），各维缺失时降级跳过并显式标注（不得静默）。
        /// 与 --rubrics-only 互斥（五维含 Update Recall 回放）。
        #[arg(long, conflicts_with = "rubrics_only")]
        repodoc: bool,
        /// 只跑裁判层（Coverage/Doc Info/lint + TQS/Rubric），跳过
        /// Update Recall 的 git commit 回放——大仓库评测时回放成本
        /// 不可接受，用此模式单独完成裁判打分。
        /// 与 --judge 正交：--rubrics-only --judge = 真实 LLM 裁判的
        /// 快速评测（验证轮的标准形态）
        #[arg(long)]
        rubrics_only: bool,
        /// 参考文件路径（可重复传 --reference，可选）——注入 LLM 裁判
        /// （Doc Info/Rubric）作为对照材料，防止凭空打分
        #[arg(long)]
        reference: Vec<PathBuf>,
    },
    /// 清单批量跑分：对清单中每个仓库执行 Coverage/
    /// Doc Info/lint/Time 四快维度，输出仓库×维度矩阵。
    ///
    /// 清单格式：每行一个仓库（`#` 注释/空行跳过）；本地路径直接使用，
    /// `https://` / `git@` 开头视为远程 URL（clone 到 --work-dir）。
    /// 每仓库产物输出到 `--work-dir/<仓库名>-out/`（不污染原仓库）。
    /// Update Recall 回放与 LLM 裁判在本模式跳过（深评请用单仓库 bench）。
    BenchManifest {
        /// 清单文件路径
        #[arg(long)]
        manifest: PathBuf,
        /// 模板配置文件路径（scope/llm/provider 等；缺省走默认配置链）
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// 以 JSON 格式输出矩阵
        #[arg(long)]
        json: bool,
        /// 远程仓库 clone 落地与产物目录的父目录（缺省系统临时目录）
        #[arg(long)]
        work_dir: Option<PathBuf>,
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
        #[arg(short, long)]
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
        #[arg(short, long)]
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
        #[arg(short, long)]
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
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

/// 解析 --root 参数（缺省当前目录）
///
/// ProjectRoot 是扫描根/git 定位/watch 根的注入载体（票 15）；
/// output.dir 等产物路径仍按配置原样解析（相对 cwd），--root 只管
/// "代码从哪扫"而非"产物写哪"。
fn resolve_root(root: Option<&Path>) -> anyhow::Result<code_repo_wiki::project::ProjectRoot> {
    match root {
        // N7 修复：--root 指定的目录不存在时显式报错——此前静默通过，
        // 扫描产出空集，流水线报"未找到任何源文件"（方向误导）或产物
        // 静默为空。
        Some(p) if !p.is_dir() => {
            anyhow::bail!("--root 指定的目录不存在: {}", p.display())
        }
        Some(p) => Ok(code_repo_wiki::project::ProjectRoot::new(p.to_path_buf())),
        None => code_repo_wiki::project::ProjectRoot::from_cwd(),
    }
}
/// 进度阶段英文标识 → 中文展示名（v44：文本模式进度行）。
/// 阶段与 lib.rs run_pipeline_with_progress 的 on_progress 事件一一对应。
fn stage_zh(stage: &str) -> &str {
    match stage {
        "scanning" => "扫描源码",
        "analyzing" => "构建知识图谱",
        "chunking" => "切分文档块",
        "cards" => "生成知识卡片",
        "wiki" => "生成 Wiki 页",
        "output" => "渲染与写盘",
        "index" => "更新搜索索引",
        "done" => "完成",
        other => other,
    }
}

/// v46：文本进度渲染状态（节流判定的上一事件快照）。
#[derive(Default)]
struct ProgressRenderState {
    last_stage: Option<&'static str>,
    last_percent: u8,
    last_quarter: Option<u32>,
}

/// 渲染一条进度事件，返回要打印的文本（None = 无实质变化，跳过）。
///
/// - TTY：行内刷新（`\r` + 清行），阶段切换时先换行；done 事件清行
///   （清理行内残留，交由完成摘要收尾）。
/// - 非 TTY（管道/重定向/后台）：普通文本行，按 阶段切换 / 百分比跨
///   10 档 / 每 5 项 节流，避免刷屏（对齐 Ubuntu CLI 规范与 clig.dev）。
/// - 有 current/total 时显示任务单位「N/M（百分比）」（LLM 逐项进度），
///   无项级进度时显示阶段百分比。
fn render_progress(
    evt: &code_repo_wiki::ProgressEvent,
    tty: bool,
    state: &mut ProgressRenderState,
) -> Option<String> {
    if evt.stage == "done" {
        // 完成态：TTY 下清掉行内残留（摘要行随后接管 stdout）；非 TTY 无残留
        return if tty {
            Some("\r\u{1b}[K".to_string())
        } else {
            None
        };
    }
    let line = match (evt.current, evt.total) {
        (Some(c), Some(t)) if t > 0 => {
            format!(
                "进度 [{}] {}/{}（{}%）",
                stage_zh(evt.stage),
                c,
                t,
                evt.percent
            )
        }
        _ => format!("进度 [{}] {}%", stage_zh(evt.stage), evt.percent),
    };
    let stage_changed = state.last_stage != Some(evt.stage);
    if tty {
        // 行内刷新：阶段切换先换行（上一阶段完整行保留），同阶段原地覆盖
        let prefix = if state.last_stage.is_some() && stage_changed {
            "\n"
        } else {
            ""
        };
        state.last_stage = Some(evt.stage);
        state.last_percent = evt.percent;
        state.last_quarter = evt.current;
        return Some(format!("{prefix}\r\u{1b}[K{line}"));
    }
    // 非 TTY 节流：阶段切换 / 百分比跨 10 档 / 每 5 项
    let quarter = evt.current.map(|c| c / 5);
    let changed = stage_changed
        || evt.percent / 10 != state.last_percent / 10
        || quarter != state.last_quarter;
    if changed {
        state.last_stage = Some(evt.stage);
        state.last_percent = evt.percent;
        state.last_quarter = quarter;
        // v47：非 TTY 行必须以换行结尾——调用处统一 `eprint!`（TTY 分支
        // 用 \r 行内刷新不能带换行），无 \n 会与后续 tracing 日志粘在同一行
        // （实测：`进度 [扫描源码] 10%2026-08-09T17:09:49Z WARN ...`）。
        return Some(format!("{line}\n"));
    }
    None
}

/// --progress-json 共享的 JSONL 事件输出闭包（generate/update 同构，
/// audit-cli-12 收敛两处近 30 行重复闭包）
fn progress_json_cb() -> impl Fn(code_repo_wiki::ProgressEvent) {
    |evt| {
        let cur = evt
            .current
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".into());
        let tot = evt
            .total
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".into());
        println!(
            r#"{{"stage":"{}","progress":{},"current":{},"total":{}}}"#,
            evt.stage, evt.percent, cur, tot
        );
    }
}

/// 文本模式共享的进度渲染闭包（TTY 行内刷新 / 非 TTY 节流；
/// generate/update 同构，audit-cli-12 收敛）。每调用一次创建独立
/// 节流状态（与旧内联 `Mutex::new(ProgressRenderState::default())` 等价）。
fn text_progress_cb() -> impl Fn(code_repo_wiki::ProgressEvent) {
    let tty = std::io::stderr().is_terminal();
    let render_state = std::sync::Mutex::new(ProgressRenderState::default());
    move |evt| {
        let mut st = render_state.lock().expect("进度渲染锁中毒");
        if let Some(s) = render_progress(&evt, tty, &mut st) {
            eprint!("{s}");
        }
    }
}

/// 加载配置并按 --output 覆盖 output_dir（与流水线内部
/// load_config_with_output 同规则：output 相对 root 解析，缺省 root/.code-repo-wiki）
///
/// audit-cli-01：update 尾部复核此前用 load_config_rooted（漏传 --output），
/// 复核扫默认目录而非流水线实际写盘目录——--output 下产物有 lint 问题时
/// 静默假阴性。此 helper 保证复核目录与流水线注入的 output 一致。
fn load_config_with_cli_output(
    config: Option<&Path>,
    output: Option<&Path>,
    root: &code_repo_wiki::project::ProjectRoot,
) -> anyhow::Result<code_repo_wiki::config::schema::WikiConfig> {
    let mut cfg = code_repo_wiki::load_config_rooted(config, root)?;
    if let Some(out) = output {
        cfg.output_dir = Some(root.path().join(out));
    }
    Ok(cfg)
}

fn main() -> anyhow::Result<()> {
    // t03 契约（v21 实证发现）：tracing_subscriber::fmt() 默认 writer 是
    // stdout——所有日志会混入业务 stdout，破坏外部 AI Coding Agent 的
    // stdout 事实源（update 一次几十行 info 日志）。显式切到 stderr：
    // stdout 只承载业务输出（println），日志（info/warn/error）全部走
    // stderr，两者可独立解析。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Generate {
            config,
            output,
            force,
            progress_json,
            root,
            wait,
            skip_if_locked,
        } => {
            let root = resolve_root(root.as_deref())?;
            // Phase 15.2：--wait/--skip-if-locked 转 LockOptions（锁冲突策略见
            // lib.rs::LockOptions；--wait 0 视为立即超时=不等待）
            let lock = code_repo_wiki::LockOptions {
                wait: wait.map(std::time::Duration::from_secs),
                skip_if_locked,
            };
            // v44：文本模式也走进度事件流（run_pipeline_with_progress）——
            // 阶段行输出到 stderr（tracing 日志流，不污染 stdout 业务输出，
            // 对齐 clig.dev「messaging to stderr」约定；非 TTY/CI 下同样是
            // 普通文本行，无动画）；完成摘要走 stdout（状态变更告知，AI
            // 事实源可解析）。--progress-json 保持原样（插件流式解析）。
            let started = std::time::Instant::now();
            let result = if progress_json {
                // JSONL 进度输出：插件 wiki_generate 流式解析。
                // v46：新增 current/total 字段（LLM 逐项进度；无项级时为 null）
                code_repo_wiki::run_pipeline_with_progress(
                    config.as_deref(),
                    output.as_deref(),
                    force,
                    &root,
                    &code_repo_wiki::GenerationMode::Full,
                    lock,
                    &progress_json_cb(),
                )?
            } else {
                // v46：文本模式 TTY 感知渲染——终端下行内刷新（\r 原地更新），
                // 非终端（管道/CI/后台）节流输出普通文本行；均走 stderr
                //（clig.dev 约定），不污染 stdout 业务输出
                code_repo_wiki::run_pipeline_with_progress(
                    config.as_deref(),
                    output.as_deref(),
                    force,
                    &root,
                    &code_repo_wiki::GenerationMode::Full,
                    lock,
                    &text_progress_cb(),
                )?
            };
            // Phase 15.2：--skip-if-locked 命中（锁冲突且未等到）→ 退出码 0
            // 跳过，不打印「生成完成」等误导文案（stdout 契约行见注释）
            if result.skipped {
                if progress_json {
                    // progress_json：跳过也走 JSONL 事件行（与 no-op 同构）
                    println!(r#"{{"stage":"skipped"}}"#);
                } else {
                    println!("另一实例正在运行，已按 --skip-if-locked 跳过");
                }
                return Ok(());
            }
            // progress_json：完成摘要也走 JSONL（与事件行同构，插件流式解析
            // 不被纯文本行污染）；文本模式保持原摘要行
            if progress_json {
                println!(
                    r#"{{"stage":"done","files":{},"entities":{},"documents":{},"elapsed_secs":{}}}"#,
                    result.stats.files_scanned,
                    result.stats.total_entities,
                    result.documents.len(),
                    started.elapsed().as_secs()
                );
            } else {
                println!(
                    "✓ 生成完成: 扫描 {} 个文件 / {} 个实体 / {} 页文档（{}s）",
                    result.stats.files_scanned,
                    result.stats.total_entities,
                    result.documents.len(),
                    started.elapsed().as_secs()
                );
            }
        }
        Commands::Update {
            config,
            output,
            force,
            progress_json,
            dry_run,
            root,
            wait,
            skip_if_locked,
        } => {
            // update 命令无外部 watch 事件，watch_paths 传空、change_kind 传 None
            let root = resolve_root(root.as_deref())?;
            // Phase 15.2：--wait/--skip-if-locked 转 LockOptions（--dry-run 不持锁
            // 不受影响，见下方 dry_run 早退分支）
            let lock = code_repo_wiki::LockOptions {
                wait: wait.map(std::time::Duration::from_secs),
                skip_if_locked,
            };
            // v17 t07：--dry-run 只做变更分析预览，不执行生成（无副作用）。
            // 与 run_pipeline 的差异：跳过 LLM 与渲染，只输出将更新的文件/
            // 模块清单——用户可先预览再决定是否真正执行。
            if dry_run {
                let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
                // scan_and_parse_at 返回 ScanOutput（v13 B5），取 insights 喂下游
                let scan = code_repo_wiki::ingest::scan_and_parse_at(&root)?;
                let graph = code_repo_wiki::analysis::build_graph(&scan.insights)?;
                let inc = code_repo_wiki::incremental::run_incremental_update_at(
                    &root,
                    &scan.insights,
                    &graph,
                    &cfg,
                    &[],
                )?;
                println!(
                    "--dry-run: {} 个文件变更, {} 个模块受影响（未执行生成）",
                    inc.changed_files.len(),
                    inc.affected_modules.len()
                );
                for f in &inc.changed_files {
                    println!("  变更: {}", f.display());
                }
                for m in &inc.affected_modules {
                    println!("  受影响模块: {m}");
                }
                return Ok(());
            }
            // v44：update 文本模式与 generate 同构——阶段行走 stderr，
            // 完成摘要走 stdout（no-op 早退分支保持「无文件变更」契约行，
            // 不打印摘要——见下方分支判断）
            let started = std::time::Instant::now();
            let result = if progress_json {
                // JSONL 进度输出：与 generate --progress-json 同构，供插件流式解析
                code_repo_wiki::run_pipeline_with_progress(
                    config.as_deref(),
                    output.as_deref(),
                    force,
                    &root,
                    &code_repo_wiki::GenerationMode::Incremental {
                        watch_paths: Vec::new(),
                        change_kind: None,
                    },
                    lock,
                    &progress_json_cb(),
                )?
            } else {
                code_repo_wiki::run_pipeline_with_progress(
                    config.as_deref(),
                    output.as_deref(),
                    force,
                    &root,
                    &code_repo_wiki::GenerationMode::Incremental {
                        watch_paths: Vec::new(),
                        change_kind: None,
                    },
                    lock,
                    &text_progress_cb(),
                )?
            };
            // Phase 15.2：--skip-if-locked 命中（锁冲突且未等到）→ 退出码 0
            // 跳过，不打印完成/ no-op 等误导文案；与 generate 跳过语义一致
            if result.skipped {
                if progress_json {
                    // progress_json：跳过也走 JSONL 事件行（与 noop 同构）
                    println!(r#"{{"stage":"skipped"}}"#);
                } else {
                    println!("另一实例正在运行，已按 --skip-if-locked 跳过");
                }
                return Ok(());
            }
            // t03（v21）：no-op 早退出口的 stdout 契约——lib.rs 在扫描前即
            // 判定"无文件变更"并返回空结果（documents 空且扫描 0 文件，
            // 与"空 diff 但已扫描"的普通更新路径可唯一区分）。外部 AI
            // Coding Agent 以 stdout 为事实源：两种结果必须可区分——
            // "增量更新完成"只属于真实执行路径，跳过时向 stdout 打印
            // 明确消息且不再打印完成行（跳过细节仍走 stderr tracing）。
            if result.documents.is_empty() && result.stats.files_scanned == 0 {
                if progress_json {
                    // progress_json：no-op 也走 JSONL 事件行，插件流式解析不被纯文本行污染
                    println!(r#"{{"stage":"noop"}}"#);
                } else {
                    println!("无文件变更，跳过更新（no-op）");
                }
            } else if progress_json {
                // progress_json：完成摘要与事件行同构（stage:"done"），文本模式保持原摘要行
                println!(
                    r#"{{"stage":"done","files":{},"documents":{},"elapsed_secs":{}}}"#,
                    result.stats.files_scanned,
                    result.documents.len(),
                    started.elapsed().as_secs()
                );
            } else {
                println!(
                    "✓ 增量更新完成: 扫描 {} 个文件 / {} 页文档（{}s）",
                    result.stats.files_scanned,
                    result.documents.len(),
                    started.elapsed().as_secs()
                );
            }

            // D2（N2）：update 尾部一致性校验——复用 lint 全部检查对产物做
            // 全量复核（本轮受影响页 + 存量页）。增量更新只重建受影响模块，
            // 跨页一致性问题（断链/引用漂移/符号漂移等）可能残留，此处让
            // 用户立即可见；只告警不改变退出码（"失败只告警"策略——产物
            // 缺陷由 lint 门禁兜底拦截，update 主流程语义不受影响）。
            // audit-cli-01：复核目录必须与流水线注入的 --output 一致——此前
            // load_config_rooted 漏传 output，--output 下扫默认目录静默假阴性。
            let cfg = load_config_with_cli_output(config.as_deref(), output.as_deref(), &root)?;
            let output_dir = cfg.output_dir();
            let source_roots = code_repo_wiki::commands::source_roots(&root);
            let issues = code_repo_wiki::output::lint::lint(output_dir, &source_roots);
            // v14 D 组（t05 拍板）：语义一致性检查（LLM 跨页矛盾，变更驱动——
            // 只查本次 update 生成的受影响页；LLM 不可用/失败时"只告警"跳过，
            // 语义检查是增强项，静态 lint 已覆盖机械问题）
            if let Err(e) = code_repo_wiki::output::semantic_lint::check_semantic_consistency(
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
                    "update 完成后产物检查发现 {} 个问题（不阻断本次更新，详情可用 `code-repo-wiki lint` 查看）:",
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
            // 产物目录与 generate 同基准：load_config_rooted 统一将相对 output.dir
            // 解析到 root（v17 F 组）——指纹键形态必须与生成状态键一致，否则
            // 人工修改保护检测失效（旧实现 sync 相对 cwd 解析，root 化后错位）。
            let root = resolve_root(root.as_deref())?;
            let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
            // audit-cli-09：sync 写 generation_state.json（指纹库），与 generate/
            // update/watch 并发会互相覆盖状态（最后写入者胜）——与 card 同构
            // 纳入运行锁（Phase 15.4 语义）。sync 无 --wait/--skip-if-locked
            // 选项，用 default()（冲突立即失败，非 0 退出码）。Skipped 仅在
            // skip_if_locked=true 时出现，default() 下不可达，防御性 bail。
            let _run_lock = match code_repo_wiki::fs::acquire_run_lock_with_options(
                &cfg,
                &code_repo_wiki::LockOptions::default(),
            )? {
                code_repo_wiki::fs::LockAcquire::Acquired(run_lock) => run_lock,
                code_repo_wiki::fs::LockAcquire::Skipped => {
                    anyhow::bail!("运行锁被另一实例占用，本次同步跳过")
                }
            };
            code_repo_wiki::commands::sync_from_git(cfg.output_dir())?;
            tracing::info!(
                "同步完成 (--config {})",
                config
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "默认链".into())
            );
        }
        Commands::Status { config, root } => {
            // --root 提供时以 root 为产物目录基准（跨 cwd 运行 status 能定位正确产物；
            // 缺省 root=cwd 行为不变）
            let root = resolve_root(root.as_deref())?;
            let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
            // 实际生效的配置文件路径：显式指定即其本身；缺省走三链
            // （项目级 config.toml → 用户级 config.toml → 创建用户级）
            let cfg_path = match config.as_deref() {
                Some(p) => p.to_path_buf(),
                None => code_repo_wiki::config::load_default_config(&root)?.0,
            };
            tracing::info!("配置加载成功: {}", cfg_path.display());
            let report = code_repo_wiki::commands::status_report(&cfg, &root);
            // ready 才报告页面统计与 lint 结果；未生成时引导运行 generate
            if report.ready {
                println!("Wiki 状态: 就绪");
                println!("配置文件: {}", cfg_path.display());
                println!("页面: {} 张，卡片: {} 张", report.wiki_pages, report.cards);
                // v32 10.1：语义索引状态显式行（读降级标记；有标记=已降级+原因）
                match code_repo_wiki::semantic_degraded_reason(&cfg) {
                    Some(reason) => println!("语义索引: 已降级（原因: {}）", reason.trim()),
                    None => println!("语义索引: 正常"),
                }
                // v36 D3：LLM 状态显式行——provider=mock 意味着未配置真实
                // LLM Key，页面内容由模板生成（无 AI 润色）；显式提示防
                // 止用户误以为已接入真实模型（静默降级是「不操心」的反面）
                if cfg.llm.provider == code_repo_wiki::config::schema::LlmProviderType::Mock {
                    println!("LLM: 已降级（mock 模拟——未配置真实 LLM Key，页面由模板生成）");
                } else {
                    println!("LLM: 正常");
                }
                // lint 产物健康检查结果（与 lint 命令同格式，error 级问题退出码非 0；
                // 告警级仅展示不阻断——与 lint 命令退出码语义一致）
                for issue in &report.issues {
                    println!("- [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
                if report.issues.iter().any(|i| !i.is_warning()) {
                    anyhow::bail!("status: 发现 {} 个问题", report.issues.len());
                }
            } else {
                println!("Wiki 状态: 未生成（运行 code-repo-wiki generate）");
                println!("配置文件: {}", cfg_path.display());
            }
        }
        Commands::Lint { config, root } => {
            // lint 检查产物健康:孤儿页/断链/过时;三态退出码（v17 t07，docverity
            // 模式）：0 = 干净 / 1 = 有发现问题 / 2 = 工具问题（配置加载失败、
            // 目录缺失——防止配置 typo 掩盖绿构建）
            // --root 提供时以 root 为产物目录基准（同 status）。
            let root = match resolve_root(root.as_deref()) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("lint: 工具问题: {}", e);
                    std::process::exit(2);
                }
            };
            let cfg = match code_repo_wiki::load_config_rooted(config.as_deref(), &root) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("lint: 工具问题（配置加载失败）: {}", e);
                    std::process::exit(2);
                }
            };
            let output_dir = cfg.output_dir();
            // v30+：源码根恒为仓库根（扫描范围已硬编码为全量遍历+内置过滤）
            let source_roots = code_repo_wiki::commands::source_roots(&root);
            let issues = code_repo_wiki::output::lint::lint(output_dir, &source_roots);
            if issues.is_empty() {
                println!("lint: 通过，无孤儿页/断链/过时问题");
            } else if issues.iter().all(|i| i.is_warning()) {
                // 仅告警级（severity==Warning：orphan / bad-mermaid /
                // entity-ownership 归属未确认）：展示但不阻断退出码（CI 门禁
                // 语义——error 级问题才导致退出码非 0）
                for issue in &issues {
                    println!("lint [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
            } else {
                for issue in &issues {
                    println!("lint [{}] {}: {}", issue.kind, issue.path, issue.message);
                }
                std::process::exit(1);
            }
        }
        Commands::Doctor { config, root } => {
            // 六查诊断（配置/产物可写/输出状态/Key/网络/版本漂移），逐项输出；
            // 任一失败退出码 1（与 lint 三态同族，供脚本门禁）
            let root = resolve_root(root.as_deref())?;
            let checks = code_repo_wiki::doctor::run(config.as_deref(), &root)?;
            for c in &checks {
                println!("[{}] {}", if c.ok { "✓" } else { "✗" }, c.name);
                if let Some(detail) = &c.detail {
                    println!("     {detail}");
                }
            }
            let failed = checks.iter().filter(|c| !c.ok).count();
            if failed > 0 {
                anyhow::bail!("doctor: {} 项未通过", failed);
            }
        }
        Commands::Key { env, config, root } => {
            // key：交互式配置 LLM API key。写入目标固定为用户级
            // config.toml——安全底线：明文凭据绝不写项目级
            // config.toml（随 Git 共享）。--config 仅用于读取 provider
            // 判定（如项目级 provider=mock 时提示无需 key）。
            let root = resolve_root(root.as_deref())?;
            code_repo_wiki::key::run(env, config.as_deref(), &root)?;
        }
        Commands::AstSearch {
            symbol,
            language,
            config,
            json,
            root,
        } => {
            // AST 精确符号查找：不依赖搜索索引，直接扫描源文件解析 AST 定位定义
            let root = resolve_root(root.as_deref())?;
            // audit-cli-02：--language 非法值此前被静默吞掉——execute_ast_search
            // 内 AstQuery::new 失败走 continue，退出码 0 且误报「未找到符号」
            // （假阴性）。显式校验非法语言 → 非 0 退出码；get_language 与
            // AST 扫描解析同源（同一支持语言集）。
            if let Some(lang) = &language {
                code_repo_wiki::search::ast::get_language(lang)?;
            }
            let results = code_repo_wiki::execute_ast_search(
                config.as_deref(),
                &root,
                &symbol,
                language.as_deref(),
            )?;
            if json {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .map(|hit| {
                        serde_json::json!({
                            "name": hit.node.name,
                            "kind": hit.node.kind.as_str(),
                            "file": hit.node.file_path,
                            "lines": hit.node.line_range,
                            "signature": hit.node.signature,
                            "source": hit.source,
                        })
                    })
                    .collect();
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
        Commands::Export {
            config,
            output,
            skip_generate,
            root,
        } => {
            let root = resolve_root(root.as_deref())?;
            // audit-cli-08：--skip-generate 下 --output 此前静默忽略——快照与
            // 产物目录绑定（快照写在 cfg.output_dir()），--output 无落点，
            // 静默吞掉会误导用户以为导出到了指定目录，显式报错防误用。
            if skip_generate && output.is_some() {
                anyhow::bail!(
                    "--skip-generate 从导出快照恢复导出，--output 不生效（快照绑定配置文件 output.dir）；请移除 --output 或去掉 --skip-generate"
                );
            }
            let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
            if skip_generate {
                // 从导出快照恢复导出（票 06）：不重跑生成流水线。
                // render_all 每次写盘后同步写 .state/export_snapshot.json，
                // 快照缺失时明确报错（不静默回退重生成——回退会掩盖
                // 快照契约被破坏的事实）。
                let snapshot_path = code_repo_wiki::output::export_snapshot_path(cfg.output_dir());
                // 票 04 陈旧检测：快照 mtime 早于任一 wiki 页 mtime = 产物在
                // 快照之后被更新（快照写入失败/被外部改动/产物被手动编辑），
                // 继续导出会静默输出过期内容——显式报错引导重新生成。
                if let (Ok(snapshot_mtime), Some(latest_page)) = (
                    std::fs::metadata(&snapshot_path).and_then(|m| m.modified()),
                    code_repo_wiki::output::latest_wiki_page_mtime(cfg.output_dir()),
                ) && snapshot_mtime < latest_page
                {
                    anyhow::bail!(
                        "导出快照过期（快照写入时间早于最新 wiki 页），请重新运行 `code-repo-wiki generate` 或 `code-repo-wiki update` 后再导出"
                    );
                }
                let content = std::fs::read_to_string(&snapshot_path).with_context(|| {
                    format!(
                        "导出快照不存在，请先运行 `code-repo-wiki generate` 或 `code-repo-wiki update`: {}",
                        snapshot_path.display()
                    )
                })?;
                let snapshot: code_repo_wiki::output::ExportSnapshot =
                    serde_json::from_str(&content).with_context(|| "解析导出快照失败")?;
                // 票 10：快照版本契约校验——未来格式演进时旧版本可被
                // 显式拒绝（当前仅版本 1；缺失字段的旧文件会被 serde
                // 默认值补齐后误读，故版本不符必须硬性报错而非容错）
                if snapshot.version != 1 {
                    anyhow::bail!(
                        "导出快照版本 {} 不受支持（当前支持: 1），请重新运行 `code-repo-wiki generate` 或 `code-repo-wiki update`",
                        snapshot.version
                    );
                }
                code_repo_wiki::output::html::export_html(
                    &snapshot.documents,
                    &snapshot.cards,
                    &snapshot.modules,
                    &cfg,
                )?;
            } else {
                let result = code_repo_wiki::run_pipeline(
                    config.as_deref(),
                    output.as_deref(),
                    false,
                    &root,
                    &code_repo_wiki::GenerationMode::Full,
                )?;
                code_repo_wiki::output::html::export_html(
                    &result.documents,
                    &result.cards,
                    &code_repo_wiki::output::export_modules(&result.graph, &result.cards),
                    &cfg,
                )?;
            }
            tracing::info!(
                "HTML 导出完成 (--config {})",
                config
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "默认链".into())
            );
        }
        Commands::Note { text, config, root } => {
            // --root 提供时以 root 为产物目录基准（同 status/lint）
            let root = resolve_root(root.as_deref())?;
            let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
            code_repo_wiki::commands::append_note(cfg.output_dir(), &cfg.wiki.language, &text)?;
            tracing::info!(
                "知识记录已写入 (--config {})",
                config
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "默认链".into())
            );
        }
        Commands::Watch { config, root } => {
            let root = resolve_root(root.as_deref())?;
            // v36 D5 + v13.2：watch 常驻自愈（有界）——监听循环崩溃（notify
            // 初始化失败/事件循环错误）时指数退避自动重启（5s 起、上限 60s，
            // 封顶 10 次）；锁冲突（真并发或保守报错）重启无意义，立即退出
            // 非零码。Ctrl-C 优雅退出（run_watch 返回 Ok）直接结束。
            // watch 是「自动维护」的最后一环：崩溃后静默消失会让 wiki
            // 从此停更（无人知道）——有界自愈兜住缺口且不永久刷日志。
            // v13.2：自愈有界化——锁冲突（真并发/读失败保守报错）重启无意义，
            // 立即退出；非锁错误指数退避重试（5s→60s），WATCH_RETRY_MAX 次后
            // 放弃退出非零码——无限重启只会永久刷日志。
            const WATCH_RETRY_MAX: u32 = 10;
            let mut delay = std::time::Duration::from_secs(5);
            let mut attempts: u32 = 0;
            loop {
                match code_repo_wiki::run_watch(config.as_deref(), &root) {
                    Ok(()) => break,
                    Err(e) => {
                        // audit-srch2-04：锁冲突判定用类型匹配（LockError）而非
                        // 文案 contains——报错措辞调整会静默破坏 watch 自愈判定
                        if code_repo_wiki::fs::is_lock_conflict(&e) {
                            eprintln!(
                                "code-repo-wiki: 运行锁冲突（另一实例或保守报错），watch 退出: {e}"
                            );
                            std::process::exit(1);
                        }
                        attempts += 1;
                        if attempts >= WATCH_RETRY_MAX {
                            eprintln!(
                                "code-repo-wiki: watch 连续失败 {WATCH_RETRY_MAX} 次，放弃自动重启: {e}"
                            );
                            std::process::exit(1);
                        }
                        eprintln!("code-repo-wiki: watch 监听循环异常退出: {e}");
                        eprintln!(
                            "code-repo-wiki: {} 秒后自动重启监听（第 {attempts}/{WATCH_RETRY_MAX} 次，Ctrl-C 退出）",
                            delay.as_secs()
                        );
                        std::thread::sleep(delay);
                        delay = std::time::Duration::from_secs((delay.as_secs() * 2).min(60));
                    }
                }
            }
        }
        Commands::Search {
            query,
            top_k,
            config,
            json,
            engine,
            root,
        } => {
            // 解析引擎类型：CLI 参数经 clap ValueEnum 校验（非法值在解析期
            // 报错退出码 2，help 列出 possible values），此处仅回退默认常量
            // SEARCH_DEFAULT_ENGINE（v36 起为 Hybrid；hybrid 无 embed key 时
            // 自动降级纯 text）
            let root = resolve_root(root.as_deref())?;
            let engine_type =
                engine.unwrap_or(code_repo_wiki::config::schema::SEARCH_DEFAULT_ENGINE);
            // CLI 显式 -k 优先，未传时回退硬编码默认 SEARCH_DEFAULT_TOP_K
            // N17：top_k 下限收敛到 1（top_k=0 的搜索调用无意义，返回空结果）
            let top_k = top_k
                .unwrap_or(code_repo_wiki::config::schema::SEARCH_DEFAULT_TOP_K)
                .max(1);
            let results = code_repo_wiki::execute_search(
                config.as_deref(),
                &root,
                &query,
                top_k,
                &engine_type,
            )?;
            if json {
                // JSON 格式输出（供 OpenCode 插件解析）
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .map(|hit| {
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
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_results)?);
            } else {
                // 表格格式输出（人类可读）
                if results.is_empty() {
                    println!("未找到匹配结果");
                } else {
                    println!(
                        "{:<4} {:<30} {:<12} {:<8} 文件",
                        "#", "名称", "类型", "分数"
                    );
                    println!("{}", "-".repeat(80));
                    for (i, hit) in results.iter().enumerate() {
                        let file = hit.node.file_path.as_deref().unwrap_or("-");
                        println!(
                            "{:<4} {:<30} {:<12} {:<8.2} {}",
                            i + 1,
                            hit.node.name,
                            hit.node.kind.as_str(),
                            hit.score,
                            file
                        );
                    }
                }
                // v32 10.1：语义索引降级显式提示（文本模式）——降级标记由
                // 最近一次 generate/update 写入（见 lib.rs 标记区）；有标记
                // 时提示原因，避免用户误以为语义结果可用。load_config_rooted
                // 失败（配置缺失/损坏）与无标记一样静默——搜索本身已成功，
                // 提示只是附加信息，不让配置错误打断结果展示。
                let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root).ok();
                if let Some(reason) = cfg.and_then(|c| code_repo_wiki::semantic_degraded_reason(&c))
                {
                    println!("语义索引已降级（原因: {}）", reason.trim());
                }
            }
        }
        Commands::Install {
            claude,
            codex,
            dsh,
            root,
        } => {
            // v25 起 init 并入 install：先确保用户级默认配置就绪
            // （缺失自动创建，含项目级 config.toml 覆盖链语义），
            // 再执行集成安装（v33 合并版：OpenCode 插件 + 多 Agent MCP
            // + AGENTS.md + git hooks；--claude/--codex/--dsh 扩展）。
            let root = resolve_root(root.as_deref())?;
            let (source, _config) = code_repo_wiki::config::load_default_config(&root)?;
            // 配置链解析完成（来源可能是用户级或项目级 config.toml——
            // 项目级存在时优先，用户级缺失不自动创建，见 load_default_config）
            tracing::info!("配置链就绪（来源: {}）", source.display());
            let opts = code_repo_wiki::commands::InstallOptions {
                claude,
                codex,
                dsh,
            };
            code_repo_wiki::commands::install(&root, &opts)?;
        }
        Commands::Uninstall { force, root } => {
            let root = resolve_root(root.as_deref())?;
            code_repo_wiki::commands::uninstall(force, &root)?;
        }
        Commands::Mcp { config, root } => {
            // MCP stdio server：阻塞直到客户端断开。异步运行时由库内
            // get_global_runtime 提供（与流水线共用，避免二次初始化）。
            let root = resolve_root(root.as_deref())?;
            let rt = code_repo_wiki::get_global_runtime();
            rt.block_on(code_repo_wiki::mcp::serve_stdio(config.as_deref(), root))?;
        }
        Commands::Card {
            action,
            root,
            wait,
            skip_if_locked,
        } => {
            use code_repo_wiki::generate::card as card_cmd;
            // Phase 15.4：card 写卡片同样纳入运行锁；--wait/--skip-if-locked
            // 与 generate/update 同构（Phase 15.2）转 LockOptions
            let lock = code_repo_wiki::LockOptions {
                wait: wait.map(std::time::Duration::from_secs),
                skip_if_locked,
            };
            // CLI 枚举转业务枚举（config 路径在匹配时提取，供 run_card_command 使用）
            let (config, action) = match action {
                CardAction::Generate { module, config } => {
                    (config, card_cmd::CardAction::Generate { module })
                }
                CardAction::Modify {
                    module,
                    instruction,
                    reference,
                    config,
                } => (
                    config,
                    card_cmd::CardAction::Modify {
                        module,
                        instruction,
                        references: reference,
                    },
                ),
                CardAction::Supplement {
                    module,
                    instruction,
                    reference,
                    config,
                } => (
                    config,
                    card_cmd::CardAction::Supplement {
                        module,
                        instruction,
                        references: reference,
                    },
                ),
                CardAction::Rewrite {
                    module,
                    instruction,
                    reference,
                    config,
                } => (
                    config,
                    card_cmd::CardAction::Rewrite {
                        module,
                        instruction,
                        references: reference,
                    },
                ),
            };
            let root = resolve_root(root.as_deref())?;
            code_repo_wiki::run_card_command(config.as_deref(), &root, &action, lock)?;
        }
        Commands::Bench {
            root,
            repo_name,
            config,
            json,
            judge,
            rubrics_only,
            repodoc,
            reference,
        } => {
            // 评测基准（U10）：五维自动评测。root 必填（评测对象仓库根），
            // root 经 resolve_root 校验目录存在性（N7）。config 缺省走默认
            // 配置链（E 组：项目级 → 全局 → 创建全局）；repo_name 缺省取
            // root 目录名。
            // Update Recall 回放前有工作区干净检查（安全闸，事故教训），
            // 脏工作区会明确报错拒绝评测。
            // --rubrics-only（v21 D 组）：跳过回放只做裁判层——大仓库
            // 评测成本控制（clap conflicts_with 已保证与 --judge 互斥）。
            // --repodoc（v32 6.4 FR-101）：RepoDocBench 对齐五维报告，
            // 强制 LLM 裁判（judge 提升）并在文本模式前置五维摘要。
            let root = resolve_root(Some(&root))?;
            let cfg = code_repo_wiki::load_config_rooted(config.as_deref(), &root)?;
            let repo_name = repo_name.unwrap_or_else(|| {
                root.path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string())
            });
            // v32（6.4）：--repodoc 隐含 judge=true（五维含 TQS LLM 裁判）
            let judge = judge || repodoc;
            let report = if rubrics_only {
                code_repo_wiki::bench::run_rubrics_only(&root, &cfg, &repo_name, &reference)?
            } else {
                code_repo_wiki::bench::run_bench(&root, &cfg, &repo_name, judge, &reference)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                // v32（6.4）：--repodoc 文本模式前置五维聚合摘要
                if repodoc {
                    print!("{}", code_repo_wiki::bench::render_repodoc(&report));
                }
                println!("{}", code_repo_wiki::bench::render_markdown(&report));
            }
        }
        Commands::BenchManifest {
            manifest,
            config,
            json,
            work_dir,
        } => {
            // 清单批量跑分（v21 E 组）：模板配置只取 scope/llm/provider 等
            // 语义字段，产物目录按仓库覆盖为 work_dir/<name>-out/。
            // work_dir 缺省系统临时目录（远程 clone 落地）。
            let work_dir = work_dir
                .unwrap_or_else(|| std::env::temp_dir().join("code-repo-wiki-bench-manifest"));
            let cfg = match config {
                Some(path) => code_repo_wiki::load_config_rooted(
                    Some(&path),
                    &code_repo_wiki::project::ProjectRoot::new(std::env::current_dir()?),
                )?,
                None => {
                    // 缺省走默认加载链（项目级 → 用户级 → 创建用户级）
                    let root = code_repo_wiki::project::ProjectRoot::new(std::env::current_dir()?);
                    code_repo_wiki::load_config_rooted(None, &root)?
                }
            };
            let entries = code_repo_wiki::bench::manifest::parse_manifest(&manifest)?;
            let report = code_repo_wiki::bench::manifest::run_manifest(&entries, &cfg, &work_dir)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}",
                    code_repo_wiki::bench::manifest::render_manifest_markdown(&report)
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(
        stage: &'static str,
        percent: u8,
        current: Option<u32>,
        total: Option<u32>,
    ) -> code_repo_wiki::ProgressEvent {
        code_repo_wiki::ProgressEvent {
            stage,
            percent,
            current,
            total,
        }
    }

    #[test]
    fn test_render_progress_non_tty_throttles() {
        let mut st = ProgressRenderState::default();
        // 首事件必打印（阶段切换）
        let s1 = render_progress(&ev("scanning", 10, None, None), false, &mut st);
        assert!(s1.is_some());
        assert!(s1.unwrap().contains("进度 [扫描源码] 10%"));
        // 同阶段同档：跳过
        assert!(render_progress(&ev("scanning", 10, None, None), false, &mut st).is_none());
        // 阶段切换：必打印
        let s2 = render_progress(&ev("analyzing", 25, None, None), false, &mut st).unwrap();
        assert!(s2.contains("进度 [构建知识图谱] 25%"));
        // 项级进度：进入 cards 阶段首事件打印（0/10）
        let s3 = render_progress(&ev("cards", 62, Some(0), Some(10)), false, &mut st).unwrap();
        assert!(s3.contains("进度 [生成知识卡片] 0/10（62%）"));
        // 同 quarter（0..=4）：跳过
        assert!(render_progress(&ev("cards", 62, Some(2), Some(10)), false, &mut st).is_none());
        // 跨 quarter（5/10）：打印
        let s4 = render_progress(&ev("cards", 62, Some(5), Some(10)), false, &mut st).unwrap();
        assert!(s4.contains("5/10（62%）"));
        // 百分比跨 10 档（80%）：打印
        let s5 = render_progress(&ev("cards", 80, Some(10), Some(10)), false, &mut st).unwrap();
        assert!(s5.contains("10/10（80%）"));
        // done：非 TTY 无残留（摘要行由命令 stdout 打印）
        assert!(render_progress(&ev("done", 100, None, None), false, &mut st).is_none());
    }

    #[test]
    fn test_render_progress_tty_inline() {
        let mut st = ProgressRenderState::default();
        // 首事件：行内刷新（无前导换行）
        let s1 = render_progress(&ev("scanning", 10, None, None), true, &mut st).unwrap();
        assert!(s1.starts_with('\r'));
        // TTY 同事件重复：仍打印（行内原地刷新）
        assert!(render_progress(&ev("scanning", 10, None, None), true, &mut st).is_some());
        // 阶段切换：前导换行
        let s2 = render_progress(&ev("analyzing", 25, None, None), true, &mut st).unwrap();
        assert!(s2.starts_with("\n\r"));
        // 项级进度文本
        let s3 = render_progress(&ev("cards", 62, Some(0), Some(10)), true, &mut st).unwrap();
        assert!(s3.contains("0/10（62%）"));
        // done：清行清理残留
        assert_eq!(
            render_progress(&ev("done", 100, None, None), true, &mut st),
            Some("\r\u{1b}[K".to_string())
        );
    }
}
