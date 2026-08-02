//! install/uninstall 插件文件闭环集成测试（票 05）
//!
//! 通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制，覆盖：
//! 1. install 写入 .opencode/plugins/repo-wiki.ts（内容含 RepoWikiPlugin），
//!    重复 install 幂等（已存在不覆盖，mtime/内容不变）
//! 2. 空目录 install 生成默认 .repo-wiki/config.toml
//! 3. uninstall --force 删除插件文件但保留 .repo-wiki/
//! 4. 已存在的自定义插件文件不被 install 覆盖
//!
//! install/uninstall 会读写 opencode 全局配置，一律隔离 HOME/USERPROFILE
//! （Windows 下 config_dir 依赖 USERPROFILE，两个都要设，参照 test_cli.rs）。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 进程内自增序号：同一进程内多个测试并行时临时目录互不冲突
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 生成唯一临时目录（进程 id + 自增序号）
fn unique_dir(name: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("repo_wiki_install_{}_{}_{}", name, std::process::id(), seq))
}

/// 在指定目录下执行 repo-wiki 二进制（额外环境变量可选），返回完整输出
fn run_bin(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_repo-wiki"));
    cmd.args(args)
        .current_dir(dir)
        .env("RUST_LOG", "off") // 关闭 tracing 日志，保证 stdout 只有业务输出
        .env_remove("OPENAI_API_KEY"); // 避免宿主机真实 Key 被误用
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("执行 repo-wiki 二进制失败")
}

/// 隔离 HOME/USERPROFILE（opencode 全局配置读写均以此为准）
fn home_envs(home: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("HOME", home.to_string_lossy().into_owned()),
        ("USERPROFILE", home.to_string_lossy().into_owned()),
    ]
}

/// 创建隔离工作目录 + 隔离 HOME，返回 (work_dir, home_dir, 环境变量)
fn setup(tag: &str) -> (PathBuf, PathBuf, Vec<(&'static str, String)>) {
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    let home = unique_dir(&format!("{tag}_home"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let envs = home_envs(&home);
    (work_dir, home, envs)
}

/// 插件文件路径（相对工作目录）
fn plugin_file(work_dir: &Path) -> PathBuf {
    work_dir.join(".opencode").join("plugins").join("repo-wiki.ts")
}

/// 将 envs（Vec<(&str, String)>）转为 run_bin 的切片参数
fn as_env_refs<'a>(envs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    envs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

// ==================== 测试用例 ====================

/// install 写入插件文件（内容含 RepoWikiPlugin）；重复 install 幂等：
/// 已存在不覆盖，mtime 与内容不变
#[test]
fn test_install_writes_plugin_file() {
    let (work_dir, home, envs) = setup("writes_plugin");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin(&work_dir, &["install-to-opencode"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = plugin_file(&work_dir);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("插件文件应写入 {}: {}", path.display(), e));
    assert!(
        content.contains("RepoWikiPlugin"),
        "插件内容应含 RepoWikiPlugin 导出，实际: {}",
        &content[..content.len().min(200)]
    );
    let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

    // 重复 install：文件未被覆盖（mtime/内容不变）
    let out = run_bin(&work_dir, &["install-to-opencode"], &envs_ref);
    assert!(out.status.success(), "重复 install 应成功");
    let content_after = std::fs::read_to_string(&path).unwrap();
    let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        content_after, content,
        "重复 install 不应覆盖已有插件文件"
    );
    assert_eq!(mtime_after, mtime_before, "重复 install 不应触碰文件 mtime");

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 空目录 install 生成默认 .repo-wiki/config.toml
#[test]
fn test_install_creates_default_config() {
    let (work_dir, home, envs) = setup("default_config");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin(&work_dir, &["install-to-opencode"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config_path = work_dir.join(".repo-wiki").join("config.toml");
    assert!(
        config_path.exists(),
        "默认配置应生成: {}",
        config_path.display()
    );
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("[llm]"),
        "默认配置应含 [llm] 段，实际:\n{}",
        &content[..content.len().min(200)]
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// uninstall --force 删除插件文件，但保留 .repo-wiki/ 数据目录
#[test]
fn test_uninstall_removes_plugin_file() {
    let (work_dir, home, envs) = setup("remove_plugin");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin(&work_dir, &["install-to-opencode"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    assert!(plugin_file(&work_dir).exists(), "install 后插件文件应存在");

    let out = run_bin(&work_dir, &["uninstall-from-opencode", "--force"], &envs_ref);
    assert!(
        out.status.success(),
        "uninstall --force 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !plugin_file(&work_dir).exists(),
        "uninstall 后插件文件应被删除"
    );
    assert!(
        work_dir.join(".repo-wiki").exists(),
        "uninstall 应保留 .repo-wiki/ 数据目录"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 已存在的自定义插件文件不被 install 覆盖（人工修改保留）
#[test]
fn test_install_plugin_file_preserves_existing() {
    let (work_dir, home, envs) = setup("preserve_existing");
    let envs_ref = as_env_refs(&envs);

    // 预置自定义插件文件（人工修改内容）
    let path = plugin_file(&work_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "export const CustomPlugin = () => ({});\n").unwrap();

    let out = run_bin(&work_dir, &["install-to-opencode"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "export const CustomPlugin = () => ({});\n",
        "install 不应覆盖已存在的自定义插件文件"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}
