//! install/uninstall 插件与 MCP 闭环测试（票 05 + v33 合并版 + v39 用户级插件化）
//!
//! 通过 env!("CARGO_BIN_EXE_code-repo-wiki") 调用真实二进制，覆盖：
//! 1. install 写入用户级插件文件 `~/.config/opencode/plugins/code-repo-wiki.ts`
//!    （v39：插件是用户级内容——装进 Agent 配置根目录，官方自动加载目录；
//!    含 RepoWikiPlugin 实现，重复 install 幂等：内容一致不重写（mtime/内容不变））
//! 2. install 注册 OpenCode MCP 到用户级全局 opencode.json（v33 拍板：
//!    用户级 mcp 块，type=local + command=当前 exe 绝对路径）
//! 3. install 不在项目级自动创建配置文件（v24：配置属非项目内容）
//! 4. uninstall --force 删除用户级插件文件 + 移除 MCP 条目，但保留 .code-repo-wiki/ 数据
//! 5. v33 升级语义：旧版本插件模板内容被 install 覆盖升级（不再保留跳过）
//! 6. v39 迁移：项目级旧版插件产物（.opencode/plugins/）被 install/uninstall 清理
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

/// 用户级插件文件路径（v39：装 Agent 配置根目录 ~/.config/opencode/plugins/）
fn user_plugin_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join("opencode")
        .join("plugins")
        .join("code-repo-wiki.ts")
}

/// v39 之前的旧版项目级插件产物路径（迁移清理目标）
fn legacy_project_plugin_file(work_dir: &Path) -> PathBuf {
    work_dir.join(".opencode").join("plugins").join("code-repo-wiki.ts")
}

/// 用户级 opencode.json 路径（隔离 HOME 下）
fn opencode_config(home: &Path) -> PathBuf {
    home.join(".config").join("opencode").join("opencode.json")
}

/// 将 envs（Vec<(&str, String)>）转为 run_bin 的切片参数
fn as_env_refs<'a>(envs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    envs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

// ==================== 测试用例 ====================

/// install 写入用户级插件文件（内容含 RepoWikiPlugin）并注册用户级全局 MCP；
/// 重复 install 幂等：内容一致不重写（mtime 与内容不变）
#[test]
fn test_install_writes_plugin_file_and_registers_mcp() {
    let (work_dir, home, envs) = setup("writes_plugin");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = user_plugin_file(&home);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("用户级插件文件应写入 {}: {}", path.display(), e));
    assert!(
        content.contains("RepoWikiPlugin"),
        "插件内容应含 RepoWikiPlugin 导出，实际: {}",
        &content[..content.len().min(200)]
    );
    let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
    // v39：项目级不得写入插件（用户级内容装 Agent 配置根目录）
    assert!(
        !legacy_project_plugin_file(&work_dir).exists(),
        "install 不得写入项目级 .opencode/plugins/（v39 用户级语义）"
    );

    // v33：OpenCode MCP 注册到用户级全局 opencode.json（type=local + command 数组）
    let oc_path = opencode_config(&home);
    let oc_content = std::fs::read_to_string(&oc_path)
        .unwrap_or_else(|e| panic!("用户级 opencode.json 应写入 {}: {}", oc_path.display(), e));
    let oc: serde_json::Value = serde_json::from_str(&oc_content).expect("opencode.json 应为合法 JSON");
    let entry = &oc["mcp"]["code-repo-wiki"];
    assert_eq!(entry["type"], "local", "MCP 条目应为 local 类型");
    assert_eq!(entry["command"][0], env!("CARGO_BIN_EXE_code-repo-wiki"), "command 应为当前可执行文件绝对路径");
    assert_eq!(entry["command"][1], "mcp");
    assert_eq!(entry["enabled"], true);

    // 重复 install：文件未被覆盖（mtime/内容不变）
    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "重复 install 应成功");
    let content_after = std::fs::read_to_string(&path).unwrap();
    let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        content_after, content,
        "重复 install 不应重写内容一致的插件文件"
    );
    assert_eq!(mtime_after, mtime_before, "重复 install 不应触碰文件 mtime");

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v24 语义：install 不得在项目级自动创建配置文件（配置属非项目
/// 内容，自动创建只发生在用户级目录）
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
        !work_dir.join(".code-repo-wiki").exists(),
        "install 不得创建产物/配置目录: {}",
        work_dir.join(".code-repo-wiki").display()
    );
    // 用户级配置确保（v25 语义）在隔离 HOME 下创建
    assert!(
        opencode_config(&home).exists(),
        "install 应确保用户级 opencode.json 就绪"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// uninstall --force 删除插件文件并移除 MCP 条目，但保留 .code-repo-wiki/ 数据目录
/// （v24：install 不再创建项目级配置/产物，此处预置数据目录模拟已有产物）
#[test]
fn test_uninstall_removes_plugin_file_and_mcp() {
    let (work_dir, home, envs) = setup("remove_plugin");
    let envs_ref = as_env_refs(&envs);

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(out.status.success(), "install 应成功");
    assert!(user_plugin_file(&home).exists(), "install 后用户级插件文件应存在");
    // 预置用户级配置中的其他键（模拟用户自有配置）——install 升级不得触碰，
    // uninstall 只移除 code-repo-wiki 条目（文件保持存在）
    let oc_path = opencode_config(&home);
    let user_cfg = serde_json::json!({ "plugin": ["user-plugin"], "mcp": { "other": { "type": "remote", "url": "https://example.com/mcp" } } });
    std::fs::write(&oc_path, serde_json::to_string_pretty(&user_cfg).unwrap()).unwrap();

    // 预置产物数据目录（模拟用户已有 wiki 产物——uninstall 不得触碰）
    std::fs::create_dir_all(work_dir.join(".code-repo-wiki").join("wiki")).unwrap();
    std::fs::write(work_dir.join(".code-repo-wiki").join("wiki").join("sentinel.md"), "data").unwrap();

    let out = run_bin_with_envs(&work_dir, &["uninstall", "--force"], &envs_ref);
    assert!(
        out.status.success(),
        "uninstall --force 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !user_plugin_file(&home).exists(),
        "uninstall 后用户级插件文件应被删除"
    );
    assert!(
        !legacy_project_plugin_file(&work_dir).exists(),
        "uninstall 后旧版项目级插件文件也应被清理"
    );
    // v33：用户级全局 MCP 条目被移除（其他键保留；文件非空则不删除）
    let oc_content = std::fs::read_to_string(opencode_config(&home)).unwrap();
    let oc: serde_json::Value = serde_json::from_str(&oc_content).expect("opencode.json 应为合法 JSON");
    assert!(
        oc.get("mcp").and_then(|m| m.get("code-repo-wiki")).is_none(),
        "uninstall 后 MCP 条目应移除"
    );
    assert_eq!(
        oc.get("plugin").and_then(|p| p.as_array()).map(|a| a.len()).unwrap_or(0),
        1,
        "用户预置的其他键（plugin）应保留"
    );
    assert!(
        work_dir.join(".code-repo-wiki").join("wiki").join("sentinel.md").exists(),
        "uninstall 应保留 .code-repo-wiki/ 数据目录"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// v33 升级语义：已存在的旧版本插件模板被 install 覆盖升级
/// （内容比对——旧模板含 execa("code-repo-wiki") 字面量，升级后为绝对路径注入；
///  与 v32 及以前的「已存在不覆盖」语义相反，用户拍板「带标记则升级」）
/// v39：旧文件位于用户级配置根；另验证项目级旧产物被迁移清理
#[test]
fn test_install_upgrades_legacy_plugin_file() {
    let (work_dir, home, envs) = setup("upgrade_legacy");
    let envs_ref = as_env_refs(&envs);

    // 预置旧版本用户级插件文件（PATH 依赖版：execa("code-repo-wiki" 未注入绝对路径）
    let path = user_plugin_file(&home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "import { execa } from 'execa';\nconst runCli = (directory) => execa(\"code-repo-wiki\", [\"status\"], { cwd: directory });\nexport const RepoWikiPlugin = () => ({ tool: {} });\n",
    )
    .unwrap();
    // 预置 v39 之前的旧版项目级插件产物（应被迁移清理）
    let legacy = legacy_project_plugin_file(&work_dir);
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "export const RepoWikiPlugin = () => ({});").unwrap();

    let out = run_bin_with_envs(&work_dir, &["install"], &envs_ref);
    assert!(
        out.status.success(),
        "install 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let upgraded = std::fs::read_to_string(&path).unwrap();
    assert!(
        !upgraded.contains("execa(\"code-repo-wiki\""),
        "升级后不应再含 PATH 字面量 execa(\"code-repo-wiki\""
    );
    assert!(
        upgraded.contains("execa(\""),
        "升级后应注入绝对路径 execa(\"<abs path>\""
    );
    assert!(
        upgraded.contains("RepoWikiPlugin"),
        "升级后应含完整插件实现"
    );
    assert!(
        !legacy.exists(),
        "旧版项目级插件产物应被迁移清理（v39 用户级语义）"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}
