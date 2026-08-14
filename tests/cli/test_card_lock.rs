//! Phase 15.4 lock-audit-001：card 命令纳入运行锁的集成测试
//!
//! card generate/modify/supplement/rewrite 写卡片但不持锁，与 generate/
//! update/watch 并发会把同一卡片互相覆盖（双写）。本文件验证：
//! 1. card 撞内核写锁 → 拒绝（非 0 退出码 + 「正在运行」）；
//! 2. card --skip-if-locked 撞锁 → 退出码 0 跳过（Phase 15.2 同语义）。
//!
//! 锁路径对齐：子进程 cwd=work_dir、config 无 output.dir → output_dir()
//! 回退 .code-repo-wiki 相对 cwd = work_dir/.code-repo-wiki（同 test_cli.rs
//! 的 watch 撞锁与 generate/update --skip-if-locked 测试）。持锁者身份经
//! acquire_run_lock 写入锁文件；Windows 上 LockFileEx 阻止子进程读锁定
//! 区域，错误信息会退化为「PID 未知」——断言只依赖「正在运行」。

use code_repo_wiki::config::schema::WikiConfig;
use code_repo_wiki::fs::acquire_run_lock;
use crate::common::{mock_llm_server, openai_compatible_config, run_bin_with_envs, unique_dir};
use std::path::Path;

/// 复制 fixture 并写入指向 mock LLM 的 mock-server.toml，返回工作目录
fn prepare_repo(tag: &str) -> std::path::PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    crate::common::copy_dir(&fixture, &work_dir);
    let port = mock_llm_server();
    std::fs::write(
        work_dir.join("mock-server.toml"),
        openai_compatible_config(port),
    )
    .unwrap();
    work_dir
}

/// card 撞内核写锁：主测试进程持锁，card generate 子进程（独立进程打开
/// 同一锁文件）必须立即失败（非 0 退出码 + 含「正在运行」），防止 card
/// 与 watch/generate 并发双写同一卡片
#[test]
fn test_card_rejected_while_locked() {
    let work_dir = prepare_repo("card_lock_conflict");

    // 主测试进程持有内核写锁，锁路径与 card 子进程一致
    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "generate",
            "src::config",
            "--config",
            "mock-server.toml",
        ],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "card 撞内核锁应拒绝，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("正在运行"),
        "撞锁报错应含「正在运行」提示，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// card --skip-if-locked：撞锁时退出码 0 跳过（与 generate/update 同语义，
/// hook/CI 非阻塞拿锁）
#[test]
fn test_card_skip_if_locked_exits_zero() {
    let work_dir = prepare_repo("card_lock_skip");

    let config = WikiConfig {
        output_dir: Some(work_dir.join(".code-repo-wiki")),
        ..Default::default()
    };
    let _lock = acquire_run_lock(&config).expect("主测试进程应能获取运行锁");

    let out = run_bin_with_envs(
        &work_dir,
        &[
            "card",
            "generate",
            "src::config",
            "--config",
            "mock-server.toml",
            "--skip-if-locked",
        ],
        &[],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "card --skip-if-locked 撞锁应退出码 0 跳过，实际 status: {:?}\n输出: {combined}",
        out.status
    );
    assert!(
        combined.contains("跳过"),
        "跳过提示应含「跳过」，实际输出: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
