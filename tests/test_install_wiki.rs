//! AGENTS.md 注入/移除集成测试（G1：v33 起并入 install/uninstall 主命令）
//!
//! 通过 env!("CARGO_BIN_EXE_code-repo-wiki") 调用真实二进制，覆盖：
//! 1. install 默认注入 AGENTS.md 标记对（v33 拍板：默认执行）
//! 2. 重复 install 幂等（用户内容保留）
//! 3. uninstall --force 移除标记块、保留用户内容；未安装时退出码 0
//! 4. --also-claude 双写 AGENTS.md + CLAUDE.md；uninstall 同时清理
//! 5. 半标记 AGENTS.md → 非 0 退出码且文件不被修改
//!
//! v33 起 install 会写用户级 opencode.json（MCP 注册），必须隔离
//! HOME/USERPROFILE（Windows 下 config_dir 依赖 USERPROFILE）。

use std::path::PathBuf;

mod common;
use common::{run_bin_with_envs, unique_dir};

const START: &str = "<!-- CODE-REPO-WIKI:START -->";
const END: &str = "<!-- CODE-REPO-WIKI:END -->";
const LEGACY_START: &str = "<!-- REPO-WIKI:START -->";

/// 创建隔离临时目录 + 隔离 HOME，返回 (work_dir, home, envs)
fn setup(tag: &str) -> (PathBuf, PathBuf, Vec<(&'static str, String)>) {
    let dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let home = unique_dir(&format!("{tag}_home"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs = vec![
        ("HOME", home.to_string_lossy().into_owned()),
        ("USERPROFILE", home.to_string_lossy().into_owned()),
    ];
    (dir, home, envs)
}

// ==================== 测试用例 ====================

/// install：默认创建 AGENTS.md 并注入含标记对的 wiki 引用块
#[test]
fn test_install_wiki_creates_agents_md() {
    let (work_dir, _home, envs) = setup("creates_agents_md");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = work_dir.join("AGENTS.md");
    assert!(path.exists(), "AGENTS.md 应被创建: {}", path.display());
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(START), "应含 START 标记，实际: {content}");
    assert!(content.contains(END), "应含 END 标记，实际: {content}");
    assert!(content.contains("code-repo-wiki update"), "应含 code-repo-wiki update 指引，实际: {content}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// install 幂等：重复执行内容不变，且用户已有内容保留
#[test]
fn test_install_wiki_idempotent_preserves_user_content() {
    let (work_dir, _home, envs) = setup("idempotent");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = work_dir.join("AGENTS.md");
    std::fs::write(&path, "# 用户项目说明\n").unwrap();

    let out1 = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out1.status.success(), "首次 install 应成功");
    let content1 = std::fs::read_to_string(&path).unwrap();

    let out2 = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out2.status.success(), "重复 install 应成功");
    let content2 = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content1, content2, "重复 install 应幂等（内容不变）");
    assert!(content2.starts_with("# 用户项目说明\n"), "用户内容应保留在块之前，实际: {content2}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// uninstall --force：新旧两代标记块都移除，用户内容保留；未安装时退出码 0
/// （双块=改名升级后的残留场景：v37 前的旧块 + 重装的新块并存）
#[test]
fn test_uninstall_wiki_removes_block() {
    let (work_dir, _home, envs) = setup("uninstall");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = work_dir.join("AGENTS.md");
    std::fs::write(
        &path,
        "用户头部\n\n<!-- REPO-WIKI:START -->\n旧名块内容\n<!-- REPO-WIKI:END -->\n\n<!-- CODE-REPO-WIKI:START -->\n块内容\n<!-- CODE-REPO-WIKI:END -->\n用户尾部\n",
    )
    .unwrap();

    let out = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(
        out.status.success(),
        "uninstall 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains(START) && !content.contains(END) && !content.contains(LEGACY_START),
        "新旧两代标记都应被移除，实际: {content}"
    );
    assert!(
        !content.contains("块内容") && !content.contains("旧名块内容"),
        "两代标记块内容都应被移除，实际: {content}"
    );
    assert!(content.contains("用户头部") && content.contains("用户尾部"), "用户内容应保留，实际: {content}");

    // 再次卸载：未安装 → 退出码 0 且提示
    let out2 = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(out2.status.success(), "未安装时卸载应退出码 0");
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout.contains("未安装"), "应提示未安装，实际: {stdout}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// install：AGENTS.md 含 v37 旧标记块 → 整体迁移替换为新标记块（不残留旧块）
#[test]
fn test_install_wiki_migrates_legacy_block() {
    let (work_dir, _home, envs) = setup("migrate_legacy");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = work_dir.join("AGENTS.md");
    std::fs::write(
        &path,
        "用户头部\n\n<!-- REPO-WIKI:START -->\n旧名块内容\n<!-- REPO-WIKI:END -->\n用户尾部\n",
    )
    .unwrap();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(START) && content.contains(END), "应注入新标记块，实际: {content}");
    assert!(!content.contains(LEGACY_START), "旧标记块应被迁移替换，实际: {content}");
    assert!(!content.contains("旧名块内容"), "旧块内容应被替换，实际: {content}");
    assert!(content.contains("用户头部") && content.contains("用户尾部"), "用户内容应保留: {content}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --claude（v36 起合并 --also-claude）：AGENTS.md 与 CLAUDE.md 都写入
/// 标记对（同时注册用户级 ~/.claude.json MCP——v39：不再写项目根 .mcp.json）；
/// uninstall 同时清理
#[test]
fn test_install_wiki_also_claude_writes_both() {
    let (work_dir, home, envs) = setup("also_claude");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install", "--claude"], &envs_ref);
    assert!(out.status.success(), "install --claude 应成功");
    let agents = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    let claude = std::fs::read_to_string(work_dir.join("CLAUDE.md")).unwrap();
    assert!(agents.contains(START) && agents.contains(END), "AGENTS.md 应含标记对");
    assert!(claude.contains(START) && claude.contains(END), "CLAUDE.md 应含标记对");
    assert_eq!(agents, claude, "CLAUDE.md 应原样写入与 AGENTS.md 相同的注入块");
    let claude_json = home.join(".claude.json");
    assert!(claude_json.exists(), "--claude 应注册用户级 ~/.claude.json，实际路径: {}", claude_json.display());
    let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert!(parsed["mcpServers"]["code-repo-wiki"].is_object(), "~/.claude.json 应含 code-repo-wiki MCP 条目");
    assert!(!work_dir.join(".mcp.json").exists(), "v39 起不再写项目根 .mcp.json");

    let out2 = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(out2.status.success(), "uninstall 应成功");
    let agents2 = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    let claude2 = std::fs::read_to_string(work_dir.join("CLAUDE.md")).unwrap();
    assert!(!agents2.contains(START), "AGENTS.md 标记应被移除");
    assert!(!claude2.contains(START), "CLAUDE.md 标记应被移除");
    let parsed2: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert!(parsed2["mcpServers"].get("code-repo-wiki").is_none(), "uninstall 应移除 ~/.claude.json MCP 条目");

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 半标记：只含 START 的 AGENTS.md → install/uninstall 均非 0 退出码且文件不被修改
#[test]
fn test_half_marker_errors_and_preserves_file() {
    let (work_dir, _home, envs) = setup("half_marker");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = work_dir.join("AGENTS.md");
    let broken = "# 标题\n<!-- REPO-WIKI:START -->\n";
    std::fs::write(&path, broken).unwrap();

    for cmd in ["install", "uninstall"] {
        let args: Vec<&str> = if cmd == "install" {
            vec!["install"]
        } else {
            vec!["uninstall", "--force"]
        };
        let out = run_bin_with_envs(&work_dir, &args, &envs_ref);
        assert!(!out.status.success(), "{cmd} 遇半标记应非 0 退出码");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(combined.contains("不完整"), "{cmd} 应报半标记错误，实际: {combined}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "{cmd} 失败时不应修改文件"
        );
    }

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// U02 回归：注入块按目标仓库配置渲染产物路径（v30 后 output.dir 硬编码
/// .code-repo-wiki，仅 wiki.language 参与渲染）
#[test]
fn test_install_wiki_template_uses_config_paths() {
    let (work_dir, _home, envs) = setup("template_config");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    std::fs::write(work_dir.join("config.toml"), "[wiki]\nlanguage = \"en\"\n").unwrap();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    assert!(
        content.contains("`.code-repo-wiki/wiki/en/overview.md`"),
        "应渲染 .code-repo-wiki/wiki/en/overview.md, 实际: {content}"
    );
    assert!(
        !content.contains(".code-repo-wiki/wiki/zh"),
        "不应残留默认 zh 路径, 实际: {content}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// U02 回归：无配置时按默认产物路径渲染并提示
#[test]
fn test_install_wiki_template_defaults_without_config() {
    let (work_dir, _home, envs) = setup("template_default");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("默认"), "应提示按默认值渲染，实际: {combined}");
    let content = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    assert!(
        content.contains("`.code-repo-wiki/wiki/zh/overview.md`"),
        "默认应渲染 .code-repo-wiki/zh 路径，实际: {content}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
