//! 多 Agent MCP 配置读写模块（v33；v39 落点统一为用户级）
//!
//! 合并 install 的多 Agent 支持：`install` 默认注册 opencode（用户级全局
//! `~/.config/opencode/opencode.json` 的 `mcp` 块），`--claude` 额外写
//! 用户级 `~/.claude.json` 顶层 `mcpServers`（User scope——command 绑定
//! 本机 exe 路径=用户级内容，v39 起不再写项目根 `.mcp.json`），`--codex`
//! 额外写用户级 `~/.codex/config.toml` 的 `[mcp_servers.<name>]` 表，
//! `--dsh`（W3）额外写**项目根** `cordis.patch.yml`（DeepSeek Harness 的
//! patch 层——dsh 不读 `.mcp.json`，MCP server 必须显式配置在 patch 层；
//! AGENTS.md/CLAUDE.md 由 dsh 自动读取作为 instruction file，零成本基线）。
//!
//! 四个 writer（`OpencodeMcp`/`ClaudeMcp`/`CodexMcp`/`DshMcp`）各自封装
//! 一种格式，共同契约：
//! - `install(server)`：条目已存在且命令一致 → 返回 `false`（跳过）；
//!   存在但命令不同（升级，如二进制路径变化）→ 更新返回 `true`；
//!   不存在 → 新建返回 `true`
//! - `remove(server)`：文件缺失/无条目 → 幂等返回 `false`；移除后空容器
//!   的清理语义各格式不同（opencode 删 `mcp` 键、Claude 保留空 `mcpServers`
//!   对象——`~/.claude.json` 承载 OAuth 会话，绝不删整个文件、Codex 删表、
//!   dsh 删标记块/裸行区间并清理残留空行）
//! - 只动本 server 的条目，绝不触碰其他 server/配置键（多 Agent 共存；
//!   Claude/opencode 的用户配置键——OAuth、provider 等——原样保留）
//! - 畸形文件（JSON 非对象/TOML 解析失败）→ 显式报错，拒绝静默处理
//!   （契约与 v32 commands::remove_mcp_config 一致：损坏的配置文件不能被静默吞掉）

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::fs::write_file_atomic;

/// 用户主目录（Codex/opencode 用户级配置的基础）
///
/// Windows 语义（N11 先例）：USERPROFILE 优先于 HOME——Windows 构建工具
/// （Git Bash/Cygwin/MSYS）常把 HOME 指向临时值，USERPROFILE 才是用户
/// 真实主目录。两者都缺失时显式报错，不静默写当前目录（与
/// [`crate::config::global_config_dir`] 的「写错位置比报错更隐蔽」同语义）。
pub fn user_home() -> Result<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("无法确定用户主目录（USERPROFILE 与 HOME 均未设置）"))
}

// ---------------------------------------------------------------------------
// opencode（用户级全局 `mcp` 块）
// ---------------------------------------------------------------------------

/// opencode MCP 配置读写（`~/.config/opencode/opencode.json`）
///
/// 官方格式（opencode.ai/docs/mcp-servers，V1 稳定版，V2 beta 兼容）：
/// 顶层 `mcp` 键 → 服务器名 → `{ "type": "local", "command": [...],
/// "enabled": true }`。全局与项目配置按服务器名逐条合并、项目覆盖全局；
/// 本 writer 只写用户级（install 拍板：一次注册所有仓库可用，
/// server 以工作区为 cwd，无需 --root）。
pub struct OpencodeMcp {
    /// 用户级 opencode.json 路径
    pub config_path: PathBuf,
}

impl OpencodeMcp {
    /// 用户级 opencode 配置路径（`~/.config/opencode/opencode.json`）
    pub fn global_path() -> Result<PathBuf> {
        Ok(user_home()?
            .join(".config")
            .join("opencode")
            .join("opencode.json"))
    }

    /// 注册/更新 MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 视为空 `{}` 新建（opencode 支持全空配置）
    /// - `mcp` 键缺失 → 新建；已有其他 server → 保留（多 server 共存）
    /// - 本 server 已存在且 command 一致 → `false`（幂等跳过）
    /// - 本 server 已存在但 command 不同 → 整体替换该条目（升级）
    /// - 顶层非 JSON 对象 → 显式报错（N12 规则：数组/标量是损坏配置）
    pub fn install(&self, server: &str, command: &[String]) -> Result<bool> {
        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
            Err(e) => {
                // v52 T08a：读失败（权限/占用/损坏）时显式中止——静默当空对象
                // 会让后续写回**覆盖含 OAuth 会话的用户配置**（凭据丢失）。
                anyhow::bail!(
                    "读取 {} 失败（已中止写回，避免覆盖现有配置）: {e}",
                    self.config_path.display()
                );
            }
        };
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析 opencode 配置失败: {}", self.config_path.display()))?;
        if !value.is_object() {
            anyhow::bail!(
                "opencode.json 顶层应为 JSON 对象: {}",
                self.config_path.display()
            );
        }

        // mcp 块：只读旧条目做变更判定，随后整体重建该 server 条目
        let mcp = value.get("mcp").and_then(|v| v.as_object());
        let unchanged = mcp
            .and_then(|m| m.get(server))
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_array())
            .is_some_and(|cur| {
                cur.iter()
                    .filter_map(|v| v.as_str())
                    .eq(command.iter().map(String::as_str))
            });
        if unchanged {
            return Ok(false);
        }

        let entry = serde_json::json!({
            "type": "local",
            "command": command,
            "enabled": true
        });
        let obj = value.as_object_mut().unwrap();
        let mcp_block = obj
            .entry("mcp")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        mcp_block
            .as_object_mut()
            .with_context(|| {
                format!(
                    "opencode.json 的 mcp 键应为对象: {}",
                    self.config_path.display()
                )
            })?
            .insert(server.to_string(), entry);

        let output = serde_json::to_string_pretty(&value).context("序列化 opencode 配置失败")?;
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        write_file_atomic(&self.config_path, &output)?;
        Ok(true)
    }

    /// 移除 MCP server；返回是否实际移除
    ///
    /// - 文件缺失/无 mcp 键/无该 server → `false`（幂等）
    /// - 移除后 mcp 块为空 → 整个删掉 `mcp` 键（不留空容器）
    /// - 其他键与其他 server 保留
    pub fn remove(&self, server: &str) -> Result<bool> {
        if !self.config_path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("读取 opencode 配置失败: {}", self.config_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析 opencode 配置失败: {}", self.config_path.display()))?;
        if !value.is_object() {
            anyhow::bail!(
                "opencode.json 顶层应为 JSON 对象: {}",
                self.config_path.display()
            );
        }

        let removed = {
            let mcp = value.get_mut("mcp").and_then(|v| v.as_object_mut());
            match mcp {
                None => false,
                Some(m) => {
                    let hit = m.remove(server).is_some();
                    if hit && m.is_empty() {
                        // mcp 块已无任何 server → 整个键失去价值，删除
                        value.as_object_mut().unwrap().remove("mcp");
                    }
                    hit
                }
            }
        };
        if !removed {
            return Ok(false);
        }
        let output = serde_json::to_string_pretty(&value).context("序列化 opencode 配置失败")?;
        write_file_atomic(&self.config_path, &output)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Claude Code（用户级 `~/.claude.json` 顶层 `mcpServers`，User scope）
// ---------------------------------------------------------------------------

/// Claude Code MCP 配置读写（用户级 `~/.claude.json` 顶层 `mcpServers`）
///
/// Claude Code 官方格式（code.claude.com/docs/en/mcp）：User scope 的 MCP
/// 服务器注册在 `~/.claude.json` 顶层 `mcpServers` → 服务器名 →
/// `{ "type": "stdio", "command": "<string>", "args": [...] }`，对所有项目生效。
///
/// 落点论证（v39）：MCP server 的 `command` 绑定本机可执行文件绝对路径
/// （机器相关=用户级内容），与 opencode 全局配置、Codex `~/.codex/config.toml`
/// 同语义——一次 install 所有仓库的 Claude 会话可用；项目根 `.mcp.json`
/// （Project scope，团队共享入 git）不再写入。
///
/// 与 opencode 格式不兼容（键名/command 形态/env 变量语法均不同），
/// 故独立 writer。`~/.claude.json` 承载 OAuth 会话等大量用户配置：
/// 只动顶层 `mcpServers` 本 server 条目，其他键与人工修改一律保留。
pub struct ClaudeMcp {
    /// 用户级 `~/.claude.json` 路径
    pub path: PathBuf,
}

impl ClaudeMcp {
    /// 用户级 `~/.claude.json` 路径（`%USERPROFILE%/.claude.json`）
    pub fn user_global_path() -> Result<PathBuf> {
        Ok(user_home()?.join(".claude.json"))
    }

    /// 注册/更新 MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 新建（仅含 mcpServers 容器，其余键留给 Claude 会话写入）
    /// - 本 server 存在且 command+args 一致 → `false`
    /// - 本 server 存在但命令不同 → 更新（升级）
    /// - OAuth 会话与其他 server 条目原样保留
    pub fn install(&self, server: &str, command: &str, args: &[String]) -> Result<bool> {
        // v52 T08a：.claude.json 承载 OAuth 会话——非 NotFound 读失败（权限/占用/损坏）
        // 不得静默当空对象后原子覆盖（与 opencode.json 同语义，见 OpencodeMcp::install）
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
            Err(e) => anyhow::bail!(
                "读取 Claude 用户配置失败（已中止写回，避免覆盖现有配置）: {}: {e}",
                self.path.display()
            ),
        };
        let mut value: serde_json::Value = if content.trim().is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            serde_json::from_str(&content)
                .with_context(|| format!("解析 Claude 用户配置失败: {}", self.path.display()))?
        };
        if !value.is_object() {
            anyhow::bail!("Claude 用户配置顶层应为 JSON 对象: {}", self.path.display());
        }

        let unchanged = value
            .get("mcpServers")
            .and_then(|v| v.get(server))
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str())
            .is_some_and(|cur| {
                cur == command
                    && value
                        .get("mcpServers")
                        .and_then(|v| v.get(server))
                        .and_then(|v| v.get("args"))
                        .and_then(|v| v.as_array())
                        .is_some_and(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .eq(args.iter().map(String::as_str))
                        })
            });
        if unchanged {
            return Ok(false);
        }

        let entry = serde_json::json!({
            "type": "stdio",
            "command": command,
            "args": args
        });
        let obj = value.as_object_mut().unwrap();
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        servers
            .as_object_mut()
            .with_context(|| {
                format!(
                    "Claude 用户配置的 mcpServers 键应为对象: {}",
                    self.path.display()
                )
            })?
            .insert(server.to_string(), entry);

        let output = serde_json::to_string_pretty(&value).context("序列化 Claude 用户配置失败")?;
        write_file_atomic(&self.path, &output)?;
        Ok(true)
    }

    /// 移除 MCP server；返回是否实际移除（语义与 v32 commands::remove_mcp_config
    /// 相同，迁入本模块统一管理；v39 落点改用户级 `~/.claude.json`）
    ///
    /// - 文件缺失 → `false`（幂等）
    /// - 移除后 mcpServers 为空 → 保留空对象写回（`~/.claude.json` 承载 OAuth
    ///   会话等用户配置，**绝不删除整个文件**；与 opencode.json 同语义）
    /// - 移除后仍有其他 server → 原子写回保留
    /// - JSON 解析失败 → 显式报错（拒绝静默清理损坏配置）
    pub fn remove(&self, server: &str) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("读取 Claude 用户配置失败: {}", self.path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&content).with_context(|| {
            format!(
                "解析 Claude 用户配置失败（拒绝静默跳过）: {}",
                self.path.display()
            )
        })?;

        let removed = {
            let servers = value.get_mut("mcpServers").and_then(|v| v.as_object_mut());
            match servers {
                None => false,
                Some(s) => s.remove(server).is_some(),
            }
        };
        if !removed {
            return Ok(false);
        }
        write_file_atomic(&self.path, &serde_json::to_string_pretty(&value)?)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Codex CLI（用户级 `~/.codex/config.toml` 的 `[mcp_servers.<name>]` 表）
// ---------------------------------------------------------------------------

/// Codex CLI MCP 配置读写（`~/.codex/config.toml`）
///
/// 官方格式（developers.openai.com/codex/mcp）：`[mcp_servers.<name>]` 表，
/// stdio 用 `command`（必填）+ `args`。项目级 `.codex/config.toml` 只在
/// 项目被信任时加载，install 场景写全局文件（一次注册所有仓库可用）。
///
/// 用**文本级表编辑**而非 toml::Value 往返：Codex 配置可能含用户手写的
/// 注释与其他表（provider/auth 等），toml::Value 序列化会重排整文件丢注释；
/// 本 writer 只替换/追加本 server 的虚线表块，其余内容原样保留。
pub struct CodexMcp {
    /// 用户级 Codex 配置路径
    pub config_path: PathBuf,
}

/// TOML basic string 转义（路径含 Windows 反斜杠必须转义，否则解析失败）
fn toml_basic_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

impl CodexMcp {
    /// 用户级 Codex 配置路径（`~/.codex/config.toml`）
    pub fn global_path() -> Result<PathBuf> {
        Ok(user_home()?.join(".codex").join("config.toml"))
    }

    /// 本 server 的虚线表头（`[mcp_servers.<name>]`）
    fn table_header(server: &str) -> String {
        format!("[mcp_servers.{server}]")
    }

    /// 定位虚线表块（[header] 起始行到下一个表头/EOF 的行区间）
    ///
    /// 返回 (起始行号 0-based, 结束行号不包含, 是否命中)。表头行要求
    /// 行首 `[`（TOML 表头必须行首）且精确匹配 header 字符串。
    fn find_table(content: &str, header: &str) -> Option<(usize, usize)> {
        let lines: Vec<&str> = content.lines().collect();
        let mut start: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            match start {
                None => {
                    // 未找到目标表：目标表头必须精确匹配（行首 [ 且整行等于 header）
                    if trimmed.starts_with('[') && line.trim() == header {
                        start = Some(i);
                    }
                }
                Some(s) => {
                    // 已找到目标表：下一个行首 [ 的表头（非注释内）即表块结束
                    if trimmed.starts_with('[') {
                        return Some((s, i));
                    }
                }
            }
        }
        start.map(|s| (s, lines.len()))
    }

    /// 注册/更新 MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 新建（内容仅本表）
    /// - 表已存在且 command 相同 → `false`；command 不同 → 整块替换（升级）
    /// - 其他表/注释原样保留
    pub fn install(&self, server: &str, command: &str, args: &[String]) -> Result<bool> {
        let header = Self::table_header(server);
        let body = format!(
            "{}\ncommand = \"{}\"\nargs = [{}]\n",
            header,
            toml_basic_escape(command),
            args.iter()
                .map(|a| format!("\"{}\"", toml_basic_escape(a)))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context("读取 Codex 配置失败"),
        };

        match Self::find_table(&content, &header) {
            Some((start, end)) => {
                // 表已存在：比对 command 是否一致（一致 → 跳过，否则替换块）
                let current: Vec<&str> = content.lines().collect();
                let block = current[start..end].join("\n");
                if block.contains(&format!("command = \"{}\"", toml_basic_escape(command))) {
                    return Ok(false);
                }
                // 组装替换：保留行尾换行一致性——用原始行重建（含换行符）
                let mut out = String::with_capacity(content.len() + body.len());
                // content 逐行重建直到 start
                let mut line_iter = content.split_inclusive('\n');
                for _ in 0..start {
                    if let Some(l) = line_iter.next() {
                        out.push_str(l);
                    }
                }
                // 跳过旧块（start..end），写入新块（表体以换行结尾）
                for _ in start..end {
                    let _ = line_iter.next();
                }
                out.push_str(&body);
                // 旧块之后的行：若紧跟旧块末尾的是空行，保留其原有前缀逻辑
                for l in line_iter {
                    out.push_str(l);
                }
                write_file_atomic(&self.config_path, &out)?;
                Ok(true)
            }
            None => {
                // 不存在：文件尾追加（空文件直接写；有内容则补一个空行分隔）
                let separator = if content.is_empty() || content.ends_with("\n\n") {
                    ""
                } else if content.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                if let Some(parent) = self.config_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
                }
                write_file_atomic(&self.config_path, &format!("{content}{separator}{body}"))?;
                Ok(true)
            }
        }
    }

    /// 移除 MCP server 表；返回是否实际移除
    ///
    /// - 文件缺失/无该表 → `false`（幂等）
    /// - 移除后其他表/注释保留
    pub fn remove(&self, server: &str) -> Result<bool> {
        if !self.config_path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("读取 Codex 配置失败: {}", self.config_path.display()))?;
        let header = Self::table_header(server);
        match Self::find_table(&content, &header) {
            Some((start, end)) => {
                // 重建：跳过 start..end 区间（含表头行与块内全部行）。
                // 块后可能残留空行：若移除后出现连续空行，压缩为单空行。
                let mut out = String::with_capacity(content.len());
                let mut line_iter = content.split_inclusive('\n');
                for _ in 0..start {
                    if let Some(l) = line_iter.next() {
                        out.push_str(l);
                    }
                }
                for _ in start..end {
                    let _ = line_iter.next();
                }
                // 剩余行：跳过块后紧跟的一个空行（块删除后留下双空行）
                let mut skip_blank = true;
                for l in line_iter {
                    if skip_blank && l.trim().is_empty() {
                        skip_blank = false;
                        continue;
                    }
                    skip_blank = false;
                    out.push_str(l);
                }
                // 尾部若以多个空行结尾，压缩为单换行
                while out.ends_with("\n\n") {
                    out.pop();
                }
                write_file_atomic(&self.config_path, &out)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// DeepSeek Harness / dsh（项目根 `cordis.patch.yml` 的 `- insert:` patch 层）
// ---------------------------------------------------------------------------

/// dsh patch 层管理块开始标记（YAML 注释——对 dsh 解析无副作用，仅作本工具
/// 识别/替换/移除的边界；与 commands.rs 的 hook 标记块同思路）
pub const DSH_BLOCK_START: &str = "# code-repo-wiki: dsh-mcp-client insert-begin";

/// dsh patch 层管理块结束标记
pub const DSH_BLOCK_END: &str = "# code-repo-wiki: dsh-mcp-client insert-end";

/// dsh MCP 客户端插件名（deepseek-ai/deepseek-harness 官方包）
pub const DSH_MCP_CLIENT_NAME: &str = "@deepseek-ai/dsh-mcp-client";

/// DeepSeek Harness（`dsh`）MCP 配置读写（项目根 `cordis.patch.yml`）
///
/// 落点论证（W3 调研核证）：dsh 是 agent 运行时（developer preview，兼容性
/// 破坏性变更警告），**不读 `.mcp.json`**，MCP server 必须显式配置在 patch
/// 层。官方 `cordis.patch.yml` 顶层是一个 patch 操作列表（YAML 数组），
/// 其中 `- insert:` 后跟插件行列表；MCP 客户端插件 `@deepseek-ai/dsh-mcp-client`
/// 以 stdio transport 启动子进程，工具以 `mcp__<serverName>__<rawName>`
/// 暴露，`serverName` 限 `[A-Za-z0-9_-]{1,32}`（"code-repo-wiki" 15 字符合法）。
/// dsh 自动读取目标仓库 AGENTS.md/CLAUDE.md 作为 instruction file，文档指引
/// 零成本获得（install_wiki 注入的块对 dsh 会话同样生效）。
///
/// 官方示例形态（packages/mcp/mcp-client/README.md 与 examples/mcp-memory/）：
/// ```yaml
/// - insert:
///     - id: memory-memorix
///       name: '@deepseek-ai/dsh-mcp-client'
///       config:
///         serverName: memorix
///         transport: stdio
///         command: memorix
///         args: [serve]
///         cwd: !!js process.cwd()
/// ```
/// `cwd: !!js process.cwd()` 使 MCP 子进程以 dsh 工作区为 cwd（与 opencode
/// 用户级全局 MCP 的「server 以工作区为 cwd」同语义，repo-wiki 的 `mcp`
/// server 据此定位项目根）。
///
/// 本 writer 用**文本级行编辑**而非 YAML 序列化往返（与 CodexMcp 同理由：
/// 文件可能含用户手写的注释与其他 patch 操作，YAML 往返会重排丢注释）。
/// 写入内容以 `#[DSH_BLOCK_START]`/`#[DSH_BLOCK_END]` 标记块整体管理：
/// install 有标记块则替换（升级）、无则追加；uninstall 有标记块则整块删除、
/// 只有裸行（用户手工合入）则删裸行区间。
///
/// 生效路径说明（官方 apps/cli README）：dsh 按序加载 patch 层 = bundle 补丁
/// → profile 的 `cordis.patch.yml` → 用户级 `$DSH_HOME/cordis.patch.yml` →
/// `--patch` 覆盖层。**项目根 `cordis.patch.yml` 不会被自动加载**——本 writer
/// 落盘在项目根（W3 拍板），用户需把它合入 profile/home patch 层或经
/// `dsh --patch <root>/cordis.patch.yml` 显式引用后生效。
///
/// 格式以 dsh 官方文档为准，可能随上游破坏性变更——本模块注释同步上游变化即可。
pub struct DshMcp {
    /// 项目根 `cordis.patch.yml` 路径
    pub path: PathBuf,
}

impl DshMcp {
    /// 渲染管理块（标记对 + `- insert:` 一行插件行；结尾以换行收束）
    fn render_block(exe: &str) -> String {
        format!(
            "{0}\n- insert:\n    - id: code-repo-wiki\n      name: '{1}'\n      config:\n        serverName: code-repo-wiki\n        transport: stdio\n        command: \"{2}\"\n        args: [mcp]\n        cwd: !!js process.cwd()\n{3}\n",
            DSH_BLOCK_START,
            DSH_MCP_CLIENT_NAME,
            // YAML 双引号转义与 TOML basic string 同字符集（\\ \" \n \r \t），
            // 复用 toml_basic_escape：Windows 反斜杠路径必须转义否则解析失败
            toml_basic_escape(exe),
            DSH_BLOCK_END,
        )
    }

    /// 定位管理块区间（begin 标记行到 end 标记行含两端），返回
    /// (起始行号 0-based, 结束行号+1)。区间不完整（只出现一端）→ 保守不识别
    /// （返回 None，install 走裸行探测/追加，remove 走裸行探测，不破坏用户文件）。
    fn block_span(content: &str) -> Option<(usize, usize)> {
        let lines: Vec<&str> = content.lines().collect();
        let begin = lines.iter().position(|l| l.trim() == DSH_BLOCK_START);
        let end = lines.iter().position(|l| l.trim() == DSH_BLOCK_END);
        match (begin, end) {
            (Some(b), Some(e)) if b <= e => Some((b, e + 1)),
            _ => None,
        }
    }

    /// 定位裸行 `- id: code-repo-wiki` 的行号（用户手工合入、无管理标记）；
    /// 无则 None
    fn find_row_line(content: &str) -> Option<usize> {
        content
            .lines()
            .position(|l| l.trim_start() == "- id: code-repo-wiki")
    }

    /// 行缩进（前导空格数）
    fn indent_of(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    /// 定位从 start 行起始的 YAML 列表项区间（行号 start..end 不含 end）。
    /// 区间结束：遇空行继续（属本项尾部）、缩进不深于 start 的非空行
    /// （同层/更外层列表项或顶层项）即截断。子键（config 下的缩进行）更深，
    /// 天然落在区间内。
    fn row_span(lines: &[&str], start: usize) -> (usize, usize) {
        let base = Self::indent_of(lines[start]);
        let mut end = start + 1;
        while end < lines.len() {
            let l = lines[end];
            if l.trim().is_empty() {
                end += 1;
                continue;
            }
            if Self::indent_of(l) <= base {
                break;
            }
            end += 1;
        }
        (start, end)
    }

    /// 在行区间内定位 `command:` 行（裸行升级时替换落点）
    fn row_command_line(lines: &[&str], start: usize, end: usize) -> Option<usize> {
        lines[start..end]
            .iter()
            .position(|l| l.trim_start().starts_with("command:"))
            .map(|i| start + i)
    }

    /// 取裸行 `command:` 的标量值（剥引号 + 反转义 `\\`→`\`；plain scalar
    /// 原样返回）——与写盘用的 toml_basic_escape 对称，Windows 反斜杠路径
    /// 才能与 `exe` 参数字符串正确比对
    fn row_command_value(lines: &[&str], start: usize, end: usize) -> Option<String> {
        let idx = Self::row_command_line(lines, start, end)?;
        let raw = lines[idx].trim_start().trim_start_matches("command:");
        let v = raw.trim();
        Some(if let Some(inner) = v.strip_prefix('"') {
            inner
                .strip_suffix('"')
                .unwrap_or(inner)
                .replace("\\\\", "\\")
        } else {
            v.to_string()
        })
    }

    /// 按行区间重建内容：丢弃 [start, end) 区间，在 start 处插入 new
    fn splice(content: &str, start: usize, end: usize, new: &str) -> String {
        let mut out = String::with_capacity(content.len() + new.len());
        let mut line_iter = content.split_inclusive('\n');
        for _ in 0..start {
            if let Some(l) = line_iter.next() {
                out.push_str(l);
            }
        }
        for _ in start..end {
            let _ = line_iter.next();
        }
        out.push_str(new);
        for l in line_iter {
            out.push_str(l);
        }
        out
    }

    /// 压缩残留空行：连续空行只留一个、文件尾多余空行收束为单换行
    /// （块删除后必有的残留，防止 patch 文件末尾悬挂空行）
    fn cleanup_blank_lines(content: &str) -> String {
        let mut out = String::with_capacity(content.len());
        let mut prev_blank = false;
        for line in content.lines() {
            if line.trim().is_empty() {
                if prev_blank {
                    continue;
                }
                prev_blank = true;
            } else {
                prev_blank = false;
            }
            out.push_str(line);
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// 注册/更新 dsh MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 新建（内容仅本管理块）
    /// - 管理块已存在且内容一致 → `false`；不一致 → 整块替换（升级）
    /// - 无管理块但存在裸行（用户手工合入的 insert 行）→ 命令一致则 `false`；
    ///   不一致 → 删除裸行区间并追加管理块（升级为受管形态）
    /// - 无任何记录 → 文件尾追加管理块（其他 patch 操作/注释保留）
    pub fn install(&self, exe: &str) -> Result<bool> {
        let new_block = Self::render_block(exe);
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context("读取 cordis.patch.yml 失败"),
        };

        // 管理块存在：内容一致跳过，否则整块替换
        if let Some((b, e)) = Self::block_span(&content) {
            let current_block = content
                .split_inclusive('\n')
                .skip(b)
                .take(e - b)
                .collect::<String>();
            if current_block == new_block {
                return Ok(false);
            }
            Self::write(&self.path, &Self::splice(&content, b, e, &new_block))?;
            return Ok(true);
        }

        // 无管理块：探测用户手工合入的裸行（避免重复 id 造成 dsh 加载冲突）
        if let Some(row) = Self::find_row_line(&content) {
            let lines: Vec<&str> = content.lines().collect();
            let (s, e) = Self::row_span(&lines, row);
            let cmd_matches = Self::row_command_value(&lines, s, e).as_deref() == Some(exe);
            if cmd_matches {
                return Ok(false);
            }
            // 命令不一致：删旧裸行区间，追加受管块（升级为可整体替换形态）
            let removed = Self::cleanup_blank_lines(&Self::splice(&content, s, e, ""));
            let next = Self::append_block(&removed, &new_block);
            Self::write(&self.path, &next)?;
            return Ok(true);
        }

        // 无任何记录：文件尾追加管理块
        Self::write(&self.path, &Self::append_block(&content, &new_block))?;
        Ok(true)
    }

    /// 文件尾追加块（空文件直接写；已有内容则补一个空行分隔）
    fn append_block(existing: &str, block: &str) -> String {
        if existing.is_empty() {
            block.to_string()
        } else if existing.ends_with('\n') {
            format!("{existing}\n{block}")
        } else {
            format!("{existing}\n\n{block}")
        }
    }

    fn write(path: &std::path::Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        write_file_atomic(path, content)?;
        Ok(())
    }

    /// 移除 dsh MCP server；返回是否实际移除
    ///
    /// - 文件缺失 → `false`（幂等）
    /// - 管理块存在 → 整块删除（清理残留空行）
    /// - 无管理块但裸行存在 → 删除裸行区间（用户手工合入形态同样清除）
    /// - 其他 patch 操作/注释保留
    /// - 移除后文件为空 → 删除文件本身（空 `cordis.patch.yml` 在 YAML 中
    ///   解析为 `null`，dsh 的 patch 加载器预期一个操作列表——空文件比缺失
    ///   更易踩上游校验，删文件最稳妥）
    pub fn remove(&self) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("读取 cordis.patch.yml 失败: {}", self.path.display()))?;
        let removed_span = if let Some((b, e)) = Self::block_span(&content) {
            Some((b, e))
        } else if let Some(row) = Self::find_row_line(&content) {
            let lines: Vec<&str> = content.lines().collect();
            let (s, e) = Self::row_span(&lines, row);
            Some((s, e))
        } else {
            None
        };
        let Some((s, e)) = removed_span else {
            return Ok(false);
        };
        let out = Self::cleanup_blank_lines(&Self::splice(&content, s, e, ""));
        if out.trim().is_empty() {
            // 剩余内容为空（文件本工具独占且无用户内容）→ 删除文件
            std::fs::remove_file(&self.path)
                .with_context(|| format!("删除空 cordis.patch.yml 失败: {}", self.path.display()))?;
        } else {
            Self::write(&self.path, &out)?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 独立临时目录（防并行测试冲突），返回目录路径
    fn temp_dir(tag: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "code-repo-wiki-mcp-test-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).expect("写入临时文件失败");
    }

    // ---- OpencodeMcp ----

    #[test]
    fn opencode_install_creates_mcp_block() {
        let dir = temp_dir("oc-create");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        let cmd = vec!["/usr/bin/code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &cmd).unwrap());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &parsed["mcp"]["code-repo-wiki"];
        assert_eq!(entry["type"], "local");
        assert_eq!(entry["command"][0], "/usr/bin/code-repo-wiki");
        assert_eq!(entry["command"][1], "mcp");
        assert_eq!(entry["enabled"], true);
    }

    #[test]
    fn opencode_install_idempotent_skips_unchanged() {
        let dir = temp_dir("oc-idem");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        let cmd = vec!["code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &cmd).unwrap());
        assert!(!mcp.install("code-repo-wiki", &cmd).unwrap());
    }

    #[test]
    fn opencode_install_upgrades_changed_command() {
        let dir = temp_dir("oc-upgrade");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        let old = vec!["/old/code-repo-wiki".to_string(), "mcp".to_string()];
        let new = vec!["/new/code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &old).unwrap());
        assert!(mcp.install("code-repo-wiki", &new).unwrap());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed["mcp"]["code-repo-wiki"]["command"][0],
            "/new/code-repo-wiki"
        );
    }

    #[test]
    fn opencode_install_preserves_other_servers() {
        let dir = temp_dir("oc-preserve");
        let path = dir.join("opencode.json");
        write(
            &path,
            r#"{"mcp": {"other": {"type": "local", "command": ["npx", "x"]}}}"#,
        );
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        mcp.install("code-repo-wiki", &["rw".to_string(), "mcp".to_string()])
            .unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcp"]["other"].is_object());
        assert!(parsed["mcp"]["code-repo-wiki"].is_object());
    }

    #[test]
    fn opencode_install_rejects_non_object_top_level() {
        let dir = temp_dir("oc-malformed");
        let path = dir.join("opencode.json");
        write(&path, "[1, 2, 3]");
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        assert!(mcp.install("code-repo-wiki", &["x".to_string()]).is_err());
    }

    /// v52 T08a：配置文件读失败（权限/占用/损坏）必须显式中止——
    /// 静默当空对象会让后续写回覆盖含 OAuth 会话的用户配置
    #[test]
    fn opencode_install_aborts_on_unreadable_config() {
        // config_path 指向目录：read_to_string 对目录必然 Err（Windows 权限模拟不可靠）
        let dir = std::env::temp_dir().join(format!("rw_mcp_unreadable_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = OpencodeMcp {
            config_path: dir.clone(),
        };
        let err = cfg
            .install("test-server", &["echo".to_string(), "hi".to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("已中止写回"),
            "应显式中止并说明原因: {err}"
        );
        assert!(dir.is_dir(), "目录本身不应被写坏");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v52 T08a：.claude.json 读失败（权限/占用/损坏）必须显式中止——
    /// 静默当空对象会让后续写回覆盖含 OAuth 会话的用户配置（与
    /// opencode_install_aborts_on_unreadable_config 同语义）
    #[test]
    fn claude_install_aborts_on_unreadable_config() {
        // path 指向目录：read_to_string 对目录必然 Err（Windows 权限模拟不可靠）
        let dir = std::env::temp_dir().join(format!("rw_claude_unreadable_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mcp = ClaudeMcp { path: dir.clone() };
        let err = mcp
            .install("test-server", "echo", &["hi".to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("已中止写回"),
            "应显式中止并说明原因: {err}"
        );
        assert!(dir.is_dir(), "目录本身不应被写坏");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_remove_idempotent_and_cleanup() {
        let dir = temp_dir("oc-remove");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 文件缺失 → false
        mcp.install("code-repo-wiki", &["rw".to_string()]).unwrap();
        assert!(mcp.remove("code-repo-wiki").unwrap());
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 已删 → false
        // mcp 块已空 → 整个 mcp 键删除
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed.get("mcp").is_none());
    }

    #[test]
    fn opencode_remove_preserves_other_servers() {
        let dir = temp_dir("oc-remove-preserve");
        let path = dir.join("opencode.json");
        write(
            &path,
            r#"{"mcp": {"code-repo-wiki": {"type": "local", "command": ["rw"]}, "other": {"type": "local", "command": ["npx", "y"]}}, "provider": {"x": 1}}"#,
        );
        let mcp = OpencodeMcp {
            config_path: path.clone(),
        };
        assert!(mcp.remove("code-repo-wiki").unwrap());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcp"]["other"].is_object());
        assert!(parsed.get("mcp").unwrap().get("code-repo-wiki").is_none());
        assert_eq!(parsed["provider"]["x"], 1);
    }

    // ---- ClaudeMcp ----

    #[test]
    fn claude_install_creates_servers_block() {
        let dir = temp_dir("cl-create");
        let path = dir.join("claude.json");
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(
            mcp.install("code-repo-wiki", "rw", &["mcp".to_string()])
                .unwrap()
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &parsed["mcpServers"]["code-repo-wiki"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "rw");
        assert_eq!(entry["args"][0], "mcp");
    }

    #[test]
    fn claude_install_preserves_other_servers_and_idempotent() {
        let dir = temp_dir("cl-preserve");
        let path = dir.join("claude.json");
        write(
            &path,
            r#"{"oauthAccount": {}, "mcpServers": {"other": {"command": "npx", "args": ["x"]}}}"#,
        );
        let mcp = ClaudeMcp { path: path.clone() };
        let cmd = ("rw", vec!["mcp".to_string()]);
        assert!(mcp.install("code-repo-wiki", cmd.0, &cmd.1).unwrap());
        assert!(!mcp.install("code-repo-wiki", cmd.0, &cmd.1).unwrap());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            parsed["oauthAccount"].is_object(),
            "OAuth 会话等用户键必须保留"
        );
        assert!(parsed["mcpServers"]["other"].is_object());
        assert!(parsed["mcpServers"]["code-repo-wiki"].is_object());
    }

    #[test]
    fn claude_remove_keeps_file_with_empty_servers() {
        let dir = temp_dir("cl-remove");
        let path = dir.join("claude.json");
        let mcp = ClaudeMcp { path: path.clone() };
        mcp.install("code-repo-wiki", "rw", &[]).unwrap();
        assert!(mcp.remove("code-repo-wiki").unwrap());
        // ~/.claude.json 承载 OAuth 会话等用户配置：空 mcpServers 保留文件
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"].as_object().unwrap().is_empty());
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 条目缺失 → 幂等
    }

    #[test]
    fn claude_remove_preserves_other_servers() {
        let dir = temp_dir("cl-remove-preserve");
        let path = dir.join("claude.json");
        write(
            &path,
            r#"{"mcpServers": {"code-repo-wiki": {"command": "rw"}, "other": {"command": "npx"}}}"#,
        );
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(mcp.remove("code-repo-wiki").unwrap());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["other"].is_object());
        assert!(parsed["mcpServers"].get("code-repo-wiki").is_none());
    }

    #[test]
    fn claude_remove_rejects_malformed_json() {
        let dir = temp_dir("cl-malformed");
        let path = dir.join("claude.json");
        write(&path, "{not json");
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(mcp.remove("code-repo-wiki").is_err());
    }

    // ---- CodexMcp ----

    #[test]
    fn codex_install_creates_table_and_roundtrips() {
        let dir = temp_dir("cx-create");
        let path = dir.join("config.toml");
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        let exe = r"C:\RustProjects\code-repo-wiki\target\release\code-repo-wiki.exe";
        assert!(
            mcp.install("code-repo-wiki", exe, &["mcp".to_string()])
                .unwrap()
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[mcp_servers.code-repo-wiki]"));
        assert!(content.contains("command = \"C:\\\\RustProjects\\\\code-repo-wiki"));
        // toml 解析回读验证
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["code-repo-wiki"]["command"].as_str(),
            Some(exe)
        );
        assert_eq!(
            parsed["mcp_servers"]["code-repo-wiki"]["args"][0].as_str(),
            Some("mcp")
        );
    }

    #[test]
    fn codex_install_preserves_other_tables_and_comments() {
        let dir = temp_dir("cx-preserve");
        let path = dir.join("config.toml");
        write(
            &path,
            "# 我的注释\n[model]\nname = \"gpt-5\"\n\n[provider.openai]\nkey = \"x\"\n",
        );
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        mcp.install("code-repo-wiki", "rw", &["mcp".to_string()])
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# 我的注释"));
        assert!(content.contains("[model]"));
        assert!(content.contains("[provider.openai]"));
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert!(parsed["mcp_servers"]["code-repo-wiki"].is_table());
        assert_eq!(parsed["model"]["name"].as_str(), Some("gpt-5"));
    }

    #[test]
    fn codex_install_upgrades_existing_table() {
        let dir = temp_dir("cx-upgrade");
        let path = dir.join("config.toml");
        write(
            &path,
            "[mcp_servers.code-repo-wiki]\ncommand = \"/old/rw\"\nargs = [\"mcp\"]\n",
        );
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        assert!(
            mcp.install("code-repo-wiki", "/new/rw", &["mcp".to_string()])
                .unwrap()
        );
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["code-repo-wiki"]["command"].as_str(),
            Some("/new/rw")
        );
    }

    #[test]
    fn codex_install_idempotent() {
        let dir = temp_dir("cx-idem");
        let path = dir.join("config.toml");
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        assert!(
            mcp.install("code-repo-wiki", "/rw", &["mcp".to_string()])
                .unwrap()
        );
        assert!(
            !mcp.install("code-repo-wiki", "/rw", &["mcp".to_string()])
                .unwrap()
        );
    }

    #[test]
    fn codex_remove_table_preserves_rest() {
        let dir = temp_dir("cx-remove");
        let path = dir.join("config.toml");
        write(
            &path,
            "[model]\nname = \"gpt-5\"\n\n[mcp_servers.code-repo-wiki]\ncommand = \"/rw\"\n\n[provider.openai]\nkey = \"x\"\n",
        );
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        assert!(mcp.remove("code-repo-wiki").unwrap());
        assert!(!mcp.remove("code-repo-wiki").unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("mcp_servers"));
        assert!(content.contains("[model]"));
        assert!(content.contains("[provider.openai]"));
        // 移除后仍可被 toml 解析（无残留双空行/孤儿行）
        toml::from_str::<toml::Value>(&content).unwrap();
    }

    #[test]
    fn codex_remove_missing_is_idempotent() {
        let dir = temp_dir("cx-remove-miss");
        let path = dir.join("config.toml");
        write(&path, "[model]\nname = \"x\"\n");
        let mcp = CodexMcp {
            config_path: path.clone(),
        };
        assert!(!mcp.remove("code-repo-wiki").unwrap());
    }

    // ---- DshMcp（DeepSeek Harness，项目根 cordis.patch.yml） ----

    /// 全新安装：创建 cordis.patch.yml，含管理标记 + `- insert:` 一行插件
    /// 定义，command 为注入 exe 绝对路径（Windows 反斜杠转义）
    #[test]
    fn dsh_install_creates_block_and_roundtrips() {
        let dir = temp_dir("dsh-create");
        let path = dir.join("cordis.patch.yml");
        let mcp = DshMcp { path: path.clone() };
        let exe = r"C:\RustProjects\repo-wiki\target\release\code-repo-wiki.exe";
        assert!(mcp.install(exe).unwrap(), "首次安装应实际写入");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(DSH_BLOCK_START), "应含管理块开始标记");
        assert!(content.contains(DSH_BLOCK_END), "应含管理块结束标记");
        assert!(content.contains("- insert:"), "应含 insert patch 操作");
        assert!(
            content.contains("name: '@deepseek-ai/dsh-mcp-client'"),
            "应注册官方 mcp-client 插件"
        );
        assert!(
            content.contains("serverName: code-repo-wiki"),
            "serverName 应为 code-repo-wiki"
        );
        assert!(content.contains("transport: stdio"), "应为 stdio 传输");
        assert!(
            content.contains("command: \"C:\\\\RustProjects\\\\repo-wiki"),
            "command 应注入转义后的 exe 绝对路径"
        );
        assert!(content.contains("args: [mcp]"), "args 应为 [mcp]");
        assert!(
            content.contains("cwd: !!js process.cwd()"),
            "cwd 应为 dsh 工作区（官方示例同款）"
        );
    }

    /// 幂等：重复安装内容一致 → 返回 false 且文件不被触碰
    #[test]
    fn dsh_install_idempotent_skips_unchanged() {
        let dir = temp_dir("dsh-idem");
        let path = dir.join("cordis.patch.yml");
        let mcp = DshMcp { path: path.clone() };
        assert!(mcp.install("/usr/bin/code-repo-wiki").unwrap());
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(!mcp.install("/usr/bin/code-repo-wiki").unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "内容一致时不得重写");
    }

    /// 升级：exe 路径变化 → 管理块整体替换（command 更新）
    #[test]
    fn dsh_install_upgrades_changed_command() {
        let dir = temp_dir("dsh-upgrade");
        let path = dir.join("cordis.patch.yml");
        let mcp = DshMcp { path: path.clone() };
        assert!(mcp.install("/old/rw").unwrap());
        assert!(mcp.install("/new/rw").unwrap(), "命令变化应触发更新");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("command: \"/new/rw\""), "command 应更新");
        assert!(!content.contains("/old/rw"), "旧 command 不应残留");
        assert_eq!(
            content.matches(DSH_BLOCK_START).count(),
            1,
            "升级后只应有一个管理块"
        );
    }

    /// 保留无关内容：既有 insert 块/注释在追加后原样保留（文本级编辑契约）
    #[test]
    fn dsh_install_preserves_other_patches() {
        let dir = temp_dir("dsh-preserve");
        let path = dir.join("cordis.patch.yml");
        write(
            &path,
            "# 用户注释\n- insert:\n    - id: memory-my-server\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: my-memory\n        transport: stdio\n        command: my-memory-mcp\n",
        );
        let mcp = DshMcp { path: path.clone() };
        mcp.install("/usr/bin/code-repo-wiki").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# 用户注释"), "用户注释应保留");
        assert!(content.contains("id: memory-my-server"), "其他 insert 行应保留");
        assert!(content.contains("id: code-repo-wiki"), "本工具行应追加");
    }

    /// 卸载：管理块整块删除并清理残留空行；二次卸载幂等；无关内容保留
    #[test]
    fn dsh_remove_block_cleans_and_is_idempotent() {
        let dir = temp_dir("dsh-remove");
        let path = dir.join("cordis.patch.yml");
        write(&path, "# 用户注释\n- insert:\n    - id: other\n      name: x\n");
        let mcp = DshMcp { path: path.clone() };
        mcp.install("/usr/bin/code-repo-wiki").unwrap();
        assert!(mcp.remove().unwrap(), "首次卸载应实际移除");
        assert!(!mcp.remove().unwrap(), "二次卸载应幂等返回 false");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains(DSH_BLOCK_START), "管理块标记应被删除");
        assert!(!content.contains("id: code-repo-wiki"), "本工具行应被删除");
        assert!(content.contains("# 用户注释"), "用户内容应保留");
        assert!(content.contains("id: other"), "其他 insert 行应保留");
        assert!(
            !content.contains("\n\n\n"),
            "块删除后不得残留连续空行"
        );
    }

    /// 卸载：文件缺失 / 无本工具记录 → 幂等 false
    #[test]
    fn dsh_remove_missing_is_idempotent() {
        let dir = temp_dir("dsh-remove-miss");
        let path = dir.join("cordis.patch.yml");
        let mcp = DshMcp { path: path.clone() };
        assert!(!mcp.remove().unwrap(), "文件缺失应幂等返回 false");
        write(&path, "- insert:\n    - id: other\n      name: x\n");
        assert!(!mcp.remove().unwrap(), "无本工具记录应幂等返回 false");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("id: other"), "无关内容不得被触碰");
    }

    /// 卸载：文件为空（本工具独占内容，无用户 patch）→ 删除文件本身
    /// （空 cordis.patch.yml 在 YAML 中解析为 null，dsh patch 加载器预期
    /// 操作列表，删文件比留空文件更稳妥）
    #[test]
    fn dsh_remove_deletes_empty_file() {
        let dir = temp_dir("dsh-remove-empty");
        let path = dir.join("cordis.patch.yml");
        let mcp = DshMcp { path: path.clone() };
        mcp.install("/usr/bin/code-repo-wiki").unwrap();
        assert!(path.exists(), "安装后文件应存在");
        assert!(mcp.remove().unwrap(), "卸载应实际移除");
        assert!(!path.exists(), "文件应为空时被删除");
    }

    /// 用户手工合入裸行（无管理标记）：命令一致 → 重复安装跳过；
    /// 命令不一致 → 裸行升级为管理块
    #[test]
    fn dsh_install_handles_legacy_bare_row() {
        let dir = temp_dir("dsh-legacy");
        let path = dir.join("cordis.patch.yml");
        let bare = "- insert:\n    - id: code-repo-wiki\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: code-repo-wiki\n        transport: stdio\n        command: \"/old/rw\"\n        args: [mcp]\n        cwd: !!js process.cwd()\n"
            .to_string();
        write(&path, &bare);
        let mcp = DshMcp { path: path.clone() };
        assert!(
            mcp.install("/new/rw").unwrap(),
            "命令不一致应升级为管理块"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(DSH_BLOCK_START), "应升级为受管块");
        assert!(content.contains("command: \"/new/rw\""), "command 应更新");
        assert_eq!(
            content.matches("id: code-repo-wiki").count(),
            1,
            "不应产生重复 id（dsh 同名 server 加载冲突）"
        );

        // 命令一致场景：手工合入的裸行已指向当前 exe → 跳过
        let dir2 = temp_dir("dsh-legacy-match");
        let path2 = dir2.join("cordis.patch.yml");
        write(&path2, &bare);
        let mcp2 = DshMcp { path: path2.clone() };
        assert!(!mcp2.install("/old/rw").unwrap(), "命令一致应跳过");
        let content2 = std::fs::read_to_string(&path2).unwrap();
        assert_eq!(content2, bare, "跳过时文件不得被改动");
    }

    /// 卸载：用户手工合入的裸行（无管理标记）同样被移除，其他内容保留
    #[test]
    fn dsh_remove_legacy_bare_row() {
        let dir = temp_dir("dsh-remove-legacy");
        let path = dir.join("cordis.patch.yml");
        write(
            &path,
            "- insert:\n    - id: code-repo-wiki\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: code-repo-wiki\n        transport: stdio\n        command: /rw\n    - id: other\n      name: x\n",
        );
        let mcp = DshMcp { path: path.clone() };
        assert!(mcp.remove().unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("id: code-repo-wiki"), "裸行应被移除");
        assert!(content.contains("id: other"), "其他行应保留");
    }
}
