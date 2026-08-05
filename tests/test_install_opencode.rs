//! install/uninstall 插件文件闭环测试（票 05）
//!
//! 通过 env!("CARGO_BIN_EXE_repo-wiki") 调用真实二进制，覆盖：
//! 1. install 写入 .opencode/plugins/repo-wiki.ts（含 RepoWikiPlugin 实现，
//!    重复 install 幂等：已存在不覆盖（mtime/内容不变））
//! 2. install 不在项目级自动创建配置文件（v24：配置属非项目内容，
//!    自动创建只发生在用户级目录——只输出 repo-wiki init 引导提示）
//! 3. uninstall --force 删除插件文件，但保留 .repo-wiki/ 数据目录
//!
//! install/uninstall 会读写 opencode 全局配置，一律隔离 HOME/USERPROFILE
//! （Windows 下 config_dir 依赖 USERPROFILE，两个都要设，参照 test_cli.rs）。

use std::path::{Path, PathBuf};

mod common;
use common::{run_bin_with_envs, unique_dir};

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

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
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
    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
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

/// v24 语义反转：install 不得在项目级自动创建配置文件（配置属非项目
/// 内容，自动创建只发生在用户级目录）——只输出引导提示
#[test]
fn test_install_does_not_create_project_config() {
    let (work_dir, home, envs) = setup("no_default_config");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 项目级配置与产物目录都不得被 install 创建
    let config_path = work_dir.join("config.toml");
    assert!(
        !config_path.exists(),
        "install 不得创建项目级配置: {}",
        config_path.display()
    );
    assert!(
        !work_dir.join(".repo-wiki").exists(),
        "install 不得创建产物/配置目录: {}",
        work_dir.join(".repo-wiki").display()
    );
    // 输出应包含配置引导提示
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("repo-wiki install"),
        "install 应提示配置引导，实际 stdout: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// uninstall --force 删除插件文件，但保留 .repo-wiki/ 数据目录
/// （v24：install 不再创建项目级配置/产物，此处预置数据目录模拟已有产物）
#[test]
fn test_uninstall_removes_plugin_file() {
    let (work_dir, home, envs) = setup("remove_plugin");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    assert!(plugin_file(&work_dir).exists(), "install 后插件文件应存在");

    // 预置产物数据目录（模拟用户已有 wiki 产物——uninstall 不得触碰）
    std::fs::create_dir_all(work_dir.join(".repo-wiki").join("wiki")).unwrap();
    std::fs::write(work_dir.join(".repo-wiki").join("wiki").join("sentinel.md"), "data").unwrap();

    let out = run_bin_with_envs(&work_dir, &["uninstall-from-opencode", "--force"], &envs_ref);
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
        work_dir.join(".repo-wiki").join("wiki").join("sentinel.md").exists(),
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

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
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
