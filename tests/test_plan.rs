#![cfg(test)]

use std::path::Path;
use std::sync::Mutex;

use repo_wiki::config::plan::{WikiPlan, load_plan_at, resolve_plan_at, PlanDocument, PlanSection, PlanTemplateType};
use repo_wiki::config::schema::WikiConfig;

/// 串行化依赖当前工作目录的用例（cargo 并行跑测试时互斥 cwd）
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// 在指定目录下运行闭包：切换 cwd → 执行 → 恢复（panic 时也恢复）
fn with_cwd<F: FnOnce()>(dir: &Path, f: F) {
    let _guard = CWD_LOCK.lock().unwrap();
    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::set_current_dir(orig).unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// 创建一次性临时目录并返回其路径（调用方负责清理）
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("repo_wiki_test_plan_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 验证 plan.enabled=false 时默认配置不加载 plan
#[test]
fn test_plan_default_disabled() {
    let config = WikiConfig::default();
    // 默认 plan.enabled == false
    assert!(!config.plan.enabled);
    // 默认 plan.path == "wiki_plan.yaml"
    assert_eq!(config.plan.path, "wiki_plan.yaml");
}

/// 验证 WikiPlan 结构体默认值
#[test]
fn test_wiki_plan_defaults() {
    let plan = WikiPlan::default();
    assert!(plan.notes.is_none());
    assert!(plan.knowledgecard.is_none());
    assert!(plan.scope.is_none());
    assert!(plan.version.is_none());
    assert!(plan.sections.is_empty());
    assert!(plan.documents.is_empty());
}

/// 验证 PlanTemplateType 序列化往返
#[test]
fn test_plan_template_type_roundtrip() {
    for (ty, expected) in [
        (PlanTemplateType::Architecture, "architecture"),
        (PlanTemplateType::Prd, "prd"),
        (PlanTemplateType::ApiRef, "api-ref"),
    ] {
        let yaml = serde_yaml::to_string(&ty).unwrap();
        assert_eq!(yaml.trim(), expected);
    }
}

/// 验证新字段（version/knowledgecard/scope/hints）反序列化
#[test]
fn test_wiki_plan_new_fields_deserialize() {
    let yaml = r#"
version: 1
notes: "全局"
knowledgecard:
  notes: "卡片专用"
scope:
  include: ["src/**"]
  exclude: []
sections:
  - module_pattern: "src/config/**"
    template_type: api-ref
    notes: "模块级"
documents:
  - title: "API 参考"
    goal: "生成 API 文档"
    hints: "重点覆盖公开接口"
"#;
    let plan: WikiPlan = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(plan.version, Some(1));
    assert_eq!(plan.notes.as_deref(), Some("全局"));
    assert_eq!(plan.knowledgecard.unwrap().notes.as_deref(), Some("卡片专用"));
    let scope = plan.scope.unwrap();
    assert_eq!(scope.include, vec!["src/**".to_string()]);
    assert_eq!(plan.sections[0].template_type, PlanTemplateType::ApiRef);
    assert_eq!(plan.documents[0].hints.as_deref(), Some("重点覆盖公开接口"));
}

/// load_plan 路径解析与 version 校验（相对项目根 cwd）
///
/// 覆盖：version=1 通过、缺 version 通过、version=2 报错。
#[test]
fn test_load_plan_cwd_and_version_validation() {
    let dir = temp_dir("version");
    with_cwd(&dir, || {
        // version=1 通过
        std::fs::write("wiki_plan.yaml", "version: 1\nnotes: \"v1\"").unwrap();
        let plan = load_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), "wiki_plan.yaml").unwrap().unwrap();
        assert_eq!(plan.version, Some(1));
        assert_eq!(plan.notes.as_deref(), Some("v1"));

        // 缺 version 通过（视为 1）
        std::fs::write("wiki_plan.yaml", "notes: \"no-version\"").unwrap();
        let plan = load_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), "wiki_plan.yaml").unwrap().unwrap();
        assert_eq!(plan.version, None);

        // version=2 报错
        std::fs::write("wiki_plan.yaml", "version: 2\nnotes: \"v2\"").unwrap();
        let err = load_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), "wiki_plan.yaml").unwrap_err();
        assert!(err.to_string().contains("不受支持"), "错误信息应含版本提示: {}", err);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// load_plan 在文件缺失时返回 Ok(None)
#[test]
fn test_load_plan_missing() {
    let dir = temp_dir("missing");
    with_cwd(&dir, || {
        let result = load_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), "wiki_plan.yaml");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// load_plan 能正确解析 wiki_plan.yaml（含 sections/documents）
#[test]
fn test_load_plan_valid_yaml() {
    let dir = temp_dir("valid");
    with_cwd(&dir, || {
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
        std::fs::write("wiki_plan.yaml", yaml_content).unwrap();
        let plan = load_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), "wiki_plan.yaml").unwrap().unwrap();
        assert_eq!(plan.notes.unwrap(), "请重点关注安全设计");
        assert_eq!(plan.sections.len(), 1);
        assert_eq!(plan.sections[0].module_pattern, "src/config/**");
        assert_eq!(plan.documents.len(), 1);
        assert_eq!(plan.documents[0].title, "API 参考");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan 三态：enabled=false → None（不碰文件系统）
#[test]
fn test_resolve_plan_disabled() {
    let config = WikiConfig::default(); // 默认 plan.enabled=false
    let resolved = resolve_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), &config).unwrap();
    assert!(resolved.is_none());
}

/// resolve_plan 三态：文件缺失 → None
#[test]
fn test_resolve_plan_file_missing() {
    let dir = temp_dir("resolve_missing");
    with_cwd(&dir, || {
        let mut config = WikiConfig::default();
        config.plan.enabled = true;
        assert!(resolve_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), &config).unwrap().is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan 三态：坏 YAML → Err（中断生成）
#[test]
fn test_resolve_plan_bad_yaml() {
    let dir = temp_dir("resolve_bad");
    with_cwd(&dir, || {
        std::fs::write("wiki_plan.yaml", "notes: [未闭合列表").unwrap();
        let mut config = WikiConfig::default();
        config.plan.enabled = true;
        let err = resolve_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), &config).unwrap_err();
        assert!(err.to_string().contains("解析 wiki_plan.yaml 失败"), "错误信息应含解析上下文: {}", err);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan 完整映射：notes/card_notes/whitelist/sections/scope_override
#[test]
fn test_resolve_plan_full_mapping() {
    let dir = temp_dir("resolve_full");
    with_cwd(&dir, || {
        let yaml = r#"
notes: "全局 notes"
knowledgecard:
  notes: "卡片 notes"
scope:
  include: ["src/**"]
  exclude: []
sections:
  - module_pattern: "src/generate/**"
    template_type: prd
documents:
  - title: "架构概览"
    goal: "目标"
"#;
        std::fs::write("wiki_plan.yaml", yaml).unwrap();
        let mut config = WikiConfig::default();
        config.plan.enabled = true;

        let resolved = resolve_plan_at(&repo_wiki::project::ProjectRoot::from_cwd().unwrap(), &config).unwrap().unwrap();
        assert_eq!(resolved.notes.as_deref(), Some("全局 notes"));
        assert_eq!(resolved.card_notes.as_deref(), Some("卡片 notes"));
        // 非空白名单保留
        let whitelist = resolved.whitelist.unwrap();
        assert_eq!(whitelist.len(), 1);
        assert_eq!(whitelist[0].title, "架构概览");
        // 白名单条目类型与 sections/scope 完整传递
        let PlanDocument { title, .. } = &whitelist[0];
        assert_eq!(title, "架构概览");
        let PlanSection { template_type, .. } = &resolved.sections[0];
        assert_eq!(*template_type, PlanTemplateType::Prd);
        assert!(resolved.scope_override.is_some());
    });
    let _ = std::fs::remove_dir_all(&dir);
}
