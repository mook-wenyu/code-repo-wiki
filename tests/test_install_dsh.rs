//! DeepSeek Harness（dsh）`--dsh` 安装闭环测试（W3）
//!
//! 通过 env!("CARGO_BIN_EXE_code-repo-wiki") 调用真实二进制，覆盖：
//! 1. install --dsh 写项目根 cordis.patch.yml 的 `- insert:` 管理块，
//!    注册 @deepseek-ai/dsh-mcp-client（stdio，command=当前 exe 绝对路径，
//!    args=[mcp]，cwd=process.cwd()）
//! 2. 重复 install --dsh 幂等（内容不变）
//! 3. --dsh 与 --codex 互不排斥（两个目标同时写入）
//! 4. uninstall --force 移除 cordis.patch.yml 管理块；未安装时退出码 0
//!
//! install 默认步骤仍会写用户级 opencode.json/插件与用户级默认配置，
//! 必须隔离 HOME/USERPROFILE（Windows 下 config_dir 依赖 USERPROFILE，
//! 参照 test_install_opencode.rs 模式）。

use std::path::{Path, PathBuf};

mod common;
use common::{run_bin_with_envs, unique_dir};

const BLOCK_START: &str = "# code-repo-wiki: dsh-mcp-client insert-begin";
const BLOCK_END: &str = "# code-repo-wiki: dsh-mcp-client insert-end";

/// 创建隔离工作目录 + 隔离 HOME，返回 (work_dir, home, envs)
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

/// 项目根 cordis.patch.yml 路径
fn patch_file(work_dir: &Path) -> PathBuf {
    work_dir.join("cordis.patch.yml")
}

/// 项目根 ~/.codex/config.toml（--codex 互斥性验证用，隔离 HOME 下）
fn codex_config(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

// ==================== 测试用例 ====================

/// install --dsh：写项目根 cordis.patch.yml 管理块（官方 dsh patch 层形态）
#[test]
fn test_install_dsh_writes_patch_file() {
    let (work_dir, _home, envs) = setup("dsh_writes");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install", "--dsh"], &envs_ref);
    assert!(
        out.status.success(),
        "install --dsh 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = patch_file(&work_dir);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cordis.patch.yml 应被写入 {}: {}", path.display(), e));
    assert!(content.contains(BLOCK_START), "应含管理块开始标记");
    assert!(content.contains(BLOCK_END), "应含管理块结束标记");
    assert!(content.contains("- insert:"), "应含 insert patch 操作");
    assert!(
        content.contains("name: '@deepseek-ai/dsh-mcp-client'"),
        "应注册官方 dsh mcp-client 插件"
    );
    assert!(
        content.contains("serverName: code-repo-wiki"),
        "serverName 应为 code-repo-wiki"
    );
    assert!(content.contains("transport: stdio"), "应为 stdio 传输");
    let exe = env!("CARGO_BIN_EXE_code-repo-wiki");
    assert!(
        content.contains("command: \"") && content.contains(exe.replace('\\', "\\\\").as_str()),
        "command 应注入当前 exe 绝对路径（转义形态），实际: {content}"
    );
    assert!(content.contains("args: [mcp]"), "args 应为 [mcp]");
    assert!(
        content.contains("cwd: !!js process.cwd()"),
        "cwd 应为 dsh 工作区"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// install --dsh 幂等：重复执行内容不变
#[test]
fn test_install_dsh_idempotent() {
    let (work_dir, _home, envs) = setup("dsh_idem");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out1 = run_bin_with_envs(&work_dir, &["install", "--dsh"], &envs_ref);
    assert!(out1.status.success(), "首次 install --dsh 应成功");
    let path = patch_file(&work_dir);
    let content1 = std::fs::read_to_string(&path).unwrap();

    let out2 = run_bin_with_envs(&work_dir, &["install", "--dsh"], &envs_ref);
    assert!(out2.status.success(), "重复 install --dsh 应成功");
    let content2 = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content1, content2, "重复安装应幂等（内容不变）");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// --dsh 与 --codex 互不排斥：同一次 install 同时写 cordis.patch.yml 与
/// ~/.codex/config.toml
#[test]
fn test_install_dsh_with_codex_both_written() {
    let (work_dir, home, envs) = setup("dsh_codex");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["install", "--dsh", "--codex"], &envs_ref);
    assert!(
        out.status.success(),
        "install --dsh --codex 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let patch = std::fs::read_to_string(patch_file(&work_dir)).unwrap();
    assert!(patch.contains(BLOCK_START), "cordis.patch.yml 应写入");
    let codex = std::fs::read_to_string(codex_config(&home)).unwrap();
    assert!(
        codex.contains("[mcp_servers.code-repo-wiki]"),
        "~/.codex/config.toml 应写入"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// uninstall --force：移除 cordis.patch.yml 管理块，用户无关内容保留；
/// 未安装时退出码 0（幂等）
#[test]
fn test_uninstall_dsh_removes_block() {
    let (work_dir, _home, envs) = setup("dsh_uninstall");
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = patch_file(&work_dir);

    // 预置用户 patch + 本工具管理块（模拟 install --dsh 后用户追加内容）
    std::fs::write(
        &path,
        "# 用户注释\n- insert:\n    - id: memory-my-server\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: my-memory\n        transport: stdio\n        command: my-memory-mcp\n",
    )
    .unwrap();
    let out = run_bin_with_envs(&work_dir, &["install", "--dsh"], &envs_ref);
    assert!(out.status.success(), "install --dsh 应成功");

    let out2 = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(
        out2.status.success(),
        "uninstall --force 应成功，stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains(BLOCK_START) && !content.contains(BLOCK_END),
        "管理块标记应被移除，实际: {content}"
    );
    assert!(
        !content.contains("id: code-repo-wiki"),
        "本工具行应被移除，实际: {content}"
    );
    assert!(
        content.contains("# 用户注释") && content.contains("id: memory-my-server"),
        "用户无关内容应保留，实际: {content}"
    );

    // 再次卸载（未安装）→ 退出码 0 且提示
    let out3 = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(out3.status.success(), "未安装时卸载应退出码 0");
    let stdout = String::from_utf8_lossy(&out3.stdout);
    assert!(
        stdout.contains("不存在"),
        "应提示条目不存在，实际: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
