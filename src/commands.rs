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

/// 汇总产物状态：页面/卡片数量 + lint 健康检查（供 `repo-wiki status` 使用）
///
/// 目录不存在时数量计 0、lint 无问题，不算缺陷（未生成也是合法状态）。
pub fn status_report(config: &WikiConfig) -> StatusReport {
    let output_dir = Path::new(&config.output.dir);
    // ready = wiki 目录存在且含 .md 文件（有产物才算生成过）
    let wiki_pages = collect_md_files(&output_dir.join("wiki")).len();
    let cards = collect_md_files(&output_dir.join("cards")).len();
    let issues = lint(output_dir, &source_roots_from_include(&config.scope.include));
    StatusReport {
        ready: wiki_pages > 0,
        wiki_pages,
        cards,
        issues,
        config_path: config.output.dir.clone(),
    }
}

/// 从 scope.include 派生源码根（取通配符前的目录前缀，如 "src/**" → "src"）
///
/// lint 过时检查需要对比源文件 mtime，空根会导致检查静默跳过；
/// main.rs 的 lint 命令与 status 共用此派生，避免两处内联逻辑漂移。
pub fn source_roots_from_include(include: &[String]) -> Vec<PathBuf> {
    include
        .iter()
        .map(|p| {
            let dir = p.split('*').next().unwrap_or_default().trim_end_matches('/');
            PathBuf::from(if dir.is_empty() { "." } else { dir })
        })
        .collect()
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

/// repo-wiki 安装: 配置 Agent 插件 + git hooks + 默认配置
pub fn install(agent: &str) -> Result<()> {
    let project_root = std::env::current_dir()?;

    // 1. 配置 OpenCode 插件 (如果 agent 是 opencode)
    if agent == "opencode" {
        let mut oc = crate::config::opencode::OpenCodeConfig::new()
            .context("读取 OpenCode 配置失败")?;
        oc.install_plugin()?;
        if oc.install_plugin_file()? {
            println!("✓ OpenCode 插件文件已安装");
        } else {
            println!("✓ OpenCode 插件文件已存在，跳过");
        }
    }

    // 2. 生成项目根 .mcp.json（Claude Code/Cursor/VS Code 等 MCP 客户端注册 repo-wiki server）
    match install_mcp_config(&project_root)? {
        true => println!("✓ .mcp.json 已生成（Claude Code/Cursor 等客户端可用）"),
        false => println!("✓ .mcp.json 已存在，跳过（保留人工修改）"),
    }

    // 3. 创建默认 .repo-wiki/config.toml (如果不存在)
    let config_path = project_root.join(".repo-wiki").join("config.toml");
    if !config_path.exists() {
        std::fs::create_dir_all(config_path.parent().unwrap())?;
        let default_config = include_str!("../default-config.toml");
        std::fs::write(&config_path, default_config)?;
        println!("✓ 默认配置已创建: .repo-wiki/config.toml");
    }

    // 3. 安装 git hooks
    let hooks_dir = project_root.join(".git").join("hooks");
    let hook_content = "#!/bin/sh\n# repo-wiki: auto-update wiki on commit\ncd \"$(git rev-parse --show-toplevel)\"\ncommand -v repo-wiki >/dev/null 2>&1 || exit 0\nrepo-wiki update 2>/dev/null || true\n";
    if hooks_dir.exists() {
        let post_commit = hooks_dir.join("post-commit");
        if !post_commit.exists() {
            std::fs::write(&post_commit, hook_content)?;
            #[cfg(unix)]
            std::fs::set_permissions(&post_commit, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            println!("✓ git post-commit hook 已安装");
        }

        let post_merge = hooks_dir.join("post-merge");
        if !post_merge.exists() {
            std::fs::write(&post_merge, hook_content)?;
            #[cfg(unix)]
            std::fs::set_permissions(&post_merge, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            println!("✓ git post-merge hook 已安装");
        }
    } else {
        println!("未检测到 .git 目录，跳过 git hook 安装");
    }

    println!("✓ repo-wiki 安装完成");
    Ok(())
}

/// repo-wiki 卸载: 移除 Agent 插件 + git hooks + 可选数据
pub fn uninstall(force: bool) -> Result<()> {
    let project_root = std::env::current_dir()?;

    if !force {
        println!("警告: 卸载将移除 repo-wiki 集成配置。");
        println!("数据目录 .repo-wiki/ 不会被删除（使用 --force 跳过确认）。");
        anyhow::bail!("请添加 --force 参数确认卸载");
    }

    // 1. 移除 OpenCode 插件
    let mut oc = crate::config::opencode::OpenCodeConfig::new()
        .context("读取 OpenCode 配置失败")?;
    oc.uninstall_plugin()?;
    oc.uninstall_plugin_file()?;
    println!("✓ OpenCode 插件已移除");

    // 2. 移除 git hooks
    let hooks_dir = project_root.join(".git").join("hooks");
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
            if content.contains("repo-wiki") {
                std::fs::remove_file(&hook_path)?;
                println!("✓ git {} hook 已移除", hook_name);
            }
        }
    }

    // 3. 移除 .mcp.json 中的 repo-wiki server 条目（Claude Code/Cursor 等客户端配置）
    remove_mcp_config(&project_root)?;
    println!("✓ .mcp.json 已清理（Claude Code/Cursor 等客户端配置）");

    println!("✓ repo-wiki 卸载完成 (数据保留: .repo-wiki/)");
    Ok(())
}

/// 生成项目根 `.mcp.json`（Claude Code/Cursor/VS Code 等 MCP 客户端注册 repo-wiki server）
///
/// - 已存在时不覆盖（保留人工修改，如用户自定义的 server 条目），返回 `false` 表示跳过
/// - 返回 `true` 表示实际写入
/// - 用 serde_json::Value 构造（本项目 serde_json 启用了 preserve_order，
///   字段按书写顺序序列化），统一走 fs::write_file_atomic 原子写
pub fn install_mcp_config(project_root: &Path) -> Result<bool> {
    let path = project_root.join(".mcp.json");
    // 存在即跳过：.mcp.json 是人工可编辑的配置文件，
    // 覆盖会丢失用户的其他 server 条目（与插件文件"已存在不覆盖"同策略）
    if path.exists() {
        return Ok(false);
    }
    // 字段顺序按书写顺序保留（preserve_order）：mcpServers → repo-wiki → command/args/type
    let content = serde_json::json!({
        "mcpServers": {
            "repo-wiki": {
                "command": "repo-wiki",
                "args": ["mcp", "--root", "."],
                "type": "stdio"
            }
        }
    });
    crate::fs::write_file_atomic(&path, &serde_json::to_string_pretty(&content)?)?;
    Ok(true)
}

/// 从项目根 `.mcp.json` 移除 repo-wiki server 条目（uninstall 配套清理）
///
/// - 文件不存在 → 静默成功（幂等，与插件文件移除语义一致）
/// - 移除 mcpServers.repo-wiki 后 mcpServers 为空 → 删除整个文件
/// - 移除后仍有其他 server → 写回剩余内容（保留其他客户端配置）
/// - JSON 解析失败 → 显式报错（契约：损坏的配置文件不能被静默吞掉，
///   用户需要知道文件坏了，而不是无声地"清理"了它）
pub fn remove_mcp_config(project_root: &Path) -> Result<()> {
    let path = project_root.join(".mcp.json");
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("解析 .mcp.json 失败（拒绝静默跳过）: {}", path.display()))?;

    // mcpServers 非对象（数组/字符串等畸形结构）时视为无 repo-wiki 条目，
    // 文件原样保留，不重写不删除
    let mut removed = false;
    if let Some(servers) = value.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        removed = servers.remove("repo-wiki").is_some();
        if removed && servers.is_empty() {
            // mcpServers 已无任何 server → 整个文件失去价值，直接删除
            std::fs::remove_file(&path)?;
            return Ok(());
        }
    }
    if removed {
        crate::fs::write_file_atomic(&path, &serde_json::to_string_pretty(&value)?)?;
    }
    Ok(())
}

/// wiki 引用块的起始标记（注入块的唯一边界）
pub const WIKI_BLOCK_START: &str = "<!-- REPO-WIKI:START -->";

/// wiki 引用块的结束标记
pub const WIKI_BLOCK_END: &str = "<!-- REPO-WIKI:END -->";

/// 注入块的固定模板（含标记对，install-wiki 与 --also-claude 共用）
///
/// 内容为中文 markdown 指针风格：只引产物路径与常用命令，不复制 wiki
/// 正文（避免与 LLM 生成的产物内容双份漂移）。以换行结尾，保证追加/
/// 替换后与相邻内容衔接干净。固定为常量是测试断言的锚点。
pub const WIKI_BLOCK_TEMPLATE: &str = "\
<!-- REPO-WIKI:START -->
本仓库使用 repo-wiki 维护可持续进化的项目 Wiki，产物位于 `wiki/`。

## AI 代理使用指引

1. 先读 `wiki/wiki/zh/overview.md` 与 `wiki/wiki/zh/architecture.md` 建立全局认知，
   再按需深入模块页。
2. 查找实体（函数/结构体/类）用 `repo-wiki search -q \"<关键词>\"`（支持
   text/semantic/hybrid 三引擎，hybrid 含调用链补全）。
3. 修改代码后运行 `repo-wiki update` 增量更新；`repo-wiki lint` 检查产物健康。
4. 知识沉淀：`repo-wiki note \"<记录>\"` 追加到 `wiki/wiki/zh/_log.md`。
<!-- REPO-WIKI:END -->
";

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
fn write_wiki_block(path: &Path) -> Result<()> {
    // 文件不存在视为空文档（正常创建路径），读取失败才显式报错
    let content = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("读取文件失败: {}", path.display()))?
    } else {
        String::new()
    };
    let new_content = inject_wiki_block(&content, WIKI_BLOCK_TEMPLATE)?;
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
pub fn install_wiki(root: &crate::project::ProjectRoot, also_claude: bool) -> Result<()> {
    let agents_path = root.join(Path::new("AGENTS.md"));
    write_wiki_block(&agents_path)?;
    println!("✓ wiki 引用块已注入 {}", agents_path.display());
    if also_claude {
        let claude_path = root.join(Path::new("CLAUDE.md"));
        write_wiki_block(&claude_path)?;
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

    /// append_note 追加式日志：同一日期节内序号递增；两次调用不覆盖历史
    #[test]
    fn test_append_note_increments_sequence() {
        let dir = std::env::temp_dir().join(format!(
            "repo_wiki_note_{}",
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
            "repo_wiki_note_empty_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(append_note(&dir, "zh", "   ").is_err(), "空内容应报错");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 创建隔离临时目录（进程 id 后缀，避免并行测试互相干扰）
    fn mcp_config_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("repo_wiki_mcp_cfg_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// install_mcp_config 首次调用实际写入 .mcp.json，内容为 repo-wiki stdio server
    #[test]
    fn test_install_mcp_config_writes_file() {
        let dir = mcp_config_dir("write");
        let wrote = install_mcp_config(&dir).unwrap();
        assert!(wrote, "首次调用应实际写入");
        let content = std::fs::read_to_string(dir.join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let server = &v["mcpServers"]["repo-wiki"];
        assert_eq!(server["command"], "repo-wiki", "command 应为 repo-wiki, 实际: {content}");
        assert_eq!(
            server["args"],
            serde_json::json!(["mcp", "--root", "."]),
            "args 应含 mcp/--root/., 实际: {content}"
        );
        assert_eq!(server["type"], "stdio", "type 应为 stdio, 实际: {content}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已存在的 .mcp.json（含人工修改）不被覆盖，返回 false
    #[test]
    fn test_install_mcp_config_preserves_existing() {
        let dir = mcp_config_dir("preserve");
        let path = dir.join(".mcp.json");
        let manual = "{\"mcpServers\":{\"custom\":{\"command\":\"custom-tool\"}}}";
        std::fs::write(&path, manual).unwrap();
        let wrote = install_mcp_config(&dir).unwrap();
        assert!(!wrote, "已存在时应跳过");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            manual,
            "已存在的文件内容不应被覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// remove_mcp_config 只移除 repo-wiki 条目，保留其他 server
    #[test]
    fn test_remove_mcp_config_removes_entry_only() {
        let dir = mcp_config_dir("remove_entry");
        let path = dir.join(".mcp.json");
        std::fs::write(
            &path,
            "{\"mcpServers\":{\"repo-wiki\":{\"command\":\"repo-wiki\"},\"other\":{\"command\":\"other-tool\"}}}",
        )
        .unwrap();
        remove_mcp_config(&dir).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["mcpServers"].get("repo-wiki").is_none(), "repo-wiki 条目应被移除");
        assert_eq!(
            v["mcpServers"]["other"]["command"], "other-tool",
            "其他 server 条目应保留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 仅含 repo-wiki 一个 server 时，移除后整个文件被删除
    #[test]
    fn test_remove_mcp_config_deletes_file_when_empty() {
        let dir = mcp_config_dir("delete_file");
        let path = dir.join(".mcp.json");
        std::fs::write(&path, "{\"mcpServers\":{\"repo-wiki\":{\"command\":\"repo-wiki\"}}}").unwrap();
        remove_mcp_config(&dir).unwrap();
        assert!(!path.exists(), "mcpServers 清空后文件应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在时静默成功（幂等）
    #[test]
    fn test_remove_mcp_config_missing_file_is_silent() {
        let dir = mcp_config_dir("missing");
        remove_mcp_config(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JSON 解析失败显式报错（不静默吞掉损坏文件）
    #[test]
    fn test_remove_mcp_config_invalid_json_errors() {
        let dir = mcp_config_dir("invalid_json");
        let path = dir.join(".mcp.json");
        std::fs::write(&path, "not json{{{").unwrap();
        let err = remove_mcp_config(&dir).unwrap_err();
        assert!(err.to_string().contains("解析 .mcp.json 失败"), "应显式报错: {err}");
        assert!(path.exists(), "解析失败时不应删除文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== wiki 引用块注入/移除 ====================

    /// 全新注入：空文档 → 追加完整标记对，结果恰等于模板本身
    #[test]
    fn test_inject_wiki_block_fresh() {
        let out = inject_wiki_block("", WIKI_BLOCK_TEMPLATE).unwrap();
        assert_eq!(out, WIKI_BLOCK_TEMPLATE, "空文档注入结果应等于模板本身");
        assert!(out.contains(WIKI_BLOCK_START) && out.contains(WIKI_BLOCK_END));
    }

    /// 幂等替换：已含完整标记对 → 旧块整体替换为模板，用户前后内容保留
    #[test]
    fn test_inject_wiki_block_replaces_existing() {
        let before = "用户头部\n\n<!-- REPO-WIKI:START -->\n旧块内容\n<!-- REPO-WIKI:END -->\n\n用户尾部\n";
        let out = inject_wiki_block(before, WIKI_BLOCK_TEMPLATE).unwrap();
        assert!(out.starts_with("用户头部\n\n"), "用户头部应保留, 实际: {out}");
        assert!(out.ends_with("用户尾部\n"), "用户尾部应保留, 实际: {out}");
        assert!(out.contains(WIKI_BLOCK_TEMPLATE), "旧块应被替换为模板, 实际: {out}");
        assert!(!out.contains("旧块内容"), "旧块内容应被替换掉, 实际: {out}");
    }

    /// 幂等：同一文档注入两次 → 结果一致（第二次走替换路径）
    #[test]
    fn test_inject_wiki_block_twice_stable() {
        let first = inject_wiki_block("头部\n", WIKI_BLOCK_TEMPLATE).unwrap();
        let second = inject_wiki_block(&first, WIKI_BLOCK_TEMPLATE).unwrap();
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
            let err = inject_wiki_block(case, WIKI_BLOCK_TEMPLATE).unwrap_err();
            assert!(err.to_string().contains("不完整"), "半标记应报错: {err}");
        }
    }

    /// 保留用户内容：无标记追加场景下用户内容完整保留在块之前
    #[test]
    fn test_inject_wiki_block_preserves_user_content() {
        let before = "# 我的项目\n\n这是用户写的说明。\n";
        let out = inject_wiki_block(before, WIKI_BLOCK_TEMPLATE).unwrap();
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
