//! install 模板（default-config.toml）与 config schema 一致性测试
//!
//! 防回归背景：模板曾含 `[project]`、`[generate]` 死键（schema 中不存在，
//! serde 静默忽略），用户按模板配置 `max_concurrency` 等无效。本测试锁定
//! "模板无死键 + 模板可被 load_config 正常加载"两个性质。

use std::path::PathBuf;

use repo_wiki::config::{load_config, schema};

const TEMPLATE: &str = include_str!("../default-config.toml");

/// 解析模板并断言不存在已废弃的配置段
#[test]
fn test_template_no_dead_keys() {
    let value: toml::Value = toml::from_str(TEMPLATE).expect("模板必须是合法 TOML");
    let table = value.as_table().expect("模板必须是表结构");

    // 死键段：`[project]`、`[generate]` 已从 schema 移除
    assert!(!table.contains_key("project"), "模板不得含死键段 [project]");
    assert!(!table.contains_key("generate"), "模板不得含死键段 [generate]");

    // schema 现有段必须齐全
    for section in ["wiki", "scope", "output", "llm", "embed", "search", "incremental", "plan"] {
        assert!(table.contains_key(section), "模板缺少配置段 [{section}]");
    }
}

/// 模板经 load_config 必须能完整加载（含默认值填充与校验通过）
#[test]
fn test_template_loads_cleanly() {
    let dir = std::env::temp_dir().join(format!("repo-wiki-template-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("config.toml");
    std::fs::write(&path, TEMPLATE).unwrap();

    let config: schema::WikiConfig = load_config(&path).expect("模板必须可被 load_config 加载");
    assert_eq!(config.llm.api_key_env, "DEEPSEEK_API_KEY");
    assert!(!config.scope.include.is_empty());
    // embed 段随模板配置（当前模板启用百炼 embedding，仅断言字段可解析）
    assert!(!config.embed.model.is_empty());
    assert!(!config.plan.enabled);

    let _ = std::fs::remove_dir_all(&dir);
}
