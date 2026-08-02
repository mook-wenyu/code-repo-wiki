//! install-wiki / uninstall-wiki CLI 集成测试（G1: AGENTS.md 注入命令）
//!
//! 通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制，覆盖：
//! 1. install-wiki --root 创建 AGENTS.md 并注入标记对
//! 2. 重复 install-wiki 幂等（用户内容保留）
//! 3. uninstall-wiki 移除标记块、保留用户内容；未安装时退出码 0
//! 4. --also-claude 双写 AGENTS.md + CLAUDE.md；uninstall 同时清理
//! 5. 半标记 AGENTS.md → 非 0 退出码且文件不被修改
//!
//! 不触碰 opencode 全局配置（无 HOME/USERPROFILE 隔离需求），
//! 仅关闭 RUST_LOG 保证 stdout 只有业务输出（参照 test_install_opencode.rs）。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

const START: &str = "<!-- REPO-WIKI:START -->";
const END: &str = "<!-- REPO-WIKI:END -->";

/// 生成唯一临时目录（进程 id + 自增序号）
fn unique_dir(name: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("repo_wiki_wiki_install_{}_{}_{}", name, std::process::id(), seq))
}

/// 在指定目录下执行 repo-wiki 二进制，返回完整输出
fn run_bin(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_repo-wiki"));
    cmd.args(args)
        .current_dir(dir)
        .env("RUST_LOG", "off") // 关闭 tracing 日志，保证 stdout 只有业务输出
        .env_remove("OPENAI_API_KEY"); // 避免宿主机真实 Key 被误用
    cmd.output().expect("执行 repo-wiki 二进制失败")
}

/// 创建隔离临时目录并返回路径
fn setup(tag: &str) -> PathBuf {
    let dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ==================== 测试用例 ====================

/// install-wiki：创建 AGENTS.md 并注入含标记对的 wiki 引用块
#[test]
fn test_install_wiki_creates_agents_md() {
    let work_dir = setup("creates_agents_md");

    let out = run_bin(&work_dir, &["install-wiki", "--root", work_dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "install-wiki 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = work_dir.join("AGENTS.md");
    assert!(path.exists(), "AGENTS.md 应被创建: {}", path.display());
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(START), "应含 START 标记，实际: {content}");
    assert!(content.contains(END), "应含 END 标记，实际: {content}");
    assert!(content.contains("repo-wiki update"), "应含 repo-wiki update 指引，实际: {content}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// install-wiki 幂等：重复执行内容不变，且用户已有内容保留
#[test]
fn test_install_wiki_idempotent_preserves_user_content() {
    let work_dir = setup("idempotent");
    let path = work_dir.join("AGENTS.md");
    std::fs::write(&path, "# 用户项目说明\n").unwrap();

    let root = work_dir.to_str().unwrap();
    let out1 = run_bin(&work_dir, &["install-wiki", "--root", root]);
    assert!(out1.status.success(), "首次 install-wiki 应成功");
    let content1 = std::fs::read_to_string(&path).unwrap();

    let out2 = run_bin(&work_dir, &["install-wiki", "--root", root]);
    assert!(out2.status.success(), "重复 install-wiki 应成功");
    let content2 = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content1, content2, "重复 install 应幂等（内容不变）");
    assert!(content2.starts_with("# 用户项目说明\n"), "用户内容应保留在块之前，实际: {content2}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// uninstall-wiki：移除标记对及内容，用户内容保留；未安装时退出码 0
#[test]
fn test_uninstall_wiki_removes_block() {
    let work_dir = setup("uninstall");
    let path = work_dir.join("AGENTS.md");
    std::fs::write(&path, "用户头部\n\n<!-- REPO-WIKI:START -->\n块内容\n<!-- REPO-WIKI:END -->\n用户尾部\n").unwrap();

    let root = work_dir.to_str().unwrap();
    let out = run_bin(&work_dir, &["uninstall-wiki", "--root", root]);
    assert!(
        out.status.success(),
        "uninstall-wiki 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains(START) && !content.contains(END), "标记应被移除，实际: {content}");
    assert!(!content.contains("块内容"), "标记块内容应被移除，实际: {content}");
    assert!(content.contains("用户头部") && content.contains("用户尾部"), "用户内容应保留，实际: {content}");

    // 再次卸载：未安装 → 退出码 0 且提示
    let out2 = run_bin(&work_dir, &["uninstall-wiki", "--root", root]);
    assert!(out2.status.success(), "未安装时卸载应退出码 0");
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout.contains("未安装"), "应提示未安装，实际: {stdout}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --also-claude：AGENTS.md 与 CLAUDE.md 都写入标记对；uninstall 同时清理
#[test]
fn test_install_wiki_also_claude_writes_both() {
    let work_dir = setup("also_claude");
    let root = work_dir.to_str().unwrap();

    let out = run_bin(&work_dir, &["install-wiki", "--also-claude", "--root", root]);
    assert!(out.status.success(), "install-wiki --also-claude 应成功");
    let agents = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    let claude = std::fs::read_to_string(work_dir.join("CLAUDE.md")).unwrap();
    assert!(agents.contains(START) && agents.contains(END), "AGENTS.md 应含标记对");
    assert!(claude.contains(START) && claude.contains(END), "CLAUDE.md 应含标记对");
    assert_eq!(agents, claude, "CLAUDE.md 应原样写入与 AGENTS.md 相同的注入块");

    let out2 = run_bin(&work_dir, &["uninstall-wiki", "--root", root]);
    assert!(out2.status.success(), "uninstall-wiki 应成功");
    let agents2 = std::fs::read_to_string(work_dir.join("AGENTS.md")).unwrap();
    let claude2 = std::fs::read_to_string(work_dir.join("CLAUDE.md")).unwrap();
    assert!(!agents2.contains(START), "AGENTS.md 标记应被移除");
    assert!(!claude2.contains(START), "CLAUDE.md 标记应被移除");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// 半标记：只含 START 的 AGENTS.md → install/uninstall 均非 0 退出码且文件不被修改
#[test]
fn test_half_marker_errors_and_preserves_file() {
    let work_dir = setup("half_marker");
    let path = work_dir.join("AGENTS.md");
    let broken = "# 标题\n<!-- REPO-WIKI:START -->\n";
    std::fs::write(&path, broken).unwrap();
    let root = work_dir.to_str().unwrap();

    for cmd in ["install-wiki", "uninstall-wiki"] {
        let out = run_bin(&work_dir, &[cmd, "--root", root]);
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
