use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::schema::WikiConfig;
use crate::incremental::state::GenerationState;
use crate::output::lint::{lint, LintIssue};

/// status 报告（结构化，main 只做格式化）
pub struct StatusReport {
    pub ready: bool,            // 产物根存在且非空
    pub wiki_pages: usize,      // wiki/{lang}/*.md 数量（所有语言合计）
    pub cards: usize,           // cards/{lang}/*.md 数量
    pub issues: Vec<LintIssue>, // lint 产物健康检查结果
    pub config_path: String,
}

/// 汇总产物状态：页面/卡片数量 + lint 健康检查（供 `code-repo-wiki status` 使用）
///
/// 目录不存在时数量计 0、lint 无问题，不算缺陷（未生成也是合法状态）。
pub fn status_report(config: &WikiConfig, root: &crate::project::ProjectRoot) -> StatusReport {
    let output_dir = config.output_dir();
    // ready = wiki 目录存在且含 .md 文件（有产物才算生成过）
    let wiki_pages = collect_md_files(&output_dir.join("wiki")).len();
    let cards = collect_md_files(&output_dir.join("cards")).len();
    // 源码根必须相对 root 解析（见 source_roots）：status 跨 cwd 运行时
    // （--root 指向其他仓库）lint 才能扫到目标仓库
    let issues = lint(
        output_dir,
        &source_roots(root),
    );
    StatusReport {
        ready: wiki_pages > 0,
        wiki_pages,
        cards,
        issues,
        config_path: config.output_dir().to_string_lossy().into_owned(),
    }
}

/// 源码根（v30+：扫描范围已硬编码为全量遍历+内置过滤，源码根恒为仓库根）
///
/// lint 过时检查需要对比源文件 mtime，空根会导致检查静默跳过；
/// main.rs 的 lint 命令与 status 共用此派生，避免两处内联逻辑漂移。
pub fn source_roots(root: &crate::project::ProjectRoot) -> Vec<PathBuf> {
    vec![root.path().to_path_buf()]
}

/// 将产物目录（wiki/{lang}/、cards/{lang}/）的工作区内容同步到指纹库
///
/// 官方语义：Git 目录中直接编辑 .md 后，手动触发"同步"以 Git 内容为准
/// 合入 Wiki（团队 pull 共享知识的场景）。对每个 .md 产物：
/// - 指纹不存在 → 记录新指纹（视为新文件）
/// - 指纹不匹配 → 工作区内容为准，更新指纹
/// - 受保护页面（protected_docs 含该路径）→ 跳过，保留人工版
///
/// 本质上 = 重新加载工作区 .md 内容到指纹库，不触发任何 LLM 生成。
pub fn sync_from_git(output_dir: &Path) -> Result<()> {
    let state_dir = output_dir.join(".state");
    let state_path = state_dir.join("generation_state.json");
    // 状态不存在（首次 sync）→ 从空状态开始：所有产物视为新文件，全部记录指纹。
    // 状态存在但损坏 → 显式报错（不静默重置：空状态会丢失 protected_docs，
    // 使人工修改保护在后续 update 中失效）。
    let mut state = if !state_path.exists() {
        GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            tool_version: None,
            failed_modules: vec![],
        }
    } else {
        GenerationState::load(&state_dir)
            .with_context(|| format!("状态文件损坏，拒绝静默重置（保护信息会丢失）: {}", state_path.display()))?
    };

    let mut updated = 0usize;
    let mut skipped = 0usize;
    for root in [output_dir.join("wiki"), output_dir.join("cards")] {
        for path in collect_md_files(&root) {
            let path_str = path.to_string_lossy().to_string();
            if state.protected_docs.iter().any(|p| p == &path_str) {
                tracing::warn!("跳过受保护页面（保留人工版）: {}", path_str);
                skipped += 1;
                continue;
            }
            let fp = GenerationState::compute_file_fingerprint(&path)?;
            if state.doc_fingerprints.get(&path_str) != Some(&fp) {
                state.doc_fingerprints.insert(path_str, fp);
                updated += 1;
                tracing::info!("同步指纹（工作区内容为准）: {}", path.display());
            }
        }
    }

    state.save(&state_dir)?;
    tracing::info!("同步完成: 指纹更新 {} 个, 跳过受保护 {} 个", updated, skipped);
    Ok(())
}

/// 追加一条知识沉淀记录到 `{output_dir}/wiki/{lang}/_log.md`（Karpathy log.md 模式）
///
/// 每条记录以 `## YYYY-MM-DD` 节组织，节内按追加顺序编号（当天第 N 条）。
/// _log.md 是人工可读、grep 可查、git 可追踪的追加式会话知识日志——
/// 与 wiki 页（LLM 生成、受保护）分开：人工记录永不被自动生成覆盖。
/// 文件不存在时自动创建；目录不存在时自动创建（与 render_all 的写盘约定一致）。
pub fn append_note(output_dir: &Path, language: &str, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("note 内容不能为空");
    }
    let log_dir = output_dir.join("wiki").join(language);
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("_log.md");

    // 读取现有内容以确定今天的节内已有几条（追加式，不重写历史）
    let existing = std::fs::read_to_string(&log_path).unwrap_or_default();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let today_header = format!("## {date}");
    // 今天节内已有条数 = 文件中今天节之后出现的 "- N." 数量
    let today_seq = existing
        .split(&today_header)
        .nth(1)
        .map(|after| {
            after
                .lines()
                .filter(|l| l.trim().starts_with("- "))
                .count()
        })
        .unwrap_or(0);

    // 追加：无今天节则新建节，否则续写
    let entry = format!("- {}. {text}\n", today_seq + 1);
    let mut append = String::new();
    if !existing.contains(&today_header) {
        // 已有内容不以换行结尾时先补一行（避免与上一个节粘连）
        if !existing.is_empty() && !existing.ends_with('\n') {
            append.push('\n');
        }
        append.push_str(&today_header);
        append.push('\n');
    }
    append.push_str(&entry);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    use std::io::Write;
    file.write_all(append.as_bytes())?;
    tracing::info!("知识记录已追加: {}", log_path.display());
    Ok(())
}

/// 递归收集目录下所有 .md 文件（目录不存在时返回空列表）
fn collect_md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(collect_md_files(&p));
        } else if p.extension().is_some_and(|e| e == "md") {
            out.push(p);
        }
    }
    out
}

/// install 的可选集成（v33 多 Agent 支持）
///
/// 默认集成集 = OpenCode 插件 + OpenCode MCP（用户级全局）+ AGENTS.md
/// + git hooks；以下 flag 扩展集成面。
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// 额外写 Claude Code 项目级 `.mcp.json`（--claude）；v36 起同时
    /// 同步注入 CLAUDE.md（Claude Code 不读 AGENTS.md，注册 MCP 时
    /// 必然需要文档指引——原 --also-claude 开关合并于此）
    pub claude: bool,
    /// 额外写 Codex CLI 用户级配置 `~/.codex/config.toml`（--codex）
    pub codex: bool,
}

/// code-repo-wiki 安装（v33 合并版）：OpenCode 插件 + 多 Agent MCP + AGENTS.md + git hooks
///
/// root 为项目根（U02：--root 注入，替代进程 cwd——插件/hook/config 全部
/// 相对项目根解析，跨 cwd 运行不再错位）。
///
/// 集成步骤（全部幂等，重复执行安全；非 code-repo-wiki 内容一律保留）：
/// 1. OpenCode 插件：`{root}/.opencode/plugins/code-repo-wiki.ts`——模板注入
///    current_exe 绝对路径（t02 摆脱 PATH 依赖）；内容与模板不同即升级
/// 2. OpenCode MCP：用户级全局 `opencode.json` 的 `mcp.code-repo-wiki` 条目
///    （v33 拍板：一次注册所有仓库可用，server 以工作区为 cwd）
/// 3. Claude MCP（--claude）：项目根 `.mcp.json` 的 `mcpServers.code-repo-wiki`
/// 4. Codex MCP（--codex）：用户级 `~/.codex/config.toml` 的
///    `[mcp_servers.code-repo-wiki]` 表
/// 5. AGENTS.md：wiki 引用块（标记对幂等替换；默认执行）
/// 6. CLAUDE.md（随 --claude，v36 起）：Claude Code 不读 AGENTS.md，
///    注册 .mcp.json 时同步注入引用块
/// 7. git hooks：post-commit/post-merge（含 code-repo-wiki 标记则升级覆盖；
///    用户自定义 hook 保留并提示）
///
/// 用户级默认配置的确保由调用方（main.rs）先行执行（v25 语义）。
pub fn install(root: &crate::project::ProjectRoot, opts: &InstallOptions) -> Result<()> {
    let project_root = root.path();

    // MCP 与插件共用当前可执行文件绝对路径（t02：不依赖 PATH）
    let exe_path = std::env::current_exe()
        .context("无法定位当前可执行文件路径（集成无法绑定绝对路径）")?;
    let exe_str = exe_path.to_string_lossy().into_owned();
    let mcp_args = ["mcp".to_string()];

    // 1. OpenCode 插件（项目级；v33：内容比对升级）
    let mut oc = crate::config::opencode::OpenCodeConfig::new(root)
        .context("读取 OpenCode 配置失败")?;
    oc.install_plugin()?;
    if oc.install_plugin_file()? {
        println!("✓ OpenCode 插件已安装");
    } else {
        println!("✓ OpenCode 插件已是最新");
    }

    // 2. OpenCode MCP（用户级全局——v33 拍板）
    let opencode_mcp = crate::config::mcp::OpencodeMcp {
        config_path: crate::config::mcp::OpencodeMcp::global_path()?,
    };
    if opencode_mcp.install("code-repo-wiki", &[exe_str.clone(), mcp_args[0].clone()])? {
        println!("✓ OpenCode MCP 已注册（用户级全局）");
    } else {
        println!("✓ OpenCode MCP 已是最新");
    }

    // 3. Claude MCP（--claude → 项目根 .mcp.json）
    if opts.claude {
        let claude = crate::config::mcp::ClaudeMcp {
            path: crate::config::mcp::ClaudeMcp::project_path(root),
        };
        if claude.install("code-repo-wiki", &exe_str, &mcp_args)? {
            println!("✓ Claude Code MCP 已注册（.mcp.json）");
        } else {
            println!("✓ Claude Code MCP 已是最新（.mcp.json）");
        }
    }

    // 4. Codex MCP（--codex → 用户级 ~/.codex/config.toml）
    if opts.codex {
        let codex = crate::config::mcp::CodexMcp {
            config_path: crate::config::mcp::CodexMcp::global_path()?,
        };
        if codex.install("code-repo-wiki", &exe_str, &mcp_args)? {
            println!("✓ Codex MCP 已注册（~/.codex/config.toml）");
        } else {
            println!("✓ Codex MCP 已是最新（~/.codex/config.toml）");
        }
    }

    // 5/6. AGENTS.md（默认）与 CLAUDE.md（v36 起随 --claude 同步——
    // Claude Code 不读 AGENTS.md，注册 MCP 时文档指引随之注入）
    install_wiki(root, opts.claude)?;

    // 7. git hooks（v33：标记升级，用户自定义保留）
    install_hooks(project_root)?;

    println!("✓ code-repo-wiki 安装完成");
    println!();
    println!("日常使用（傻瓜式全自动，无需记忆命令）：");
    println!("  1. git commit 后 wiki 自动增量更新（post-commit/post-merge hook 已装）");
    println!("  2. 手动一条命令：code-repo-wiki update（首次自动全量生成，之后自动增量；");
    println!("     无变更秒回，失败模块自动补偿重试，尾部自动 lint 复核）");
    println!("  3. 常驻实时模式：code-repo-wiki watch（代码保存即自动更新，Ctrl-C 退出）");
    println!("  4. 健康检查：code-repo-wiki doctor / code-repo-wiki lint");
    Ok(())
}

/// git hook 内容标记（升级判定与 uninstall 删除判定共用的「是否 code-repo-wiki
/// 所有」判据；用户自定义 hook 不含此标记，安装/卸载均不触碰）
pub const HOOK_MARKER: &str = "# code-repo-wiki managed";

/// 生成 hook 脚本内容
///
/// `#!/bin/sh` + LF：Windows 上由 Git for Windows 的 sh 执行（POSIX 语义，
/// 绝非 PowerShell）；`cd` 到仓库顶层保证 --root 无关；`command -v` 探测
/// 二进制存在性；update 失败不阻断 git 主流程（hook 是通知型），但
/// 失败必须可见：stderr 落 .code-repo-wiki/update-error.log 并在提交输出中
/// 提示一行（v36 D2：此前 2>/dev/null || true 把失败完全吞掉，用户
/// 永远不知道 wiki 已陈旧）。
fn hook_content() -> String {
    format!(
        "#!/bin/sh\n{0}: auto-update wiki on commit\ncd \"$(git rev-parse --show-toplevel)\"\ncommand -v code-repo-wiki >/dev/null 2>&1 || exit 0\nmkdir -p .code-repo-wiki\ncode-repo-wiki update 2>>.code-repo-wiki/update-error.log || echo \"code-repo-wiki: wiki 更新失败（详见 .code-repo-wiki/update-error.log）\" >&2\n",
        HOOK_MARKER
    )
}

/// 原子写 hook 并设置执行位（unix；Windows 由 sh 解释执行无需执行位）
fn write_hook(path: &std::path::Path, content: &str) -> Result<()> {
    crate::fs::write_file_atomic(path, content)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    Ok(())
}

/// 安装 git hooks（post-commit/post-merge）
///
/// v33 升级语义：
/// - 不存在 → 新建
/// - 已存在且含 code-repo-wiki 标记（旧模板/本模板）→ 内容不同则升级覆盖，
///   相同则跳过（幂等）
/// - 已存在且无标记（用户/第三方自定义 hook）→ 保留并提示，绝不覆盖
/// - `.git/hooks` 不存在（非 git 仓库）→ 提示跳过（install 不因此失败）
fn install_hooks(project_root: &std::path::Path) -> Result<()> {
    let hooks_dir = project_root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        println!("未检测到 .git 目录，跳过 git hook 安装");
        return Ok(());
    }
    let content = hook_content();
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let existing = std::fs::read_to_string(&hook_path)?;
            if existing.contains("code-repo-wiki") {
                if existing != content {
                    write_hook(&hook_path, &content)?;
                    println!("✓ git {hook_name} hook 已升级");
                } else {
                    println!("✓ git {hook_name} hook 已是最新");
                }
            } else {
                println!("? git {hook_name} hook 已存在且非 code-repo-wiki 内容，保留（未覆盖）");
            }
        } else {
            write_hook(&hook_path, &content)?;
            println!("✓ git {hook_name} hook 已安装");
        }
    }
    Ok(())
}

/// 移除 git hooks（仅 code-repo-wiki 标记的；用户自定义 hook 保留）
///
/// 与 install_hooks 的判定同源（内容含 "code-repo-wiki" 即视为本工具产物——
/// 兼容 v33 前的旧模板：旧内容无 managed 标记但含 code-repo-wiki 调用）。
fn remove_hooks(project_root: &std::path::Path) -> Result<()> {
    let hooks_dir = project_root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        return Ok(());
    }
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
            if content.contains("code-repo-wiki") {
                std::fs::remove_file(&hook_path)?;
                println!("✓ git {hook_name} hook 已移除");
            }
        }
    }
    Ok(())
}

/// code-repo-wiki 卸载（v33 合并版）：移除全部集成痕迹（--force 确认）
///
/// root 为项目根（U02：--root 注入，与 install 对称）。
///
/// 清理集 = install 全集的反向（全部幂等，缺省即跳过）：
/// 1. OpenCode MCP 用户级全局条目（其他 server 保留）
/// 2. OpenCode 插件文件
/// 3. Claude MCP .mcp.json 条目（其他 server 保留；空则删文件）
/// 4. Codex MCP 表（其他表/注释保留）
/// 5. AGENTS.md / CLAUDE.md wiki 块（无标记则跳过）
/// 6. git hooks（仅 code-repo-wiki 标记的删除；用户自定义 hook 保留）
///
/// 保留（设计如此，配置与数据属用户资产）：用户级 config.toml、
/// `.code-repo-wiki/` 产物数据。
pub fn uninstall(force: bool, root: &crate::project::ProjectRoot) -> Result<()> {
    let project_root = root.path();

    if !force {
        println!("警告: 卸载将移除 code-repo-wiki 集成配置（插件/MCP/hook/AGENTS.md 引用块）。");
        println!("保留：用户级 config.toml 与产物数据 .code-repo-wiki/（使用 --force 跳过确认）。");
        anyhow::bail!("请添加 --force 参数确认卸载");
    }

    // 1. OpenCode MCP（用户级全局）
    let opencode_mcp = crate::config::mcp::OpencodeMcp {
        config_path: crate::config::mcp::OpencodeMcp::global_path()?,
    };
    if opencode_mcp.remove("code-repo-wiki")? {
        println!("✓ OpenCode MCP 条目已移除（用户级全局——其他仓库如需继续使用请重新 install）");
    } else {
        println!("✓ OpenCode MCP 条目不存在，跳过");
    }

    // 2. OpenCode 插件
    let mut oc = crate::config::opencode::OpenCodeConfig::new(root)
        .context("读取 OpenCode 配置失败")?;
    oc.uninstall_plugin()?;
    oc.uninstall_plugin_file()?;
    println!("✓ OpenCode 插件已移除");

    // 3. Claude MCP（.mcp.json）
    let claude = crate::config::mcp::ClaudeMcp {
        path: crate::config::mcp::ClaudeMcp::project_path(root),
    };
    if claude.remove("code-repo-wiki")? {
        println!("✓ Claude Code MCP 条目已移除（.mcp.json）");
    } else {
        println!("✓ Claude Code MCP 条目不存在，跳过（.mcp.json）");
    }

    // 4. Codex MCP（~/.codex/config.toml）
    let codex = crate::config::mcp::CodexMcp {
        config_path: crate::config::mcp::CodexMcp::global_path()?,
    };
    if codex.remove("code-repo-wiki")? {
        println!("✓ Codex MCP 条目已移除（~/.codex/config.toml）");
    } else {
        println!("✓ Codex MCP 条目不存在，跳过（~/.codex/config.toml）");
    }

    // 5. AGENTS.md / CLAUDE.md wiki 块
    uninstall_wiki(root)?;

    // 6. git hooks
    remove_hooks(project_root)?;

    println!("✓ code-repo-wiki 卸载完成 (数据保留: .code-repo-wiki/ 与用户级配置)");
    Ok(())
}

/// wiki 引用块的起始标记（注入块的唯一边界）
pub const WIKI_BLOCK_START: &str = "<!-- REPO-WIKI:START -->";

/// wiki 引用块的结束标记
pub const WIKI_BLOCK_END: &str = "<!-- REPO-WIKI:END -->";

/// 渲染注入块模板（install-wiki 与 --also-claude 共用）
///
/// 内容为中文 markdown 指针风格：只引产物路径与常用命令，不复制 wiki
/// 正文（避免与 LLM 生成的产物内容双份漂移）。以换行结尾，保证追加/
/// 替换后与相邻内容衔接干净。
///
/// 产物路径按实际配置渲染（U02）：`output_dir` 与 `lang` 来自目标仓库的
/// config.toml（output.dir / wiki.language）——此前模板硬编码 `wiki/` 与
/// `zh`，默认配置（output.dir=.code-repo-wiki、language 可改）下注入指引失配。
pub fn wiki_block_template(output_dir: &str, lang: &str) -> String {
    format!(
        "\
<!-- REPO-WIKI:START -->
本仓库使用 code-repo-wiki 维护可持续进化的项目 Wiki，产物位于 `{output_dir}/`。

## AI 代理使用指引

1. 先读 `{output_dir}/llms.txt` 定位目标页面（站点地图），再读
   `{output_dir}/wiki/{lang}/overview.md` 与 `{output_dir}/wiki/{lang}/architecture.md`
   建立全局认知，按需深入模块页；上下文预算充足时用 `{output_dir}/llms-full.txt`
   一次获得完整实体骨架。
2. 查找实体（函数/结构体/类）用 `code-repo-wiki search -q \"<关键词>\"`（支持
   text/semantic/hybrid 三引擎，hybrid 含调用链补全）。
3. 修改代码后运行 `code-repo-wiki update` 增量更新；`code-repo-wiki lint` 检查产物健康。
4. 知识沉淀：`code-repo-wiki note \"<记录>\"` 追加到 `{output_dir}/wiki/{lang}/_log.md`。
<!-- REPO-WIKI:END -->
"
    )
}

/// 文档中 wiki 标记对的状态
enum WikiBlockState {
    /// 完整标记对：START 所在行的行首偏移，END 所在行的行尾偏移（含换行）
    Both(usize, usize),
    /// 只出现一个标记（或顺序颠倒）：拒绝自动修复
    Half,
    /// 无任何标记
    None,
}

/// 定位文档中的 wiki 标记对
///
/// - Both: start 对齐到 START 行首、end 对齐到 END 行尾（含换行），
///   使"整块替换/删除"只触碰标记及其之间内容，不伤用户文本。
/// - Half: 只出现 START 或 END 之一，或 END 出现在 START 之前（顺序颠倒）。
///   此时不自动修复——半标记说明文件被人为改坏或与其他工具冲突，
///   修补方向有歧义（删哪半？补哪半？），显式报错让用户处理。
/// - None: 干净状态，可安全追加。
fn wiki_block_state(content: &str) -> WikiBlockState {
    let start = content.find(WIKI_BLOCK_START);
    let end = content.find(WIKI_BLOCK_END);
    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let line_start = content[..s].rfind('\n').map_or(0, |i| i + 1);
            let line_end = content[e..].find('\n').map_or(content.len(), |i| e + i + 1);
            WikiBlockState::Both(line_start, line_end)
        }
        (None, None) => WikiBlockState::None,
        _ => WikiBlockState::Half,
    }
}

/// 将 wiki 引用块注入文档文本（纯函数，不含 I/O，install-wiki / --also-claude 共用）
///
/// 幂等策略：
/// - 完整标记对 → 整块替换（只动标记之间内容，保留用户其他内容）；
/// - 无标记 → 文件尾 trim 后追加（与已有内容之间留一个空行）；
/// - 只有一半标记 → 报错（理由见 `wiki_block_state`）。
pub fn inject_wiki_block(content: &str, block: &str) -> Result<String> {
    match wiki_block_state(content) {
        WikiBlockState::Both(start, end) => {
            let mut out = String::with_capacity(content.len() + block.len());
            out.push_str(&content[..start]);
            out.push_str(block);
            out.push_str(&content[end..]);
            Ok(out)
        }
        WikiBlockState::None => {
            // 追加：trim 掉尾部空白后接一个空行再放块，避免与用户内容粘连
            let trimmed = content.trim_end();
            let mut out = String::with_capacity(content.len() + block.len() + 2);
            out.push_str(trimmed);
            if !trimmed.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(block);
            Ok(out)
        }
        WikiBlockState::Half => {
            anyhow::bail!("检测到不完整的 wiki 标记对（只出现 {WIKI_BLOCK_START} 或 {WIKI_BLOCK_END} 之一，或顺序颠倒），拒绝修改，请人工检查文件")
        }
    }
}

/// 从文档文本移除 wiki 引用块（纯函数，不含 I/O）
///
/// 返回 `None` 表示无标记（未安装）；`Some` 为移除后的内容。
/// 半标记同样报错（与注入一致：不自动修复）。
pub fn remove_wiki_block(content: &str) -> Result<Option<String>> {
    match wiki_block_state(content) {
        WikiBlockState::Both(start, end) => {
            let mut out = String::with_capacity(content.len() - (end - start));
            out.push_str(&content[..start]);
            out.push_str(&content[end..]);
            Ok(Some(out))
        }
        WikiBlockState::None => Ok(None),
        WikiBlockState::Half => {
            anyhow::bail!("检测到不完整的 wiki 标记对（只出现 {WIKI_BLOCK_START} 或 {WIKI_BLOCK_END} 之一，或顺序颠倒），拒绝修改，请人工检查文件")
        }
    }
}

/// 向单个文件写入 wiki 引用块（读 → 注入 → 原子写）
fn write_wiki_block(path: &Path, block: &str) -> Result<()> {
    // 文件不存在视为空文档（正常创建路径），读取失败才显式报错
    let content = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("读取文件失败: {}", path.display()))?
    } else {
        String::new()
    };
    let new_content = inject_wiki_block(&content, block)?;
    crate::fs::write_file_atomic(path, &new_content)
}

/// 移除单个文件中的 wiki 引用块；返回是否实际移除
fn remove_wiki_block_from_file(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取文件失败: {}", path.display()))?;
    match remove_wiki_block(&content)? {
        Some(new_content) => {
            crate::fs::write_file_atomic(path, &new_content)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// install-wiki: 向项目根 AGENTS.md 注入 wiki 引用块（--also-claude 时同步写 CLAUDE.md）
///
/// 文件不存在则创建；已存在完整标记对则整块替换（只动标记之间内容）；
/// 半标记报错（不修，理由见 `wiki_block_state`）。
///
/// 注入块按目标仓库配置渲染（U02）：读 `root/config.toml` 取
/// output.dir 与 wiki.language；配置缺失（首次运行/未 install）时回退默认值
/// (".code-repo-wiki", "zh") 不报错——wiki 块缺失比注入失败更隐蔽。
pub fn install_wiki(root: &crate::project::ProjectRoot, also_claude: bool) -> Result<()> {
    // 目标仓库配置路径（v25：项目级 config.toml，与产物目录分离）；
    // 缺失时回退默认——v30 起 load_config 原样加载无净化（用户拍板），
    // 此处取的是 output_dir/language（项目契约）
    let config_path = root.join(Path::new(crate::config::PROJECT_CONFIG_FILE));
    let (output_dir, lang) = match crate::config::load_config(&config_path) {
        Ok(c) => (c.output_dir().to_string_lossy().into_owned(), c.wiki.language),
        Err(e) => {
            println!("提示: 未找到有效配置（{}），注入块按默认产物路径 (.code-repo-wiki / zh) 渲染", e);
            (".code-repo-wiki".to_string(), "zh".to_string())
        }
    };
    let block = wiki_block_template(&output_dir, &lang);
    let agents_path = root.join(Path::new("AGENTS.md"));
    write_wiki_block(&agents_path, &block)?;
    println!("✓ wiki 引用块已注入 {}", agents_path.display());
    if also_claude {
        let claude_path = root.join(Path::new("CLAUDE.md"));
        write_wiki_block(&claude_path, &block)?;
        println!("✓ wiki 引用块已注入 {}", claude_path.display());
    }
    Ok(())
}

/// uninstall-wiki: 移除 AGENTS.md 中的 wiki 引用块（含标记本身）
///
/// - AGENTS.md 无标记 → 提示"未安装"，退出码 0（幂等，与卸载语义一致）；
/// - 半标记 → 报错（不修）；
/// - CLAUDE.md 只在含标记对时清理（install --also-claude 的对称卸载），
///   从未注入过则静默跳过——CLAUDE.md 不被无标记情况下改动。
pub fn uninstall_wiki(root: &crate::project::ProjectRoot) -> Result<()> {
    let agents_path = root.join(Path::new("AGENTS.md"));
    if remove_wiki_block_from_file(&agents_path)? {
        println!("✓ wiki 引用块已从 {} 移除", agents_path.display());
    } else {
        println!("AGENTS.md 未安装 wiki 引用块，无需卸载");
    }
    let claude_path = root.join(Path::new("CLAUDE.md"));
    if remove_wiki_block_from_file(&claude_path)? {
        println!("✓ wiki 引用块已从 {} 移除", claude_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注入测试用的模板块（默认产物路径形态，模板函数化后的断言锚点）
    fn test_template() -> String {
        wiki_block_template(".code-repo-wiki", "zh")
    }

    /// append_note 追加式日志：同一日期节内序号递增；两次调用不覆盖历史
    #[test]
    fn test_append_note_increments_sequence() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_note_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        append_note(&dir, "zh", "第一条记录").unwrap();
        append_note(&dir, "zh", "第二条记录").unwrap();

        let log = std::fs::read_to_string(dir.join("wiki").join("zh").join("_log.md")).unwrap();
        assert!(log.contains("## "), "应含日期节");
        assert!(log.contains("- 1. 第一条记录"), "第一条应编号 1, 实际: {log}");
        assert!(log.contains("- 2. 第二条记录"), "第二条应编号 2, 实际: {log}");
        assert_eq!(
            log.matches("- ").count(),
            2,
            "应恰好 2 条记录, 实际: {log}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空内容拒绝写入（显式报错，不产生空记录）
    #[test]
    fn test_append_note_rejects_empty() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_note_empty_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(append_note(&dir, "zh", "   ").is_err(), "空内容应报错");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== wiki 引用块注入/移除 ====================

    /// 全新注入：空文档 → 追加完整标记对，结果恰等于模板本身
    #[test]
    fn test_inject_wiki_block_fresh() {
        let out = inject_wiki_block("", &test_template()).unwrap();
        assert_eq!(out, test_template(), "空文档注入结果应等于模板本身");
        assert!(out.contains(WIKI_BLOCK_START) && out.contains(WIKI_BLOCK_END));
    }

    /// 幂等替换：已含完整标记对 → 旧块整体替换为模板，用户前后内容保留
    #[test]
    fn test_inject_wiki_block_replaces_existing() {
        let before = "用户头部\n\n<!-- REPO-WIKI:START -->\n旧块内容\n<!-- REPO-WIKI:END -->\n\n用户尾部\n";
        let out = inject_wiki_block(before, &test_template()).unwrap();
        assert!(out.starts_with("用户头部\n\n"), "用户头部应保留, 实际: {out}");
        assert!(out.ends_with("用户尾部\n"), "用户尾部应保留, 实际: {out}");
        assert!(out.contains(&test_template()), "旧块应被替换为模板, 实际: {out}");
        assert!(!out.contains("旧块内容"), "旧块内容应被替换掉, 实际: {out}");
    }

    /// 幂等：同一文档注入两次 → 结果一致（第二次走替换路径）
    #[test]
    fn test_inject_wiki_block_twice_stable() {
        let first = inject_wiki_block("头部\n", &test_template()).unwrap();
        let second = inject_wiki_block(&first, &test_template()).unwrap();
        assert_eq!(first, second, "重复注入应幂等（内容不变）");
    }

    /// 半标记报错：只有 START / 只有 END / 顺序颠倒 → 均显式报错
    #[test]
    fn test_inject_wiki_block_half_marker_errors() {
        let cases = [
            "# 标题\n<!-- REPO-WIKI:START -->\n",
            "<!-- REPO-WIKI:END -->\n",
            "<!-- REPO-WIKI:END -->\n<!-- REPO-WIKI:START -->\n",
        ];
        for case in cases {
            let err = inject_wiki_block(case, &test_template()).unwrap_err();
            assert!(err.to_string().contains("不完整"), "半标记应报错: {err}");
        }
    }

    /// 保留用户内容：无标记追加场景下用户内容完整保留在块之前
    #[test]
    fn test_inject_wiki_block_preserves_user_content() {
        let before = "# 我的项目\n\n这是用户写的说明。\n";
        let out = inject_wiki_block(before, &test_template()).unwrap();
        let marker_idx = out.find(WIKI_BLOCK_START).unwrap();
        assert_eq!(
            &out[..marker_idx],
            "# 我的项目\n\n这是用户写的说明。\n\n",
            "块前应只有用户内容加一个空行"
        );
    }

    /// remove：无标记 → None（未安装）
    #[test]
    fn test_remove_wiki_block_not_installed() {
        assert!(remove_wiki_block("# 标题\n").unwrap().is_none());
    }

    /// remove：完整标记对 → 移除标记及内容，用户前后内容保留
    #[test]
    fn test_remove_wiki_block_removes_only_block() {
        let content =
            "用户头部\n\n<!-- REPO-WIKI:START -->\n块内容\n<!-- REPO-WIKI:END -->\n用户尾部\n";
        let out = remove_wiki_block(content).unwrap().unwrap();
        assert!(!out.contains(WIKI_BLOCK_START) && !out.contains(WIKI_BLOCK_END), "标记应被移除: {out}");
        assert!(out.contains("用户头部") && out.contains("用户尾部"), "用户内容应保留: {out}");
    }

    /// remove：半标记同样报错
    #[test]
    fn test_remove_wiki_block_half_marker_errors() {
        let err = remove_wiki_block("<!-- REPO-WIKI:START -->\n").unwrap_err();
        assert!(err.to_string().contains("不完整"), "半标记应报错: {err}");
    }
}
