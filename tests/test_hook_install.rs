//! git hook 安装/卸载集成测试（票 04）
//!
//! 通过 env!("CARGO_BIN_EXE_code-repo-wiki") 调用真实二进制，覆盖：
//! 1. install 在 git 仓库中写入 post-commit/post-merge hook，
//!    内容含 `code-repo-wiki update` 且不含已废弃的 --quiet（clap 会报错被 || true 吞掉）
//! 2. hook 内容解析出的命令合法（防 --quiet 回归）
//! 3. 非 git 仓库 install 打印提示而非静默
//! 4. uninstall 只删除含 code-repo-wiki 标记的 hook（人工 hook 保留）
//!
//! 每个测试使用独立临时目录（进程 pid + 自增序号）避免并行冲突；
//! install/uninstall 会读写 opencode 配置，一律隔离 HOME/USERPROFILE
//! （参照 test_cli.rs 手法）。

use std::path::{Path, PathBuf};

mod common;
use common::{run_bin_with_envs, unique_dir};

/// 隔离 HOME/USERPROFILE（install/uninstall 会读写 opencode 全局配置）
fn home_envs(home: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("HOME", home.to_string_lossy().into_owned()),
        ("USERPROFILE", home.to_string_lossy().into_owned()),
        ("APPDATA", home.to_string_lossy().into_owned()),
    ]
}

/// 初始化 git 仓库（hooks 安装的前置条件），返回 .git/hooks 目录
fn init_git_repo(dir: &Path) -> PathBuf {
    let git = git2::Repository::init(dir).expect("git init 失败");
    let mut cfg = git.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    dir.join(".git").join("hooks")
}

/// 从 hook 内容解析出 code-repo-wiki 命令（取首个 `code-repo-wiki ...` 行，剥掉重定向与兜底尾缀）
fn parse_hook_command(content: &str) -> String {
    content
        .lines()
        .find(|l| l.starts_with("code-repo-wiki "))
        .expect("hook 应包含 code-repo-wiki 命令行")
        // v36 D2 起模板为 `code-repo-wiki update 2>>.code-repo-wiki/update-error.log || …`：
        // 命令在首个重定向标记前结束
        .split(" 2>>")
        .next()
        .unwrap()
        .trim()
        .to_string()
}

// ==================== 测试用例 ====================

/// install 在 git 仓库中写入 post-commit/post-merge hook：
/// 内容含 `code-repo-wiki update` 与 `command -v code-repo-wiki`，不含已废弃的 --quiet
#[test]
fn test_install_writes_git_hooks() {
    let work_dir = unique_dir("writes_hooks");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    let home = unique_dir("writes_hooks_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for hook_name in ["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        let content = std::fs::read_to_string(&hook_path)
            .unwrap_or_else(|e| panic!("{} hook 应写入 {}: {}", hook_name, hook_path.display(), e));
        assert!(
            content.contains("code-repo-wiki update"),
            "{hook_name} 应含 update 命令，实际: {content}"
        );
        assert!(
            !content.contains("--quiet"),
            "{hook_name} 不应含已废弃的 --quiet（clap 会报错被 || true 吞掉），实际: {content}"
        );
        assert!(
            content.contains("command -v code-repo-wiki"),
            "{hook_name} 应含 PATH 探测，实际: {content}"
        );
    }

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 从 hook 内容解析出的命令必须等于 `code-repo-wiki update`（防 --quiet 回归：
/// 若 install 写入带 --quiet 的旧模板，解析结果不匹配即失败）
#[test]
fn test_hook_command_succeeds() {
    let work_dir = unique_dir("cmd_valid");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    let home = unique_dir("cmd_valid_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");

    let content = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert_eq!(
        parse_hook_command(&content),
        "code-repo-wiki update",
        "hook 命令应为 code-repo-wiki update（无 --quiet），实际 hook:\n{content}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 非 git 仓库（无 .git/hooks）：install 成功退出且 stdout 打印提示而非静默
#[test]
fn test_install_non_git_repo_prints_hint() {
    let work_dir = unique_dir("no_git");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let home = unique_dir("no_git_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "非 git 仓库 install 应成功退出，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("未检测到 .git 目录"),
        "应打印跳过提示，实际 stdout: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// uninstall --force 只删除含 code-repo-wiki 标记的 hook：
/// post-commit（标记）被删，人工 post-merge（无标记）保留
#[test]
fn test_uninstall_removes_only_own_hooks() {
    let work_dir = unique_dir("rm_own");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    // 预置两个 hook：post-commit 带 code-repo-wiki 标记，post-merge 为人工内容
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\n# code-repo-wiki: auto-update wiki on commit\ncode-repo-wiki update 2>/dev/null || true\n",
    )
    .unwrap();
    std::fs::write(
        hooks_dir.join("post-merge"),
        "#!/bin/sh\necho '人工合并后处理'\n",
    )
    .unwrap();
    let home = unique_dir("rm_own_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(
        out.status.success(),
        "uninstall --force 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !hooks_dir.join("post-commit").exists(),
        "含 code-repo-wiki 标记的 post-commit 应被删除"
    );
    assert!(
        hooks_dir.join("post-merge").exists(),
        "人工 post-merge（无 code-repo-wiki 标记）应保留"
    );
    assert_eq!(
        std::fs::read_to_string(hooks_dir.join("post-merge")).unwrap(),
        "#!/bin/sh\necho '人工合并后处理'\n",
        "保留的 hook 内容不应被改动"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v33 升级语义：install 对已存在的旧模板 hook（含 code-repo-wiki 标记）覆盖升级
/// （内容更新为带 # code-repo-wiki managed 标记的新模板）
#[test]
fn test_install_upgrades_legacy_hooks() {
    let work_dir = unique_dir("upgrade_hooks");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    // 预置旧版本模板 hook（v33 前：含 code-repo-wiki 但无 managed 标记行）
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\n# code-repo-wiki: auto-update wiki on commit\ncode-repo-wiki update 2>/dev/null || true\n",
    )
    .unwrap();
    // 预置人工 hook（无 code-repo-wiki 标记）——install 不得覆盖
    std::fs::write(
        hooks_dir.join("post-merge"),
        "#!/bin/sh\necho '人工 hook'\n",
    )
    .unwrap();
    let home = unique_dir("upgrade_hooks_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 旧模板 post-commit 被升级为带 managed 标记的新模板
    let upgraded = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert!(
        upgraded.contains("# code-repo-wiki managed"),
        "旧模板应升级为含 managed 标记，实际: {upgraded}"
    );
    assert!(
        upgraded.contains("code-repo-wiki update"),
        "升级后仍应含 update 命令"
    );
    // 人工 post-merge 原样保留
    assert_eq!(
        std::fs::read_to_string(hooks_dir.join("post-merge")).unwrap(),
        "#!/bin/sh\necho '人工 hook'\n",
        "人工 hook 不应被覆盖"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}
