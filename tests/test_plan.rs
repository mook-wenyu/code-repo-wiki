//! wiki_plan.yaml 加载/校验/解析测试（v0.9 W1 重构，对齐 Qoder 语义）
//!
//! 覆盖：全 schema 往返、缺失文件=None、坏 YAML 显式报错、版本校验、
//! template 枚举（""/architecture/非法值）、scope 语法校验、
//! 顶层 scope 显式别名解析、resolve_plan 完整映射。

use code_repo_wiki::config::plan::{
    PlanNote, PlanScope, PlanTemplate, ResolvedPlan, WikiPlan, load_plan_at, resolve_plan_at,
};
use code_repo_wiki::project::ProjectRoot;

/// 创建一次性临时目录并返回其路径（调用方负责清理）
fn temp_dir(tag: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_test_plan_{}_{}_{}",
        tag,
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写入 wiki_plan.yaml 并加载（返回目录，调用方负责清理）
fn load_yaml(tag: &str, yaml: &str) -> (std::path::PathBuf, anyhow::Result<Option<WikiPlan>>) {
    let dir = temp_dir(tag);
    std::fs::write(dir.join("wiki_plan.yaml"), yaml).unwrap();
    let root = ProjectRoot::new(dir.clone());
    let result = load_plan_at(&root, "wiki_plan.yaml");
    (dir, result)
}

/// 全 schema 往返：template/notes(author)/documents/scope 完整解析
#[test]
fn test_plan_full_schema_roundtrip() {
    let yaml = r#"
version: 1
repowiki:
  template: "architecture"
  notes:
    - text: "命名规范：公开函数必须写文档注释"
      author: "架构组"
    - text: "必写小节：用法示例"
  documents:
    - title: "接入指南"
      goal: "介绍如何集成本库"
      parent: "运维手册"
      hints: "突出快速开始"
knowledgecard:
  notes:
    - text: "卡片注明编码规约"
  scope:
    include: ["src/**"]
    exclude: ["src/vendor/**"]
"#;
    let (dir, result) = load_yaml("roundtrip", yaml);
    let plan = result.expect("合法 plan 应加载成功");
    let plan = plan.expect("文件存在应返回 Some");
    assert_eq!(plan.version, 1);
    assert_eq!(plan.repowiki.template, PlanTemplate::Architecture);
    assert_eq!(plan.repowiki.notes.len(), 2);
    assert_eq!(plan.repowiki.notes[0].text, "命名规范：公开函数必须写文档注释");
    assert_eq!(plan.repowiki.notes[0].author, "架构组");
    // author 缺省 = 空串（兼容旧格式）
    assert_eq!(plan.repowiki.notes[1].author, "");
    let doc = &plan.repowiki.documents[0];
    assert_eq!(doc.title, "接入指南");
    assert_eq!(doc.parent, "运维手册");
    assert_eq!(doc.hints, "突出快速开始");
    assert_eq!(plan.knowledgecard.notes[0].text, "卡片注明编码规约");
    let scope = plan.knowledgecard.scope.expect("knowledgecard.scope 应存在");
    assert_eq!(scope.include, vec!["src/**".to_string()]);
    assert_eq!(scope.exclude, vec!["src/vendor/**".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 文件缺失 → Ok(None)（空 plan，保持默认行为）
#[test]
fn test_plan_missing_file_returns_none() {
    let dir = temp_dir("missing");
    let root = ProjectRoot::new(dir.clone());
    let result = load_plan_at(&root, "wiki_plan.yaml");
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 坏 YAML → 显式 Err（不静默忽略、不兜底）
#[test]
fn test_plan_bad_yaml_errors() {
    let (dir, result) = load_yaml("bad_yaml", "notes: [未闭合列表");
    let err = result.expect_err("坏 YAML 应报错");
    assert!(
        err.to_string().contains("解析 wiki_plan.yaml 失败"),
        "错误应含解析上下文: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 版本校验：显式 2 → 报错；缺省 → 视为 1
#[test]
fn test_plan_version_validation() {
    let (dir, result) = load_yaml("version2", "version: 2\nrepowiki:\n  template: \"\"\n");
    let err = result.expect_err("版本 2 应报错");
    assert!(
        err.to_string().contains("不受支持"),
        "错误应含版本提示: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&dir);

    // 缺省 version → 视为 1（通过）
    let (dir2, result2) = load_yaml("version_absent", "repowiki:\n  template: \"\"\n");
    let plan = result2.expect("缺省 version 应通过");
    assert_eq!(plan.expect("文件存在应返回 Some").version, 1);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// template 枚举："" → Default，architecture → Architecture，非法值 → 报错
#[test]
fn test_plan_template_enum() {
    // 缺省 template → Default
    let (dir, result) = load_yaml("tpl_absent", "repowiki:\n  notes: []\n");
    let plan = result.expect("缺省 template 应通过").unwrap();
    assert_eq!(plan.repowiki.template, PlanTemplate::Default);
    let _ = std::fs::remove_dir_all(&dir);

    // "" → Default
    let (dir, result) = load_yaml("tpl_empty", "repowiki:\n  template: \"\"\n");
    let plan = result.expect("空字符串应解析为 Default").unwrap();
    assert_eq!(plan.repowiki.template, PlanTemplate::Default);
    let _ = std::fs::remove_dir_all(&dir);

    // architecture → Architecture
    let (dir, result) = load_yaml("tpl_arch", "repowiki:\n  template: architecture\n");
    let plan = result.expect("architecture 应解析成功").unwrap();
    assert_eq!(plan.repowiki.template, PlanTemplate::Architecture);
    let _ = std::fs::remove_dir_all(&dir);

    // 非法值 → 报错（解析层拦截，不静默兜底）
    let (dir, result) = load_yaml("tpl_bad", "repowiki:\n  template: prd\n");
    let err = result.expect_err("非法 template 值应报错");
    assert!(
        err.to_string().contains("解析 wiki_plan.yaml 失败"),
        "错误应含解析上下文: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// scope 语法校验：非法 glob（未闭合 alternate 组）→ 解析期报错；
/// 未闭合 `[` 属 gitignore 宽容语义（字面量），不拦截
#[test]
fn test_plan_scope_syntax_validation() {
    let (dir, result) = load_yaml(
        "scope_bad_glob",
        "knowledgecard:\n  scope:\n    include: [\"a{b\"]\n",
    );
    let err = result.expect_err("非法 glob 应报错");
    assert!(
        err.to_string().contains("scope 模式语法错误"),
        "错误应定位到 scope 模式: {}",
        err
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 顶层 scope 显式别名：knowledgecard.scope 未提供时回落顶层 scope；
/// 两者都提供时以 knowledgecard.scope 为准
#[test]
fn test_plan_scope_alias_resolution() {
    // 仅顶层 scope → 回落生效
    let yaml = "scope:\n  include: [\"lib/**\"]\n  exclude: []\n";
    let (dir, result) = load_yaml("scope_top", yaml);
    let plan = result.expect("合法 plan 应加载成功").unwrap();
    assert!(plan.knowledgecard.scope.is_none());
    let top = plan.scope.expect("顶层 scope 应存在");
    assert_eq!(top.include, vec!["lib/**".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);

    // 两层都提供 → knowledgecard.scope 优先（官方位置）
    let yaml = "knowledgecard:\n  scope:\n    include: [\"src/**\"]\nscope:\n  include: [\"lib/**\"]\n";
    let (dir, result) = load_yaml("scope_both", yaml);
    let plan = result.expect("合法 plan 应加载成功").unwrap();
    assert_eq!(
        plan.knowledgecard
            .scope
            .as_ref()
            .expect("knowledgecard.scope 应存在")
            .include,
        vec!["src/**".to_string()]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan_at：文件缺失 → None
#[test]
fn test_resolve_plan_none_when_missing() {
    let dir = temp_dir("resolve_missing");
    let root = ProjectRoot::new(dir.clone());
    assert!(resolve_plan_at(&root).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan_at：坏文件 → Err（解析失败终止，不兜底）
#[test]
fn test_resolve_plan_bad_yaml_errors() {
    let dir = temp_dir("resolve_bad");
    std::fs::write(dir.join("wiki_plan.yaml"), "version: [坏").unwrap();
    let root = ProjectRoot::new(dir.clone());
    let err = resolve_plan_at(&root).unwrap_err();
    assert!(err.to_string().contains("解析 wiki_plan.yaml 失败"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_plan_at：完整映射——notes/card_notes/template/documents/scope
#[test]
fn test_resolve_plan_full_mapping() {
    let yaml = r#"
version: 1
repowiki:
  template: "architecture"
  notes:
    - text: "全局引导"
      author: "A"
  documents:
    - title: "接入指南"
      goal: "集成步骤"
      hints: "快速开始"
knowledgecard:
  notes:
    - text: "卡片引导"
  scope:
    include: ["src/**"]
    exclude: []
"#;
    let dir = temp_dir("resolve_full");
    std::fs::write(dir.join("wiki_plan.yaml"), yaml).unwrap();
    let root = ProjectRoot::new(dir.clone());
    let resolved: ResolvedPlan = resolve_plan_at(&root).unwrap().expect("应解析出 plan");
    assert_eq!(resolved.notes.len(), 1);
    assert_eq!(resolved.notes[0].text, "全局引导");
    assert_eq!(resolved.notes[0].author, "A");
    assert_eq!(resolved.card_notes.len(), 1);
    assert_eq!(resolved.card_notes[0].text, "卡片引导");
    assert_eq!(resolved.template, PlanTemplate::Architecture);
    assert_eq!(resolved.documents[0].title, "接入指南");
    let scope = resolved.scope_override.expect("scope_override 应存在");
    assert_eq!(scope.include, vec!["src/**".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 空 repowiki/knowledgecard 缺省：全字段回退默认（零配置零破坏）
#[test]
fn test_plan_minimal_schema_defaults() {
    let (dir, result) = load_yaml("minimal", "version: 1\n");
    let plan = result.expect("最小 schema 应通过").unwrap();
    assert!(plan.repowiki.notes.is_empty());
    assert!(plan.repowiki.documents.is_empty());
    assert!(plan.knowledgecard.notes.is_empty());
    assert!(plan.knowledgecard.scope.is_none());
    assert_eq!(plan.repowiki.template, PlanTemplate::Default);
    let _ = std::fs::remove_dir_all(&dir);
}

/// PlanNote 与 PlanScope 的默认构造（供生成层测试直接使用）
#[test]
fn test_plan_note_and_scope_defaults() {
    let note = PlanNote {
        text: "x".into(),
        author: String::new(),
    };
    assert_eq!(note.author, "");
    let scope = PlanScope::default();
    assert!(scope.include.is_empty() && scope.exclude.is_empty());
}
