use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::schema::WikiConfig;
use crate::incremental::state::GenerationState;
use crate::output::lint::{LintIssue, lint};

/// status 报告（结构化，main 只做格式化）
pub struct StatusReport {
    pub ready: bool,            // 产物根存在且非空
    pub wiki_pages: usize,      // wiki/{lang}/*.md 数量（所有语言合计）
    pub cards: usize,           // cards/{lang}/*.md 数量
    pub issues: Vec<LintIssue>, // lint 产物健康检查结果
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
    let issues = lint(output_dir, &source_roots(root));
    StatusReport {
        ready: wiki_pages > 0,
        wiki_pages,
        cards,
        issues,
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
        GenerationState::load(&state_dir).with_context(|| {
            format!(
                "状态文件损坏，拒绝静默重置（保护信息会丢失）: {}",
                state_path.display()
            )
        })?
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
    tracing::info!(
        "同步完成: 指纹更新 {} 个, 跳过受保护 {} 个",
        updated,
        skipped
    );
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
        .map(|after| after.lines().filter(|l| l.trim().starts_with("- ")).count())
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
    /// 额外注册 Claude Code MCP（--claude，用户级 `~/.claude.json` 顶层
    /// mcpServers，User scope——command 绑定本机 exe 路径=用户级内容，与
    /// opencode/codex 全局注册对称；v39 起不再写项目根 .mcp.json）；
    /// 同时同步注入 CLAUDE.md（Claude Code 不读 AGENTS.md，注册 MCP 时
    /// 必然需要文档指引——原 --also-claude 开关合并于此）
    pub claude: bool,
    /// 额外写 Codex CLI 用户级配置 `~/.codex/config.toml`（--codex）
    pub codex: bool,
    /// 额外写 DeepSeek Harness（dsh）patch 层 `{root}/cordis.patch.yml`
    /// （--dsh，W3）：dsh 不读 `.mcp.json`，MCP server 必须显式配置在
    /// patch 层——注册 `@deepseek-ai/dsh-mcp-client`（stdio，command 绑定
    /// 本机 exe）指向 repo-wiki 的 MCP server；AGENTS.md/CLAUDE.md 由 dsh
    /// 自动读取作为 instruction file，文档指引零成本获得
    pub dsh: bool,
}

/// code-repo-wiki 安装（v33 合并版）：OpenCode 插件 + 多 Agent MCP + AGENTS.md + git hooks
///
/// root 为项目根（U02：--root 注入，替代进程 cwd——插件/hook/config 全部
/// 相对项目根解析，跨 cwd 运行不再错位）。
///
/// 集成步骤（全部幂等，重复执行安全；非 code-repo-wiki 内容一律保留）：
/// 1. OpenCode 插件：用户级配置根 `~/.config/opencode/plugins/code-repo-wiki.ts`
///    （v39：插件是用户级内容——装进 Agent 配置根目录，官方自动加载目录，
///    一次 install 全仓库 opencode 会话可用；不再写入项目 `.opencode/plugins/`，
///    旧版项目级产物自动迁移清理）；模板注入 current_exe 绝对路径
///    （t02 摆脱 PATH 依赖）；内容与模板不同即升级
/// 2. OpenCode MCP：用户级全局 `opencode.json` 的 `mcp.code-repo-wiki` 条目
///    （v33 拍板：一次注册所有仓库可用，server 以工作区为 cwd）
/// 3. Claude MCP（--claude）：用户级 `~/.claude.json` 顶层 mcpServers 条目
///    （Claude Code 官方 User scope，command 绑定本机 exe=用户级内容；
///    v39 起不再写项目根 .mcp.json——机器相关配置不入团队共享文件）
/// 4. Codex MCP（--codex）：用户级 `~/.codex/config.toml` 的
///    `[mcp_servers.code-repo-wiki]` 表（Codex 官方用户级配置）
/// 5. dsh MCP（--dsh，W3）：项目根 `{root}/cordis.patch.yml` 追加/更新
///    `- insert:` 块注册 `@deepseek-ai/dsh-mcp-client`（stdio，command 绑定
///    本机 exe）——dsh 不读 `.mcp.json`，MCP 必须显式配置在 patch 层；
///    与 --codex 等互不排斥，install/uninstall 对称
/// 6. AGENTS.md：wiki 引用块（标记对幂等替换；默认执行）
/// 7. CLAUDE.md（随 --claude，v36 起）：Claude Code 不读 AGENTS.md，
///    注册 Claude MCP 时同步注入引用块
/// 8. git hooks：post-commit/post-merge（含 code-repo-wiki 标记则升级覆盖；
///    用户自定义 hook 保留并提示）
///
/// 用户级默认配置的确保由调用方（main.rs）先行执行（v25 语义）。
pub fn install(root: &crate::project::ProjectRoot, opts: &InstallOptions) -> Result<()> {
    let project_root = root.path();

    // MCP 与插件共用当前可执行文件绝对路径（t02：不依赖 PATH）
    let exe_path =
        std::env::current_exe().context("无法定位当前可执行文件路径（集成无法绑定绝对路径）")?;
    let exe_str = exe_path.to_string_lossy().into_owned();
    let mcp_args = ["mcp".to_string()];

    // 1. OpenCode 插件（用户级配置根——v39：Agent 配置根目录安装）
    let mut oc =
        crate::config::opencode::OpenCodeConfig::new(root).context("读取 OpenCode 配置失败")?;
    oc.install_plugin()?;
    if oc.install_plugin_file()? {
        println!("✓ OpenCode 插件已安装（用户级: ~/.config/opencode/plugins/）");
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

    // 3. Claude MCP（--claude → 用户级 ~/.claude.json User scope，v39）
    if opts.claude {
        let claude = crate::config::mcp::ClaudeMcp {
            path: crate::config::mcp::ClaudeMcp::user_global_path()?,
        };
        if claude.install("code-repo-wiki", &exe_str, &mcp_args)? {
            println!("✓ Claude Code MCP 已注册（用户级: ~/.claude.json）");
        } else {
            println!("✓ Claude Code MCP 已是最新（~/.claude.json）");
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

    // 5. dsh MCP（--dsh，W3 → 项目根 cordis.patch.yml patch 层）
    if opts.dsh {
        let dsh = crate::config::mcp::DshMcp {
            path: project_root.join(Path::new("cordis.patch.yml")),
        };
        if dsh.install(&exe_str)? {
            println!("✓ DeepSeek Harness MCP 已注册（cordis.patch.yml）");
        } else {
            println!("✓ DeepSeek Harness MCP 已是最新（cordis.patch.yml）");
        }
    }

    // 6/7. AGENTS.md（默认）与 CLAUDE.md（v36 起随 --claude 同步——
    // Claude Code 不读 AGENTS.md，注册 MCP 时文档指引随之注入）
    install_wiki(root, opts.claude)?;

    // 8. git hooks（v41：新装独立脚本；既有用户 hook 尾部追加块共存）
    let hooks_present = install_hooks(project_root)?;

    println!("✓ code-repo-wiki 安装完成");
    println!();
    println!("日常使用（傻瓜式全自动，无需记忆命令）：");
    if hooks_present {
        println!("  1. git commit 后 wiki 自动增量更新（post-commit/post-merge hook 已配置）");
    } else {
        println!("  1. git commit 后 wiki 自动增量更新——hook 未安装（未检测到 .git 目录），");
        println!("     使用命令 2/3 手动/常驻更新");
    }
    println!("  2. 手动一条命令：code-repo-wiki update（首次自动全量生成，之后自动增量；");
    println!("     无变更秒回，失败模块自动补偿重试，尾部自动 lint 复核）");
    println!("  3. 常驻实时模式：code-repo-wiki watch（代码保存即自动更新，Ctrl-C 退出）");
    println!("  4. 健康检查：code-repo-wiki doctor / code-repo-wiki lint");
    Ok(())
}

/// git hook 追加块开始标记（v41 方案：与用户既有 hook 共存——尾部追加
/// 标记块，升级判定与卸载剥离判定共用「是否 code-repo-wiki 所有」判据；
/// 用户自定义 hook 不含此标记，不会被覆盖，只追加块）
pub const HOOK_MARKER: &str = "# code-repo-wiki: append-begin";

/// git hook 追加块结束标记（与 [`HOOK_MARKER`] 成对，剥离时按区间删除）
pub const HOOK_END_MARKER: &str = "# code-repo-wiki: append-end";

/// v37 改名前的 hook 标记（v33 旧模板——独立脚本式）。升级/卸载时一并
/// 识别——改名后旧 hook 里的 `repo-wiki update` 命令已不存在，残留只会
/// 静默失效。
pub const LEGACY_HOOK_MARKER: &str = "# repo-wiki managed";

/// hook 内容是否属于本工具（追加块标记、v37 旧标记、或 v33 前旧模板都
/// 算——旧 hook 是本工具旧版本的产物，升级剥离与卸载删除都应覆盖它）。
/// v33 前旧模板无 managed 标记，特征行是注释里的
/// `auto-update wiki on commit`（用户/第三方 hook 不会写这行）。
fn hook_is_ours(content: &str) -> bool {
    content.contains(HOOK_MARKER)
        || content.contains(LEGACY_HOOK_MARKER)
        || content.contains("auto-update wiki on commit")
}

/// 生成独立 hook 脚本内容（新装/整文件升级场景）
///
/// `#!/bin/sh` + LF：Windows 上由 Git for Windows 的 sh 执行（POSIX 语义，
/// 绝非 PowerShell）；`cd` 到仓库顶层保证 --root 无关；`command -v` 探测
/// 二进制存在性；并发由 update --skip-if-locked 处理（v15.3：fd-lock 下
/// 锁文件常驻，kill -0 活性判定失真，改由命令内原子拿锁自行跳过，消除
/// hook 内 check-then-act TOCTOU）；update 失败不阻断 git 主流程（hook 是
/// 通知型），但失败必须可见：stderr 落 .code-repo-wiki/update-error.log 并在
/// 提交输出中提示一行（v36 D2：此前 2>/dev/null || true 把失败完全吞掉，
/// 用户永远不知道 wiki 已陈旧）。
fn hook_content() -> String {
    format!(
        "#!/bin/sh\n{0}: auto-update wiki on commit\ncd \"$(git rev-parse --show-toplevel)\"\ncommand -v code-repo-wiki >/dev/null 2>&1 || exit 0\nmkdir -p .code-repo-wiki\n# v15.3 并发处理：--skip-if-locked 在另一实例（watch/手动）持有运行锁时跳过本次更新\n# （fd-lock 下锁文件常驻，旧 PID 活性判定失真，v13.3 锁感知块已废弃）\n# v13.3 日志有界：update-error.log 超 1MB 时轮转保留尾部 100 行\nlog=.code-repo-wiki/update-error.log\nif [ -f \"$log\" ] && [ \"$(wc -c < \"$log\" 2>/dev/null)\" -gt 1048576 ]; then\n  tail -n 100 \"$log\" > \"$log.tmp\" 2>/dev/null && mv \"$log.tmp\" \"$log\" 2>/dev/null || true\nfi\ncode-repo-wiki update --skip-if-locked 2>>.code-repo-wiki/update-error.log || echo \"code-repo-wiki: wiki 更新失败（详见 .code-repo-wiki/update-error.log）\" >&2\n",
        HOOK_MARKER
    )
}

/// 生成追加块内容（v41：无 shebang——追加在用户 hook 尾部；`cd` 到仓库
/// 顶层保证位置；每条命令带 `|| exit 0`/`|| echo` 满足 `set -e` 隔离——
/// 单条失败不传播为 git hook 失败，也不被静默吞掉）
fn hook_block() -> String {
    format!(
        "{0}\n# 自动更新 wiki（追加块，与仓库既有 hook 共存；用户 hook 若以 exit 结束，\n# 本块不会执行——post-commit 场景罕见，若需保证请移除既有 hook 后重装）\ncd \"$(git rev-parse --show-toplevel)\" 2>/dev/null || exit 0\ncommand -v code-repo-wiki >/dev/null 2>&1 || exit 0\nmkdir -p .code-repo-wiki 2>/dev/null || exit 0\n# v15.3 并发处理：--skip-if-locked 在另一实例持有运行锁时跳过（PID 活性判定失真，旧锁感知块废弃）\n# v13.3 日志有界：超 1MB 轮转保留尾部 100 行\nlog=.code-repo-wiki/update-error.log\nif [ -f \"$log\" ] && [ \"$(wc -c < \"$log\" 2>/dev/null)\" -gt 1048576 ]; then\n  tail -n 100 \"$log\" > \"$log.tmp\" 2>/dev/null && mv \"$log.tmp\" \"$log\" 2>/dev/null || true\nfi\ncode-repo-wiki update --skip-if-locked 2>>.code-repo-wiki/update-error.log || echo \"code-repo-wiki: wiki 更新失败（详见 .code-repo-wiki/update-error.log）\" >&2\n{1}\n",
        HOOK_MARKER, HOOK_END_MARKER
    )
}

/// 剥离追加块区间（begin 标记行到 end 标记行含两端），返回剩余内容。
/// 无区间时原样返回（幂等）。剥离后 trim——去掉区间剥除留下的多余空行。
fn strip_hook_block(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let begin = lines.iter().position(|l| l.trim() == HOOK_MARKER);
    let end = lines.iter().position(|l| l.trim() == HOOK_END_MARKER);
    match (begin, end) {
        (Some(b), Some(e)) if b <= e => {
            let mut kept: Vec<&str> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if i < b || i > e {
                    kept.push(line);
                }
            }
            kept.join("\n").trim().to_string()
        }
        // 区间不完整（只有一端）→ 保守不动（不破坏用户 hook）
        _ => content.trim().to_string(),
    }
}

/// 内容中是否存在完整的追加块区间（begin/end 标记各恰一行且 begin 在 end
/// 前）。存在才算「追加块场景」——install 只替换区间、uninstall 只剥离区间，
/// 保留区间外用户内容。
///
/// 缺失区间时：v41 独立脚本的标记只作特征行前缀（`HOOK_MARKER: auto-update...`，
/// 非独立行）或 v33/v37 旧模板无 begin/end 行，属「整文件场景」——必须整文件
/// 覆盖升级/删除。若误走区间替换，strip 找不到区间会原样返回、随后向尾部
/// 再追一块：旧块（含已废弃的 kill -0 锁感知）残留且块重复（v15.3 前实测）。
fn has_hook_block_span(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let begin = lines.iter().position(|l| l.trim() == HOOK_MARKER);
    let end = lines.iter().position(|l| l.trim() == HOOK_END_MARKER);
    matches!((begin, end), (Some(b), Some(e)) if b <= e)
}

/// 在既有内容尾部追加块（块不存在时；已存在则整文件升级走
/// [`replace_hook_block`]）
fn append_hook_block(existing: &str, block: &str) -> String {
    format!("{}\n\n{}", existing.trim_end(), block)
}

/// 替换既有内容中的追加块区间（begin-end 整体替换为新块——升级场景，
/// 保留区间外的用户内容）
fn replace_hook_block(existing: &str, block: &str) -> String {
    let stripped = strip_hook_block(existing);
    format!("{}\n\n{}", stripped, block)
}

/// 原子写 hook 并设置执行位（unix；Windows 由 sh 解释执行无需执行位）
fn write_hook(path: &std::path::Path, content: &str) -> Result<()> {
    crate::fs::write_file_atomic(path, content)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    Ok(())
}

/// 检测 core.hooksPath 配置（v41）：Git 从该目录加载 hook——若指向
/// .git/hooks 之外，写入 .git/hooks 的 hook 不会生效，安装前必须提示。
/// 返回配置值（未配置返回 None）。
fn detect_core_hooks_path(project_root: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "core.hooksPath"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// 安装 git hooks（post-commit/post-merge）；返回（新装/升级数量,
/// 任一 hook 文件最终是否存在）——第二值供总结语区分「已配置」与
/// 「未检测到 .git 目录」（v41：全部已最新时 installed=0 但 hook 可用，
/// 不再误报「未安装」）
///
/// v41 追加块语义（与用户既有 hook 共存，业界标记块惯例——husky 等
/// 工具同款思路）：
/// - 不存在 → 新建独立脚本
/// - 已存在且含追加块标记 → 只替换块区间（保留区间外用户内容）
/// - 已存在且含旧标记（v33/v37 独立脚本）→ 内容不同则整文件覆盖升级
/// - 已存在且无标记（用户/第三方自定义 hook）→ 尾部追加块（不覆盖）
/// - `.git/hooks` 不存在（非 git 仓库）→ 提示跳过（install 不因此失败）
/// - core.hooksPath 指向 .git/hooks 之外 → 提示（不失败）
fn install_hooks(project_root: &std::path::Path) -> Result<bool> {
    let hooks_dir = project_root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        println!("未检测到 .git 目录，跳过 git hook 安装");
        return Ok(false);
    }
    if let Some(path) = detect_core_hooks_path(project_root) {
        // hooksPath 指向 .git/hooks 本身 = 等同默认，无影响；指向其他
        // 目录才提示（写入 .git/hooks 不会生效）
        let hooks_dir_abs = hooks_dir
            .canonicalize()
            .unwrap_or_else(|_| hooks_dir.clone());
        let hooks_path_abs = std::path::Path::new(&path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(&path));
        if hooks_path_abs != hooks_dir_abs {
            println!(
                "? 检测到 git core.hooksPath = {path}——hook 将从该目录加载，写入 .git/hooks 不会生效（可移除该配置或将其指向 .git/hooks）"
            );
        }
    }
    let content = hook_content();
    let block = hook_block();
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let existing = std::fs::read_to_string(&hook_path)?;
            if hook_is_ours(&existing) {
                if existing.contains(HOOK_MARKER) && has_hook_block_span(&existing) {
                    // 追加块场景：只替换块区间，保留用户内容
                    let new_content = replace_hook_block(&existing, &block);
                    if new_content != existing {
                        write_hook(&hook_path, &new_content)?;
                        println!("✓ git {hook_name} hook 已升级（追加块已更新，用户内容保留）");
                    } else {
                        println!("✓ git {hook_name} hook 已是最新");
                    }
                } else if existing != content {
                    // 独立脚本（v41 标记前缀 / v33/v37 旧模板，无 begin/end 区间）：
                    // 整文件覆盖升级——旧 kill -0 锁感知块整体移除（v15.3）
                    write_hook(&hook_path, &content)?;
                    println!("✓ git {hook_name} hook 已升级");
                } else {
                    println!("✓ git {hook_name} hook 已是最新");
                }
            } else {
                // 用户/第三方 hook：尾部追加块，保留原内容
                write_hook(&hook_path, &append_hook_block(&existing, &block))?;
                println!("✓ git {hook_name} hook 已追加 code-repo-wiki 块（原内容保留）");
            }
        } else {
            write_hook(&hook_path, &content)?;
            println!("✓ git {hook_name} hook 已安装");
        }
    }
    Ok(true)
}

/// 移除 git hooks（仅 code-repo-wiki 的；用户自定义 hook 保留）
///
/// v41：追加块场景剥离块区间（用户内容保留写回）；独立脚本场景删文件。
/// 判定与 install_hooks 同源（当前标记或 v37 旧标记——旧模板无 managed
/// 标记但含 repo-wiki 调用，同样识别，避免改名后旧 hook 残留静默失效）。
fn remove_hooks(project_root: &std::path::Path) -> Result<()> {
    let hooks_dir = project_root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        return Ok(());
    }
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
            if hook_is_ours(&content) {
                if content.contains(HOOK_MARKER) && has_hook_block_span(&content) {
                    // 追加块：剥离区间，用户内容保留
                    let remaining = strip_hook_block(&content);
                    if remaining.is_empty() {
                        std::fs::remove_file(&hook_path)?;
                    } else {
                        write_hook(&hook_path, &remaining)?;
                    }
                    println!("✓ git {hook_name} hook 已移除 code-repo-wiki 块（原内容保留）");
                } else {
                    // 独立脚本（v41 标记前缀 / v33/v37 旧模板）：整文件删除
                    std::fs::remove_file(&hook_path)?;
                    println!("✓ git {hook_name} hook 已移除");
                }
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
/// 3. Claude MCP `~/.claude.json` 顶层 mcpServers 条目（其他键/其他 server
///    保留；空 mcpServers 保留文件——OAuth 会话等用户配置绝不动）
/// 4. Codex MCP 表（其他表/注释保留）
/// 5. dsh MCP 项目根 cordis.patch.yml 注册块（--dsh 的对称卸载，W3；
///    其他 patch 操作/注释保留）
/// 6. AGENTS.md / CLAUDE.md wiki 块（无标记则跳过）
/// 7. git hooks（仅 code-repo-wiki 标记的删除；用户自定义 hook 保留）
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

    // 2. OpenCode 插件（用户级配置根 + 旧版项目级产物迁移清理）
    let mut oc =
        crate::config::opencode::OpenCodeConfig::new(root).context("读取 OpenCode 配置失败")?;
    oc.uninstall_plugin()?;
    oc.uninstall_plugin_file()?;
    println!("✓ OpenCode 插件已移除（用户级全局——所有仓库的 opencode 会话不再自动加载）");

    // 3. Claude MCP（用户级 ~/.claude.json User scope，v39）
    let claude = crate::config::mcp::ClaudeMcp {
        path: crate::config::mcp::ClaudeMcp::user_global_path()?,
    };
    if claude.remove("code-repo-wiki")? {
        println!(
            "✓ Claude Code MCP 条目已移除（~/.claude.json——其他仓库如需继续使用请重新 install）"
        );
    } else {
        println!("✓ Claude Code MCP 条目不存在，跳过（~/.claude.json）");
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

    // 5. dsh MCP（项目根 cordis.patch.yml 注册块，W3——--dsh 的对称卸载）
    let dsh = crate::config::mcp::DshMcp {
        path: project_root.join(Path::new("cordis.patch.yml")),
    };
    if dsh.remove()? {
        println!("✓ DeepSeek Harness MCP 条目已移除（cordis.patch.yml）");
    } else {
        println!("✓ DeepSeek Harness MCP 条目不存在，跳过（cordis.patch.yml）");
    }

    // 6. AGENTS.md / CLAUDE.md wiki 块
    uninstall_wiki(root)?;

    // 7. git hooks
    remove_hooks(project_root)?;

    println!("✓ code-repo-wiki 卸载完成 (数据保留: .code-repo-wiki/ 与用户级配置)");
    Ok(())
}

/// wiki 引用块的起始标记（注入块的唯一边界；v37 改名后为 CODE-REPO-WIKI）
pub const WIKI_BLOCK_START: &str = "<!-- CODE-REPO-WIKI:START -->";

/// wiki 引用块的结束标记
pub const WIKI_BLOCK_END: &str = "<!-- CODE-REPO-WIKI:END -->";

/// v37 改名前的旧标记（REPO-WIKI）。升级兼容：inject 把旧块整体替换为新
/// 模板（迁移），remove 把新旧标记对一并删除——已注入旧块的仓库升级
/// 后不会留下双份块。
pub const LEGACY_WIKI_BLOCK_START: &str = "<!-- REPO-WIKI:START -->";

/// v37 改名前的旧结束标记
pub const LEGACY_WIKI_BLOCK_END: &str = "<!-- REPO-WIKI:END -->";

/// 渲染注入块模板（install 的 AGENTS.md 注入与 CLAUDE.md 注入共用，单一模板双写）
///
/// 内容按 agents.md 标准五要素组织：命令优先（核心命令速查）/ MCP 工具清单
/// （16.1 改名后的 wiki_ 前缀工具与使用时机）/ 完成定义（update no-op、lint
/// 通过等判据）/ 产物新鲜度核对（过期判据与 update 刷新路径，W3 补强——
/// 对齐 generate_agents_md 的「先核对再消费」指引）/ 人工修改保护（指纹保护
/// 与 generate --force 强制覆盖）/ 预构建架构知识 · 少调工具（先读
/// architecture-map.md / llms-full.txt / 卡片 dependents 再决定是否调 MCP，
/// W3 补强）/ 何时不做（不手改确定性重生成产物）/ 渐进式披露（llms.txt 站点
/// 地图 → overview/architecture → 模块页 → api.md，按上下文预算分层）。
///
/// 只引**稳定产物路径**与模块依赖概念（llms.txt、architecture-map.md、卡片
/// dependents 字段等产物层名词），不写易变的源码具体路径。不复制 wiki 正文
/// （避免与 LLM 生成的产物内容双份漂移）。以换行结尾，保证追加/替换后与
/// 相邻内容衔接干净。
///
/// 产物路径按实际配置渲染（U02）：`output_dir` 与 `lang` 来自目标仓库的
/// config.toml（output.dir / wiki.language）——此前模板硬编码 `wiki/` 与
/// `zh`，默认配置（output.dir=.code-repo-wiki、language 可改）下注入指引失配。
pub fn wiki_block_template(output_dir: &str, lang: &str) -> String {
    format!(
        "\
<!-- CODE-REPO-WIKI:START -->
本仓库使用 code-repo-wiki 维护可持续进化的项目 Wiki，产物位于 `{output_dir}/`。

## 核心命令速查

| 命令 | 用途 |
|---|---|
| `code-repo-wiki generate` | 全量生成（首次/配置变更后；分阶段进度 + 完成摘要） |
| `code-repo-wiki update` | 增量更新（改完代码后运行；无变更秒回 no-op） |
| `code-repo-wiki search -q \"<关键词>\"` | 语义搜索实体（text/semantic/hybrid，hybrid 含调用链补全） |
| `code-repo-wiki ast-search <符号>` | 精确符号查找（文件 + 行号 + 签名） |
| `code-repo-wiki lint` | 产物健康检查（孤儿页/断链/过时/引用错位） |
| `code-repo-wiki status` | Wiki 状态报告（是否就绪/语义降级/lint 问题） |
| `code-repo-wiki watch` | 常驻监听，保存即自动更新 |
| `code-repo-wiki note \"<记录>\"` | 追加知识记录到 `{output_dir}/wiki/{lang}/_log.md` |
| `code-repo-wiki install` | 注册 git hooks + 插件 + MCP（本块由它维护） |

## MCP 工具（已注册的 Agent 会话可直接调用）

| 工具 | 使用时机 |
|---|---|
| `wiki_search` | 按关键词检索代码实体（定位函数/结构体/类定义或引用，hybrid 含调用链补全） |
| `wiki_ast_search` | 精确符号定义查找（全量 AST 扫描，成本随仓库规模增长，仅需精确定位时用） |
| `wiki_status` | 先确认 Wiki 是否已生成/健康（语义索引降级、lint 问题） |
| `wiki_read_page` | 读取模块页/架构/概览/API 页面正文 |
| `wiki_read_card` | 读取知识卡片（模块结构化摘要） |

## 完成定义

- `update` 输出 no-op（无文件变更，跳过更新）即无增量待生成；
- `lint` 无孤儿页/断链/过时/引用错位问题即产物健康；
- `generate` 输出完成摘要（扫描 N 文件 / M 实体 / K 页文档）即生成成功。

## 产物新鲜度核对

- `{output_dir}/llms.txt` 头部含生成工具版本行，产物 mtime 距今超过 7 天视为过期
  （过期产物会降低 Agent 检索质量）；
- 不确定 `{output_dir}/wiki/{lang}/architecture.md` 与 `{output_dir}/wiki/{lang}/overview.md`
  是否与当前代码一致时，以 `code-repo-wiki status` / `code-repo-wiki lint` 输出为准；
- 过期或拿不准：先 `code-repo-wiki update` 增量刷新（无变更秒回 no-op）再消费产物。

## 人工修改保护

- 产物页面/知识卡片被人工编辑后不会被自动覆盖（指纹保护；`update` 跳过该页，
  修改记入卡片 `pending_manual_edits` 节）；
- 需要强制覆盖时用 `code-repo-wiki generate --force` 清空保护集。

## 预构建架构知识 · 少调工具

回答「X 如何实现」「谁依赖 X」类问题，先读产物再决定是否调 MCP 工具：

1. 先读 `{output_dir}/wiki/{lang}/architecture-map.md` 架构地图——模块职责与
   依赖/调用关系大多可直接回答实现方式与反向依赖；
2. `{output_dir}/llms-full.txt` 一次可回答实体级问题（模块职责 + 实体清单内联）；
3. 知识卡片 `dependents` 字段（`{output_dir}/cards/{lang}/<模块>.md`）直接给出
   反向依赖；能少调 MCP 工具就少调（工具调用有上下文与延迟成本）。
4. 项目级知识卡（写代码/审查前先读，对齐团队规范与技术环境）：
   `{output_dir}/cards/{lang}/project/spec.md`（代码规约：命名/接口/约束，
   每条带来源文件）与 `{output_dir}/cards/{lang}/project/tech-stack.md`
   （技术栈清单：依赖/版本/来源清单，确定性解析零 LLM 防幻觉）。

## 何时不做

- 不直接编辑 `{output_dir}/llms.txt` / `{output_dir}/llms-full.txt`（确定性重生成会覆盖）；
- 不在产物目录手工放置页面（`code-repo-wiki lint` 会判为孤儿页）；
- 未改代码时不跑 `update`（no-op 无收益）。

## 渐进式披露（按上下文预算分层）

1. 预算紧张：只读 `{output_dir}/llms.txt` 站点地图定位目标页面；
2. 预算充足：读 `{output_dir}/wiki/{lang}/overview.md` 与 `{output_dir}/wiki/{lang}/architecture.md`
   建立全局认知，再按需深入模块页（可用 `{output_dir}/llms-full.txt` 一次获得实体骨架）；
3. 查 API 签名与文件行号：`{output_dir}/wiki/{lang}/api.md`。
<!-- CODE-REPO-WIKI:END -->
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

/// 定位文档中的 wiki 标记对（当前标记与 v37 旧标记都识别）
///
/// 标记对候选按「新标记优先」顺序探测：
/// - 新标记（CODE-REPO-WIKI）完整对 → 用它（升级替换的落点）
/// - 新标记无、旧标记（REPO-WIKI）完整对 → 用它（旧块迁移落点）
/// - 任一套只出现 START/END 之一，或顺序颠倒 → Half（不自动修复）
/// - 两套都无 → None（干净状态，可安全追加）
///
/// - Both: start 对齐到 START 行首、end 对齐到 END 行尾（含换行），
///   使"整块替换/删除"只触碰标记及其之间内容，不伤用户文本。
/// - Half: 半标记说明文件被人为改坏或与其他工具冲突，修补方向有歧义
///   （删哪半？补哪半？），显式报错让用户处理。
fn wiki_block_state(content: &str) -> WikiBlockState {
    // 探测顺序：新标记对优先，其次旧标记对；两套都存在时只处理新对
    // （旧对残留由 remove 循环删除清理，inject 不重复迁移）
    for (start_marker, end_marker) in [
        (WIKI_BLOCK_START, WIKI_BLOCK_END),
        (LEGACY_WIKI_BLOCK_START, LEGACY_WIKI_BLOCK_END),
    ] {
        let start = content.find(start_marker);
        let end = content.find(end_marker);
        match (start, end) {
            (Some(s), Some(e)) if s < e => {
                let line_start = content[..s].rfind('\n').map_or(0, |i| i + 1);
                let line_end = content[e..].find('\n').map_or(content.len(), |i| e + i + 1);
                return WikiBlockState::Both(line_start, line_end);
            }
            (None, None) => continue,
            _ => return WikiBlockState::Half,
        }
    }
    WikiBlockState::None
}

/// 将 wiki 引用块注入文档文本（纯函数，不含 I/O，install 的 AGENTS.md/CLAUDE.md 注入共用）
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
            anyhow::bail!(
                "检测到不完整的 wiki 标记对（只出现 {WIKI_BLOCK_START} 或 {WIKI_BLOCK_END} 之一，或顺序颠倒），拒绝修改，请人工检查文件"
            )
        }
    }
}

/// 从文档文本移除 wiki 引用块（纯函数，不含 I/O）
///
/// 返回 `None` 表示无标记（未安装）；`Some` 为移除后的内容。
/// 半标记同样报错（与注入一致：不自动修复）。
///
/// 循环删除：新标记对删完后可能还残留旧标记对（v37 改名前的块），
/// 一并清掉——两套对都处理完才返回，保证卸载彻底。
pub fn remove_wiki_block(content: &str) -> Result<Option<String>> {
    let mut out = content.to_string();
    let mut removed = false;
    // 最多两轮：新对 + 旧对各一轮；每轮删除后重扫（对位置随删除前移）
    for _ in 0..2 {
        match wiki_block_state(&out) {
            WikiBlockState::Both(start, end) => {
                let mut next = String::with_capacity(out.len() - (end - start));
                next.push_str(&out[..start]);
                next.push_str(&out[end..]);
                out = next;
                removed = true;
            }
            WikiBlockState::None => break,
            WikiBlockState::Half => {
                anyhow::bail!(
                    "检测到不完整的 wiki 标记对（只出现 {WIKI_BLOCK_START} 或 {WIKI_BLOCK_END} 之一，或顺序颠倒），拒绝修改，请人工检查文件"
                )
            }
        }
    }
    Ok(if removed { Some(out) } else { None })
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

/// install: 向项目根 AGENTS.md 注入 wiki 引用块（--claude 时同步写 CLAUDE.md）
///
/// 文件不存在则创建；已存在完整标记对则整块替换（只动标记之间内容）；
/// 半标记报错（不修，理由见 `wiki_block_state`）。
///
/// 注入块按目标仓库配置渲染（U02）：读 `root/config.toml` 取
/// output.dir 与 wiki.language；配置缺失（首次运行/未 install）时回退默认值
/// (".code-repo-wiki", "zh") 不报错——wiki 块缺失比注入失败更隐蔽。
pub fn install_wiki(root: &crate::project::ProjectRoot, also_claude: bool) -> Result<()> {
    // 渲染配置走完整配置链（v41 修复：与运行命令同源——项目级 config.toml
    // 字段级合并覆盖用户级 ~/.code-repo-wiki/config.toml；两者皆无时自动
    // 创建用户级默认模板）。此前只读项目级单文件，用户级配置存在时仍误报
    // 「未找到有效配置」，且渲染值忽略用户级偏好（如 wiki.language）。
    let (_, cfg) = match crate::config::load_default_config(root) {
        Ok(pair) => pair,
        // 配置畸形（如 TOML 语法错误）：按默认值继续注入——wiki 块缺失
        // 比注入失败更隐蔽（U02 语义保持）
        Err(e) => {
            println!("提示: 配置解析失败（{e}），注入块按默认产物路径 (.code-repo-wiki / zh) 渲染");
            let cfg = crate::config::load_config(
                &root.join(Path::new(crate::config::PROJECT_CONFIG_FILE)),
            )
            .unwrap_or_else(|_| crate::config::schema::WikiConfig::default());
            let output_dir = cfg.output_dir().to_string_lossy().into_owned();
            let lang = cfg.wiki.language;
            let block = wiki_block_template(&output_dir, &lang);
            let agents_path = root.join(Path::new("AGENTS.md"));
            write_wiki_block(&agents_path, &block)?;
            println!("✓ wiki 引用块已注入 {}", agents_path.display());
            if also_claude {
                let claude_path = root.join(Path::new("CLAUDE.md"));
                write_wiki_block(&claude_path, &block)?;
                println!("✓ wiki 引用块已注入 {}", claude_path.display());
            }
            return Ok(());
        }
    };
    let output_dir = cfg.output_dir().to_string_lossy().into_owned();
    let lang = cfg.wiki.language;
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

/// uninstall: 移除 AGENTS.md 中的 wiki 引用块（含标记本身）
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_note_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        append_note(&dir, "zh", "第一条记录").unwrap();
        append_note(&dir, "zh", "第二条记录").unwrap();

        let log = std::fs::read_to_string(dir.join("wiki").join("zh").join("_log.md")).unwrap();
        assert!(log.contains("## "), "应含日期节");
        assert!(
            log.contains("- 1. 第一条记录"),
            "第一条应编号 1, 实际: {log}"
        );
        assert!(
            log.contains("- 2. 第二条记录"),
            "第二条应编号 2, 实际: {log}"
        );
        assert_eq!(log.matches("- ").count(), 2, "应恰好 2 条记录, 实际: {log}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空内容拒绝写入（显式报错，不产生空记录）
    #[test]
    fn test_append_note_rejects_empty() {
        let dir =
            std::env::temp_dir().join(format!("code_repo_wiki_note_empty_{}", std::process::id()));
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
        let before = "用户头部\n\n<!-- CODE-REPO-WIKI:START -->\n旧块内容\n<!-- CODE-REPO-WIKI:END -->\n\n用户尾部\n";
        let out = inject_wiki_block(before, &test_template()).unwrap();
        assert!(
            out.starts_with("用户头部\n\n"),
            "用户头部应保留, 实际: {out}"
        );
        assert!(out.ends_with("用户尾部\n"), "用户尾部应保留, 实际: {out}");
        assert!(
            out.contains(&test_template()),
            "旧块应被替换为模板, 实际: {out}"
        );
        assert!(!out.contains("旧块内容"), "旧块内容应被替换掉, 实际: {out}");
    }

    /// v37 旧标记迁移：含旧标记（REPO-WIKI）完整块 → 同样整体替换为新模板
    /// （改名前的仓库升级路径，块标记随之换代）
    #[test]
    fn test_inject_wiki_block_migrates_legacy_marker() {
        let before =
            "用户头部\n\n<!-- REPO-WIKI:START -->\n旧名块内容\n<!-- REPO-WIKI:END -->\n用户尾部\n";
        let out = inject_wiki_block(before, &test_template()).unwrap();
        assert!(out.contains(WIKI_BLOCK_START) && out.contains(WIKI_BLOCK_END));
        assert!(
            !out.contains("<!-- REPO-WIKI:START -->") && !out.contains("<!-- REPO-WIKI:END -->"),
            "旧标记应随迁移消失, 实际: {out}"
        );
        assert!(
            !out.contains("旧名块内容"),
            "旧块内容应被替换掉, 实际: {out}"
        );
        assert!(
            out.contains("用户头部") && out.contains("用户尾部"),
            "用户内容应保留: {out}"
        );
    }

    /// 幂等：同一文档注入两次 → 结果一致（第二次走替换路径）
    #[test]
    fn test_inject_wiki_block_twice_stable() {
        let first = inject_wiki_block("头部\n", &test_template()).unwrap();
        let second = inject_wiki_block(&first, &test_template()).unwrap();
        assert_eq!(first, second, "重复注入应幂等（内容不变）");
    }

    /// 半标记报错：只有 START / 只有 END / 顺序颠倒 → 均显式报错
    /// （当前标记与 v37 旧标记两套都要检）
    #[test]
    fn test_inject_wiki_block_half_marker_errors() {
        let cases = [
            "# 标题\n<!-- CODE-REPO-WIKI:START -->\n",
            "<!-- CODE-REPO-WIKI:END -->\n",
            "<!-- CODE-REPO-WIKI:END -->\n<!-- CODE-REPO-WIKI:START -->\n",
            "# 标题\n<!-- REPO-WIKI:START -->\n",
            "<!-- REPO-WIKI:END -->\n",
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
        let content = "用户头部\n\n<!-- CODE-REPO-WIKI:START -->\n块内容\n<!-- CODE-REPO-WIKI:END -->\n用户尾部\n";
        let out = remove_wiki_block(content).unwrap().unwrap();
        assert!(
            !out.contains(WIKI_BLOCK_START) && !out.contains(WIKI_BLOCK_END),
            "标记应被移除: {out}"
        );
        assert!(
            out.contains("用户头部") && out.contains("用户尾部"),
            "用户内容应保留: {out}"
        );
    }

    /// remove：新旧标记块并存（升级残留的双块）→ 一并移除，用户内容保留
    #[test]
    fn test_remove_wiki_block_removes_both_marker_generations() {
        let content = "用户头部\n\n<!-- REPO-WIKI:START -->\n旧名块\n<!-- REPO-WIKI:END -->\n\n<!-- CODE-REPO-WIKI:START -->\n新块\n<!-- CODE-REPO-WIKI:END -->\n用户尾部\n";
        let out = remove_wiki_block(content).unwrap().unwrap();
        assert!(
            !out.contains("REPO-WIKI") && !out.contains("CODE-REPO-WIKI"),
            "两代标记都应被移除, 实际: {out}"
        );
        assert!(
            out.contains("用户头部") && out.contains("用户尾部"),
            "用户内容应保留: {out}"
        );
    }

    /// remove：半标记同样报错（当前标记与旧标记都检）
    #[test]
    fn test_remove_wiki_block_half_marker_errors() {
        let err = remove_wiki_block("<!-- CODE-REPO-WIKI:START -->\n").unwrap_err();
        assert!(err.to_string().contains("不完整"), "半标记应报错: {err}");
        let err = remove_wiki_block("<!-- REPO-WIKI:START -->\n").unwrap_err();
        assert!(
            err.to_string().contains("不完整"),
            "旧标记半标记应报错: {err}"
        );
    }
}
