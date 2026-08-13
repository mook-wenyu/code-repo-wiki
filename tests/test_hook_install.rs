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
            content.contains("--skip-if-locked"),
            "{hook_name} 应含 --skip-if-locked（Phase 15.3 消除 kill -0 TOCTOU），实际: {content}"
        );
        assert!(
            !content.contains("kill -0"),
            "{hook_name} 不应含 kill -0 锁感知块（fd-lock 下锁文件常驻，kill -0 判定失真），实际: {content}"
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
        "code-repo-wiki update --skip-if-locked",
        "hook 命令应为 code-repo-wiki update --skip-if-locked（Phase 15.3 并发由命令内原子拿锁处理），实际 hook:\n{content}"
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

/// v33 升级语义（v41 追加块版）：install 对已存在的旧模板 hook（含
/// code-repo-wiki 旧标记）整文件覆盖升级为当前模板；人工 hook 尾部追加
/// 块（原内容保留）
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
    // 预置人工 hook（无 code-repo-wiki 标记）——install 追加块而非覆盖
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
    // 旧模板 post-commit 被升级为含当前标记的新模板
    let upgraded = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert!(
        upgraded.contains("# code-repo-wiki: append-begin"),
        "旧模板应升级为含当前标记，实际: {upgraded}"
    );
    assert!(
        upgraded.contains("code-repo-wiki update"),
        "升级后仍应含 update 命令"
    );
    // 人工 post-merge 原样保留 + 追加块
    let merged = std::fs::read_to_string(hooks_dir.join("post-merge")).unwrap();
    assert!(
        merged.contains("echo '人工 hook'"),
        "人工 hook 原内容应保留，实际: {merged}"
    );
    assert!(
        merged.contains("# code-repo-wiki: append-begin"),
        "人工 hook 应追加 code-repo-wiki 块，实际: {merged}"
    );
    assert!(
        merged.contains("code-repo-wiki update"),
        "追加块应含 update 命令，实际: {merged}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v41：人工 hook + 追加块后再次 install 幂等——不重复追加块
#[test]
fn test_install_appends_idempotent() {
    let work_dir = unique_dir("append_idem");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\necho '人工 hook'\n",
    )
    .unwrap();
    let home = unique_dir("append_idem_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // 第一次 install：追加块
    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    let once = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    // 第二次 install：块已存在且内容相同 → 不重复追加
    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "第二次 install 应成功");
    let twice = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert_eq!(once, twice, "幂等：第二次 install 不得改变 hook 内容");
    assert_eq!(
        twice.matches("# code-repo-wiki: append-begin").count(),
        1,
        "追加块只应出现一次，实际: {twice}"
    );
    assert!(
        twice.contains("echo '人工 hook'"),
        "人工 hook 原内容应保留，实际: {twice}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v41：追加块场景的升级——只替换块区间，人工内容保留
#[test]
fn test_install_upgrades_appended_block_keeps_user_content() {
    let work_dir = unique_dir("append_upgrade");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    // 预置：人工 hook + 旧版追加块（块内容与当前模板不同——v33 旧模板行）
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\necho '人工 hook'\n# code-repo-wiki: append-begin\ncode-repo-wiki update 2>/dev/null || true\n# code-repo-wiki: append-end\n",
    )
    .unwrap();
    let home = unique_dir("append_upgrade_home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs: Vec<(&str, String)> = home_envs(&home);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    let content = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert!(
        content.contains("echo '人工 hook'"),
        "人工内容应保留，实际: {content}"
    );
    assert!(
        content.contains("2>>.code-repo-wiki/update-error.log"),
        "块应升级为当前模板（v36 D2 错误日志特性），实际: {content}"
    );
    assert_eq!(
        content.matches("# code-repo-wiki: append-begin").count(),
        1,
        "块只应有一个，实际: {content}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// Phase 15.3（lock-audit-007）：旧版 v13.3 kill -0 锁感知追加块 → 新版
/// --skip-if-locked 块升级——fd-lock 下锁文件常驻、kill -0 活性判定失真
/// （check-then-act TOCTOU），旧块必须被整块替换，不能残留
#[test]
fn test_install_upgrades_kill0_lock_block_to_skip_if_locked() {
    let work_dir = unique_dir("lock_block_upgrade");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    // 预置：人工 hook + 旧版（v13.3）kill -0 锁感知追加块（带 begin/end 标记）
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\necho '人工 hook'\n# code-repo-wiki: append-begin\n# v13.3 锁感知：另一实例运行时跳过\nlock=\"$(git rev-parse --show-toplevel)/.code-repo-wiki/.state/run.lock\"\nif [ -f \"$lock\" ]; then\n  pid=\"$(cat \"$lock\" 2>/dev/null)\"\n  if [ -n \"$pid\" ] && kill -0 \"$pid\" 2>/dev/null; then\n    echo \"code-repo-wiki: 另一实例正在运行，跳过本次提交更新\" >&2\n    exit 0\n  fi\nfi\ncode-repo-wiki update 2>>.code-repo-wiki/update-error.log || echo \"code-repo-wiki: wiki 更新失败\" >&2\n# code-repo-wiki: append-end\n",
    )
    .unwrap();
    let home = unique_dir("lock_block_upgrade_home");
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
    let content = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert!(
        content.contains("echo '人工 hook'"),
        "人工内容应保留，实际: {content}"
    );
    assert!(
        !content.contains("kill -0"),
        "旧 kill -0 锁感知块应被整块替换（TOCTOU 消除），实际: {content}"
    );
    assert!(
        content.contains("--skip-if-locked"),
        "块应升级为 --skip-if-locked 版本，实际: {content}"
    );
    assert!(
        content.contains("2>>.code-repo-wiki/update-error.log"),
        "错误日志特性保留，实际: {content}"
    );
    assert_eq!(
        content.matches("# code-repo-wiki: append-begin").count(),
        1,
        "块只应有一个，实际: {content}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v41：uninstall 剥离追加块区间，人工 hook 内容保留写回
#[test]
fn test_uninstall_strips_appended_block() {
    let work_dir = unique_dir("strip_block");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let hooks_dir = init_git_repo(&work_dir);
    // 预置：人工 hook + 追加块
    std::fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\necho '人工 hook'\n# code-repo-wiki: append-begin\n# 自动更新 wiki\ncode-repo-wiki update 2>>.code-repo-wiki/update-error.log || echo x >&2\n# code-repo-wiki: append-end\n",
    )
    .unwrap();
    let home = unique_dir("strip_block_home");
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
    let remaining = std::fs::read_to_string(hooks_dir.join("post-commit")).unwrap();
    assert!(
        remaining.contains("echo '人工 hook'"),
        "人工内容应保留，实际: {remaining}"
    );
    assert!(
        !remaining.contains("code-repo-wiki"),
        "块应被剥离，实际: {remaining}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}
