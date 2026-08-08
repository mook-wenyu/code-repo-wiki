//! 多 Agent MCP 配置读写模块（v33）
//!
//! 合并 install 的多 Agent 支持：`install` 默认注册 opencode（用户级全局
//! `~/.config/opencode/opencode.json` 的 `mcp` 块），`--claude` 额外写项目根
//! `.mcp.json`（Claude Code 格式 `mcpServers`），`--codex` 额外写用户级
//! `~/.codex/config.toml` 的 `[mcp_servers.<name>]` 表。
//!
//! 三个 writer（`OpencodeMcp`/`ClaudeMcp`/`CodexMcp`）各自封装一种
//! 格式，共同契约：
//! - `install(server)`：条目已存在且命令一致 → 返回 `false`（跳过）；
//!   存在但命令不同（升级，如二进制路径变化）→ 更新返回 `true`；
//!   不存在 → 新建返回 `true`
//! - `remove(server)`：文件缺失/无条目 → 幂等返回 `false`；移除后空容器
//!   的清理语义各格式不同（opencode 删 `mcp` 键、Claude 空则删整个文件、
//!   Codex 删表）
//! - 只动本 server 的条目，绝不触碰其他 server/配置键（多 Agent 共存）
//! - 畸形文件（JSON 非对象/TOML 解析失败）→ 显式报错，拒绝静默处理
//!   （契约与 v32 commands::remove_mcp_config 一致：损坏的配置文件不能被静默吞掉）

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::fs::write_file_atomic;
use crate::project::ProjectRoot;

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
        Ok(user_home()?.join(".config").join("opencode").join("opencode.json"))
    }

    /// 注册/更新 MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 视为空 `{}` 新建（opencode 支持全空配置）
    /// - `mcp` 键缺失 → 新建；已有其他 server → 保留（多 server 共存）
    /// - 本 server 已存在且 command 一致 → `false`（幂等跳过）
    /// - 本 server 已存在但 command 不同 → 整体替换该条目（升级）
    /// - 顶层非 JSON 对象 → 显式报错（N12 规则：数组/标量是损坏配置）
    pub fn install(&self, server: &str, command: &[String]) -> Result<bool> {
        let content = std::fs::read_to_string(&self.config_path)
            .unwrap_or_else(|_| "{}".to_string());
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
            .with_context(|| format!("opencode.json 的 mcp 键应为对象: {}", self.config_path.display()))?
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
// Claude Code（项目根 `.mcp.json` 的 `mcpServers`）
// ---------------------------------------------------------------------------

/// Claude Code MCP 配置读写（项目根 `.mcp.json`）
///
/// Claude Code 官方格式（code.claude.com/docs/en/mcp）：顶层 `mcpServers` →
/// 服务器名 → `{ "type": "stdio", "command": "<string>", "args": [...] }`。
/// 与 opencode 格式不兼容（键名/command 形态/env 变量语法均不同），
/// 故独立 writer。文件是人工可编辑配置：只动本 server 条目，其他 server
/// 与人工修改保留。
pub struct ClaudeMcp {
    /// 项目根 `.mcp.json` 路径
    pub path: PathBuf,
}

impl ClaudeMcp {
    /// 项目根 `.mcp.json` 路径
    pub fn project_path(root: &ProjectRoot) -> PathBuf {
        root.path().join(".mcp.json")
    }

    /// 注册/更新 MCP server；返回是否实际变更
    ///
    /// - 文件缺失 → 新建（含 mcpServers 容器）
    /// - 本 server 存在且 command+args 一致 → `false`
    /// - 本 server 存在但命令不同 → 更新（升级）
    /// - 其他 server 条目原样保留
    pub fn install(
        &self,
        server: &str,
        command: &str,
        args: &[String],
    ) -> Result<bool> {
        let content = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut value: serde_json::Value = if content.trim().is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            serde_json::from_str(&content)
                .with_context(|| format!("解析 .mcp.json 失败: {}", self.path.display()))?
        };
        if !value.is_object() {
            anyhow::bail!(".mcp.json 顶层应为 JSON 对象: {}", self.path.display());
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
                            a.iter().filter_map(|v| v.as_str()).eq(args.iter().map(String::as_str))
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
            .with_context(|| format!(".mcp.json 的 mcpServers 键应为对象: {}", self.path.display()))?
            .insert(server.to_string(), entry);

        let output = serde_json::to_string_pretty(&value).context("序列化 .mcp.json 失败")?;
        write_file_atomic(&self.path, &output)?;
        Ok(true)
    }

    /// 移除 MCP server；返回是否实际移除（语义与 v32 commands::remove_mcp_config
    /// 相同，迁入本模块统一管理）
    ///
    /// - 文件缺失 → `false`（幂等）
    /// - 移除后 mcpServers 为空 → 删整个文件（文件失去价值）
    /// - 移除后仍有其他 server → 原子写回保留
    /// - JSON 解析失败 → 显式报错（拒绝静默清理损坏配置）
    pub fn remove(&self, server: &str) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("读取 .mcp.json 失败: {}", self.path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析 .mcp.json 失败（拒绝静默跳过）: {}", self.path.display()))?;

        let removed = {
            let servers = value.get_mut("mcpServers").and_then(|v| v.as_object_mut());
            match servers {
                None => false,
                Some(s) => {
                    let hit = s.remove(server).is_some();
                    if hit && s.is_empty() {
                        // mcpServers 已无任何 server → 整个文件失去价值，删除
                        std::fs::remove_file(&self.path)?;
                        return Ok(true);
                    }
                    hit
                }
            }
        };
        if !removed {
            return Ok(false);
        }
        write_file_atomic(&self.path, &serde_json::to_string_pretty(&value)?)?;
        Ok(true)
    }}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 独立临时目录（防并行测试冲突），返回目录路径
    fn temp_dir(tag: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("code-repo-wiki-mcp-test-{tag}-{}-{id}", std::process::id()));
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
        let mcp = OpencodeMcp { config_path: path.clone() };
        let cmd = vec!["/usr/bin/code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &cmd).unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
        let mcp = OpencodeMcp { config_path: path.clone() };
        let cmd = vec!["code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &cmd).unwrap());
        assert!(!mcp.install("code-repo-wiki", &cmd).unwrap());
    }

    #[test]
    fn opencode_install_upgrades_changed_command() {
        let dir = temp_dir("oc-upgrade");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp { config_path: path.clone() };
        let old = vec!["/old/code-repo-wiki".to_string(), "mcp".to_string()];
        let new = vec!["/new/code-repo-wiki".to_string(), "mcp".to_string()];
        assert!(mcp.install("code-repo-wiki", &old).unwrap());
        assert!(mcp.install("code-repo-wiki", &new).unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcp"]["code-repo-wiki"]["command"][0], "/new/code-repo-wiki");
    }

    #[test]
    fn opencode_install_preserves_other_servers() {
        let dir = temp_dir("oc-preserve");
        let path = dir.join("opencode.json");
        write(&path, r#"{"mcp": {"other": {"type": "local", "command": ["npx", "x"]}}}"#);
        let mcp = OpencodeMcp { config_path: path.clone() };
        mcp.install("code-repo-wiki", &["rw".to_string(), "mcp".to_string()]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcp"]["other"].is_object());
        assert!(parsed["mcp"]["code-repo-wiki"].is_object());
    }

    #[test]
    fn opencode_install_rejects_non_object_top_level() {
        let dir = temp_dir("oc-malformed");
        let path = dir.join("opencode.json");
        write(&path, "[1, 2, 3]");
        let mcp = OpencodeMcp { config_path: path.clone() };
        assert!(mcp.install("code-repo-wiki", &["x".to_string()]).is_err());
    }

    #[test]
    fn opencode_remove_idempotent_and_cleanup() {
        let dir = temp_dir("oc-remove");
        let path = dir.join("opencode.json");
        let mcp = OpencodeMcp { config_path: path.clone() };
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 文件缺失 → false
        mcp.install("code-repo-wiki", &["rw".to_string()]).unwrap();
        assert!(mcp.remove("code-repo-wiki").unwrap());
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 已删 → false
        // mcp 块已空 → 整个 mcp 键删除
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed.get("mcp").is_none());
    }

    #[test]
    fn opencode_remove_preserves_other_servers() {
        let dir = temp_dir("oc-remove-preserve");
        let path = dir.join("opencode.json");
        write(&path, r#"{"mcp": {"code-repo-wiki": {"type": "local", "command": ["rw"]}, "other": {"type": "local", "command": ["npx", "y"]}}, "provider": {"x": 1}}"#);
        let mcp = OpencodeMcp { config_path: path.clone() };
        assert!(mcp.remove("code-repo-wiki").unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcp"]["other"].is_object());
        assert!(parsed.get("mcp").unwrap().get("code-repo-wiki").is_none());
        assert_eq!(parsed["provider"]["x"], 1);
    }

    // ---- ClaudeMcp ----

    #[test]
    fn claude_install_creates_servers_block() {
        let dir = temp_dir("cl-create");
        let path = dir.join(".mcp.json");
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(mcp.install("code-repo-wiki", "rw", &["mcp".to_string()]).unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &parsed["mcpServers"]["code-repo-wiki"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "rw");
        assert_eq!(entry["args"][0], "mcp");
    }

    #[test]
    fn claude_install_preserves_other_servers_and_idempotent() {
        let dir = temp_dir("cl-preserve");
        let path = dir.join(".mcp.json");
        write(&path, r#"{"mcpServers": {"other": {"command": "npx", "args": ["x"]}}}"#);
        let mcp = ClaudeMcp { path: path.clone() };
        let cmd = ("rw", vec!["mcp".to_string()]);
        assert!(mcp.install("code-repo-wiki", cmd.0, &cmd.1).unwrap());
        assert!(!mcp.install("code-repo-wiki", cmd.0, &cmd.1).unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["other"].is_object());
        assert!(parsed["mcpServers"]["code-repo-wiki"].is_object());
    }

    #[test]
    fn claude_remove_empty_deletes_file() {
        let dir = temp_dir("cl-remove");
        let path = dir.join(".mcp.json");
        let mcp = ClaudeMcp { path: path.clone() };
        mcp.install("code-repo-wiki", "rw", &[]).unwrap();
        assert!(mcp.remove("code-repo-wiki").unwrap());
        assert!(!path.exists()); // 空则删整个文件
        assert!(!mcp.remove("code-repo-wiki").unwrap()); // 文件缺失 → 幂等
    }

    #[test]
    fn claude_remove_preserves_other_servers() {
        let dir = temp_dir("cl-remove-preserve");
        let path = dir.join(".mcp.json");
        write(&path, r#"{"mcpServers": {"code-repo-wiki": {"command": "rw"}, "other": {"command": "npx"}}}"#);
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(mcp.remove("code-repo-wiki").unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["other"].is_object());
        assert!(parsed["mcpServers"].get("code-repo-wiki").is_none());
    }

    #[test]
    fn claude_remove_rejects_malformed_json() {
        let dir = temp_dir("cl-malformed");
        let path = dir.join(".mcp.json");
        write(&path, "{not json");
        let mcp = ClaudeMcp { path: path.clone() };
        assert!(mcp.remove("code-repo-wiki").is_err());
    }

    // ---- CodexMcp ----

    #[test]
    fn codex_install_creates_table_and_roundtrips() {
        let dir = temp_dir("cx-create");
        let path = dir.join("config.toml");
        let mcp = CodexMcp { config_path: path.clone() };
        let exe = r"C:\RustProjects\code-repo-wiki\target\release\code-repo-wiki.exe";
        assert!(mcp.install("code-repo-wiki", exe, &["mcp".to_string()]).unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[mcp_servers.code-repo-wiki]"));
        assert!(content.contains("command = \"C:\\\\RustProjects\\\\code-repo-wiki"));
        // toml 解析回读验证
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(parsed["mcp_servers"]["code-repo-wiki"]["command"].as_str(), Some(exe));
        assert_eq!(parsed["mcp_servers"]["code-repo-wiki"]["args"][0].as_str(), Some("mcp"));
    }

    #[test]
    fn codex_install_preserves_other_tables_and_comments() {
        let dir = temp_dir("cx-preserve");
        let path = dir.join("config.toml");
        write(&path, "# 我的注释\n[model]\nname = \"gpt-5\"\n\n[provider.openai]\nkey = \"x\"\n");
        let mcp = CodexMcp { config_path: path.clone() };
        mcp.install("code-repo-wiki", "rw", &["mcp".to_string()]).unwrap();
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
        write(&path, "[mcp_servers.code-repo-wiki]\ncommand = \"/old/rw\"\nargs = [\"mcp\"]\n");
        let mcp = CodexMcp { config_path: path.clone() };
        assert!(mcp.install("code-repo-wiki", "/new/rw", &["mcp".to_string()]).unwrap());
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcp_servers"]["code-repo-wiki"]["command"].as_str(), Some("/new/rw"));
    }

    #[test]
    fn codex_install_idempotent() {
        let dir = temp_dir("cx-idem");
        let path = dir.join("config.toml");
        let mcp = CodexMcp { config_path: path.clone() };
        assert!(mcp.install("code-repo-wiki", "/rw", &["mcp".to_string()]).unwrap());
        assert!(!mcp.install("code-repo-wiki", "/rw", &["mcp".to_string()]).unwrap());
    }

    #[test]
    fn codex_remove_table_preserves_rest() {
        let dir = temp_dir("cx-remove");
        let path = dir.join("config.toml");
        write(&path, "[model]\nname = \"gpt-5\"\n\n[mcp_servers.code-repo-wiki]\ncommand = \"/rw\"\n\n[provider.openai]\nkey = \"x\"\n");
        let mcp = CodexMcp { config_path: path.clone() };
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
        let mcp = CodexMcp { config_path: path.clone() };
        assert!(!mcp.remove("code-repo-wiki").unwrap());
    }
}
