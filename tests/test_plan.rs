#![cfg(test)]

use std::path::Path;

/// 验证 plan.enabled=false 时默认配置不加载 plan
#[test]
fn test_plan_default_disabled() {
    let config = repo_wiki::config::schema::WikiConfig::default();
    // 默认 plan.enabled == false
    assert!(!config.plan.enabled);
    // 默认 plan.path == "wiki_plan.yaml"
    assert_eq!(config.plan.path, "wiki_plan.yaml");
}

/// 验证 WikiPlan 结构体默认值
#[test]
fn test_wiki_plan_defaults() {
    let plan = repo_wiki::config::plan::WikiPlan::default();
    assert!(plan.notes.is_none());
    assert!(plan.sections.is_empty());
    assert!(plan.documents.is_empty());
}

/// 验证 PlanTemplateType 序列化往返
#[test]
fn test_plan_template_type_roundtrip() {
    use repo_wiki::config::plan::PlanTemplateType;
    for (ty, expected) in [
        (PlanTemplateType::Architecture, "architecture"),
        (PlanTemplateType::Prd, "prd"),
        (PlanTemplateType::ApiRef, "api-ref"),
    ] {
        let yaml = serde_yaml::to_string(&ty).unwrap();
        assert_eq!(yaml.trim(), expected);
    }
}

/// 验证 load_plan 在目录不存在时返回 Ok(None)
#[test]
fn test_load_plan_missing() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_test_plan_missing_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let result = repo_wiki::config::plan::load_plan(&dir);
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 验证 load_plan 能正确解析 wiki_plan.yaml
#[test]
fn test_load_plan_valid_yaml() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_test_plan_valid_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yaml_content = r#"
notes: "请重点关注安全设计"
sections:
  - module_pattern: "src/config/**"
    template_type: architecture
    notes: "配置模块架构文档"
documents:
  - title: "API 参考"
    goal: "生成 API 文档"
    include_patterns: ["src/api/**"]
"#;
    std::fs::write(dir.join("wiki_plan.yaml"), yaml_content).unwrap();
    let plan = repo_wiki::config::plan::load_plan(&dir).unwrap().unwrap();
    assert_eq!(plan.notes.unwrap(), "请重点关注安全设计");
    assert_eq!(plan.sections.len(), 1);
    assert_eq!(plan.sections[0].module_pattern, "src/config/**");
    assert_eq!(plan.documents.len(), 1);
    assert_eq!(plan.documents[0].title, "API 参考");
    let _ = std::fs::remove_dir_all(&dir);
}
