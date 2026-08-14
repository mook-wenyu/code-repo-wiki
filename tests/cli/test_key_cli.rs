//! key 命令 CLI 集成测试（A7.6 audit-cfg-08）
//!
//! 覆盖三条安全性质（经真实二进制 + 隔离用户级目录验证）：
//! 1. `--env` 模式不落明文：只写建议的 api_key_env 环境变量名引用；
//! 2. 明文/引用都只写用户级 config.toml，项目级 config.toml（随 Git 共享）
//!    永不被动过，也不被自动创建；
//! 3. 非交互终端（管道 stdin）打印引导退出 0，不写任何字段。
//!
//! 隔离方式：`CODE_REPO_WIKI_HOME` 指向临时目录（v41 全局目录重定位环境
//! 变量，ensure_global_config_dir 优先读它），与真实用户配置完全隔离。
//! 用户级配置文件预写为 provider=openai + 一个不可能存在的 env 名，规避
//! 宿主环境已设置 OPENCODEGO2_API_KEY/DEEPSEEK_API_KEY 等触发「已配置」
//! 早退分支（与 key.rs 单测 temp_global 同思路）。
//!
//! 明文交互写入路径依赖 TTY（IsTerminal 无法在子进程管道中伪造），由
//! key.rs 单测 test_key_writes_plain_api_key_to_user_config 覆盖；本文件
//! 聚焦 CLI 接线与「不写项目级」的负向保证。

use std::path::PathBuf;

use crate::common::{run_bin_with_envs, unique_dir};

/// 构造隔离环境，返回 (工作目录, 用户级配置路径, 用户级目录, 环境变量注入表)
fn setup(
    tag: &str,
    project_text: Option<&str>,
) -> (PathBuf, PathBuf, PathBuf, Vec<(&'static str, String)>) {
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).unwrap();
    if let Some(text) = project_text {
        std::fs::write(work_dir.join("config.toml"), text).unwrap();
    }

    let home = unique_dir(&format!("{tag}_home"));
    let _ = std::fs::remove_dir_all(&home);
    // CODE_REPO_WIKI_HOME 就是用户级配置目录本身（global_config_dir 优先读它，
    // 直接返回该值，不再拼 .code-repo-wiki），故用户配置写在 home/config.toml
    std::fs::create_dir_all(&home).unwrap();
    let user_cfg = home.join("config.toml");
    // api_key_env 指向不可能存在的 env 名（防「已配置」早退）；#api_key 占位保留
    std::fs::write(
        &user_cfg,
        "[llm]\nprovider = \"openai\"\nmodel = \"deepseek-v4-flash\"\nbase_url = \"https://opencode.ai/zen/go/v1\"\n#api_key = \"\"\napi_key_env = \"REPO_WIKI_CLI_TEST_ENV_NONE\"\n",
    )
    .unwrap();

    let home_str = home.to_string_lossy().into_owned();
    let envs = vec![
        ("CODE_REPO_WIKI_HOME", home_str.clone()),
        ("HOME", home_str.clone()),
        ("USERPROFILE", home_str.clone()),
        ("APPDATA", home_str.clone()),
    ];
    (work_dir, user_cfg, home, envs)
}

/// 统计配置文本中非注释的 `api_key = ` 行数（明文写入探测）
fn count_plain_api_key_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            t.strip_prefix("api_key")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .count()
}

/// --env 模式：不落明文，写建议名（openai 阵营 → OPENCODEGO2_API_KEY）；
/// 项目级 config.toml 不被创建、不被改动
#[test]
fn test_key_cli_env_writes_user_level_only() {
    let (work_dir, user_cfg, home, envs) = setup("key_env", None);
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["key", "--env"], &envs_ref);
    assert!(
        out.status.success(),
        "key --env 应退出 0，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("api_key_env = \"OPENCODEGO2_API_KEY\""),
        "应写建议 env 名（openai 默认阵营）: {stdout}"
    );

    // 用户级配置：env 引用写入，明文 api_key 不落
    let user_text = std::fs::read_to_string(&user_cfg).unwrap();
    assert!(
        user_text.contains("api_key_env = \"OPENCODEGO2_API_KEY\""),
        "用户级配置应含建议 env 引用: {user_text}"
    );
    assert_eq!(
        count_plain_api_key_lines(&user_text),
        0,
        "--env 模式不得写明文 api_key: {user_text}"
    );

    // 项目级：不创建、不写任何字段
    assert!(
        !work_dir.join("config.toml").exists(),
        "key 不得自动创建项目级 config.toml"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// 非交互（管道 stdin）：打印引导退出 0，用户级与项目级均不写字段
#[test]
fn test_key_cli_noninteractive_prints_guidance_no_write() {
    let (work_dir, user_cfg, home, envs) = setup("key_tty", None);
    let user_before = std::fs::read_to_string(&user_cfg).unwrap();
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["key"], &envs_ref);
    assert!(
        out.status.success(),
        "非交互 key 应退出 0，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("非交互"),
        "应打印非交互引导: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let user_after = std::fs::read_to_string(&user_cfg).unwrap();
    assert_eq!(user_after, user_before, "非交互不得改动用户级配置");
    assert!(
        !work_dir.join("config.toml").exists(),
        "项目级配置不得被创建"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// mock 项目级 provider：key 提示无需 key 即退出，项目级配置原样保留
/// （顺带断言 audit-cfg-06：项目级明文 api_key 加载时打印显式警告）
#[test]
fn test_key_cli_mock_project_untouched_and_warns() {
    let project_text =
        "[llm]\nprovider = \"mock\"\nmodel = \"mock\"\napi_key = \"sk-project-placeholder\"\n";
    let (work_dir, user_cfg, home, envs) = setup("key_mock", Some(project_text));
    let project_cfg = work_dir.join("config.toml");
    let project_before = std::fs::read_to_string(&project_cfg).unwrap();
    let user_before = std::fs::read_to_string(&user_cfg).unwrap();
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // 普通 key：mock provider 早退，不写任何字段
    let out = run_bin_with_envs(&work_dir, &["key"], &envs_ref);
    assert!(out.status.success(), "mock provider key 应退出 0");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("mock provider 无需 API key"),
        "应提示 mock provider 无需 key: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // audit-cfg-06：项目级明文 api_key → stderr 显式警告
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("明文 api_key"),
        "项目级明文 api_key 应打印警告: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // key --env：同样 mock 早退，不写引用
    let out_env = run_bin_with_envs(&work_dir, &["key", "--env"], &envs_ref);
    assert!(out_env.status.success(), "mock provider key --env 应退出 0");

    // 项目级与用户级均未被改动
    assert_eq!(
        std::fs::read_to_string(&project_cfg).unwrap(),
        project_before,
        "项目级配置不得被 key 改动"
    );
    assert_eq!(
        std::fs::read_to_string(&user_cfg).unwrap(),
        user_before,
        "mock 早退不得改动用户级配置"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// audit-cfg-06：项目级 config.toml 明文 api_key 在默认配置链加载时
/// 打印显式警告（经 status 命令触发，隔离 HOME；exit 0 不阻断主流程）
#[test]
fn test_project_plaintext_key_warns_on_default_chain_load() {
    let project_text =
        "[llm]\nprovider = \"mock\"\nmodel = \"mock\"\napi_key = \"sk-leak-in-git\"\n";
    let (work_dir, _user_cfg, home, envs) = setup("plain_warn", Some(project_text));
    let envs_ref: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_bin_with_envs(&work_dir, &["status"], &envs_ref);
    assert!(
        out.status.success(),
        "警告不应阻断主流程，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("明文 api_key") && stderr.contains("api_key_env"),
        "应警告项目级明文 api_key 并引导改用 env: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
    let _ = std::fs::remove_dir_all(&home);
}
