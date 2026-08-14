//! 技术栈确定性解析：从依赖清单文件提取框架/库/版本（零 LLM，防幻觉）。
//!
//! 支持清单：Cargo.toml/Cargo.lock（TOML）、package.json（JSON）、
//! pyproject.toml（TOML）、requirements.txt（文本）、go.mod（文本）。
//! 不支持 XML 形态清单（pom.xml/*.csproj）——v1 明确边界，文档已注明。
//!
//! 设计要点：
//! - 确定性：输出按 (category, name) 字典序，与 HashMap 迭代序无关；
//! - 防幻觉：版本字段只从清单原文提取，不做任何规范化或推断；
//! - 韧性：某清单缺失/解析失败 → 跳过该分类，不中断其他清单、不兜底。

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 技术栈条目：name=依赖名、version=版本（清单无版本时为空串）、
/// category=技术分类（rust/cargo、javascript/npm、python/pip、go）、
/// manifest=来源清单文件名（如 "Cargo.toml"），供卡片「来源清单」展示与溯源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TechStackEntry {
    pub name: String,
    pub version: String,
    pub category: String,
    pub manifest: String,
}

/// 支持解析的清单文件名权威集合（增量判定/外部消费者引用）。
/// parse_tech_stack 内部按解析顺序使用同名清单字面量——两处逐字一致，
/// 一致性由 tests/test_project_cards.rs 的断言（MANIFEST_FILES ⊆
/// model::PROJECT_CARD_INPUT_FILES）与各清单解析单测守护。
pub const MANIFEST_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
];

/// 解析项目根下的全部支持清单，返回确定性排序（按 (category, name) 字典序）的条目列表。
///
/// 边界契约：
/// - 清单文件不存在 → 跳过（该分类无条目）；
/// - 清单文件存在但解析失败（坏 TOML/JSON/格式）→ tracing::warn 显式告警并跳过该文件
///   （不中断其他清单、不静默、不兜底）；
/// - 解析失败视为「该清单无条目」，函数总返回 Ok（解析错误不向上传播——错误已告警，
///   调用方拿到的是可用的部分结果）。
pub fn parse_tech_stack(root: &Path) -> Vec<TechStackEntry> {
    let mut all: Vec<TechStackEntry> = Vec::new();

    // Cargo.toml + Cargo.lock（rust/cargo）：lock 为权威版本源，补版本不输出传递依赖
    if let Some(content) = read_manifest(root, "Cargo.toml") {
        match parse_cargo_toml(&content) {
            Ok(mut entries) => {
                // Cargo.lock 缺失时跳过补版本（直接依赖版本仍以 Cargo.toml 为准）
                if let Some(lock_content) = read_manifest(root, "Cargo.lock") {
                    match parse_cargo_lock(&lock_content) {
                        Ok(lock_versions) => {
                            for e in &mut entries {
                                if let Some(v) = lock_versions.get(&e.name) {
                                    e.version = v.clone();
                                }
                            }
                        }
                        Err(msg) => {
                            tracing::warn!(file = "Cargo.lock", %msg, "依赖清单解析失败，跳过补版本")
                        }
                    }
                }
                all.append(&mut entries);
            }
            Err(msg) => tracing::warn!(file = "Cargo.toml", %msg, "依赖清单解析失败，跳过"),
        }
    }

    // package.json（javascript/npm）
    if let Some(content) = read_manifest(root, "package.json") {
        match parse_package_json(&content) {
            Ok(mut entries) => all.append(&mut entries),
            Err(msg) => tracing::warn!(file = "package.json", %msg, "依赖清单解析失败，跳过"),
        }
    }

    // pyproject.toml + requirements.txt（python/pip）；pyproject 在前，
    // requirements 与之同分类重复的条目在后序去重中被丢弃
    if let Some(content) = read_manifest(root, "pyproject.toml") {
        match parse_pyproject(&content) {
            Ok(mut entries) => all.append(&mut entries),
            Err(msg) => tracing::warn!(file = "pyproject.toml", %msg, "依赖清单解析失败，跳过"),
        }
    }
    if let Some(content) = read_manifest(root, "requirements.txt") {
        match parse_requirements(&content) {
            Ok(mut entries) => all.append(&mut entries),
            Err(msg) => tracing::warn!(file = "requirements.txt", %msg, "依赖清单解析失败，跳过"),
        }
    }

    // go.mod（go）
    if let Some(content) = read_manifest(root, "go.mod") {
        match parse_go_mod(&content) {
            Ok(mut entries) => all.append(&mut entries),
            Err(msg) => tracing::warn!(file = "go.mod", %msg, "依赖清单解析失败，跳过"),
        }
    }

    // 去重：同 (name, category) 保留首个（处理序决定优先级：pyproject 先于
    // requirements、Cargo 各表依赖先于 dev/workspace），再确定性排序
    let mut seen: HashSet<(String, String)> = HashSet::new();
    all.retain(|e| seen.insert((e.name.clone(), e.category.clone())));
    all.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    all
}

/// 读取并确认清单文件存在；不存在返回 None（跳过该分类），
/// 读取失败按解析失败处理（告警并跳过）。
fn read_manifest(root: &Path, file: &str) -> Option<String> {
    let path = root.join(file);
    if !path.is_file() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(file, error = %e, "依赖清单读取失败，跳过");
            None
        }
    }
}

/// 拆分 Python 依赖说明的包名与版本：`name>=1.26` → ("numpy", ">=1.26")。
/// 无操作符 → version 空串。版本段原样保留，不规范化。
fn split_python_spec(spec: &str) -> (String, String) {
    const OPS: [&str; 6] = ["==", ">=", "<=", "~=", "<", ">"];
    let mut pos = spec.len();
    for op in OPS {
        if let Some(p) = spec.find(op) {
            pos = pos.min(p);
        }
    }
    let name = spec[..pos].trim();
    let version = spec[pos..].trim();
    (name.to_string(), version.to_string())
}

/// Cargo.toml：`[dependencies]`/`[dev-dependencies]`/`[workspace.dependencies]` 表。
/// 值=纯版本字符串时取之；值=表（git/path/workspace 形态）时 version 留空但条目保留。
fn parse_cargo_toml(content: &str) -> Result<Vec<TechStackEntry>, String> {
    let doc: toml::Value = toml::from_str(content).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    // 处理序：dependencies 优先于 dev-dependencies 与 workspace.dependencies，
    // 保证同名单键保留 [dependencies] 版本（后续去重保留首个即达此语义）。
    // workspace.dependencies 是嵌套表 `[workspace.dependencies]`，需逐层取而非用
    // 带点路径（后者会当作字面键导航到不存在的顶层 workspace 表）。
    for table in ["dependencies", "dev-dependencies"] {
        let Some(deps) = doc.get(table).and_then(|v| v.as_table()) else {
            continue;
        };
        push_cargo_deps(&mut out, deps);
    }
    if let Some(deps) = doc
        .get("workspace")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("dependencies"))
        .and_then(|v| v.as_table())
    {
        push_cargo_deps(&mut out, deps);
    }
    Ok(out)
}

/// push 单个 Cargo 依赖表（值可为纯版本字符串或 git/path/workspace 表单）。
fn push_cargo_deps(out: &mut Vec<TechStackEntry>, deps: &toml::map::Map<String, toml::Value>) {
    for (name, spec) in deps {
        let version = match spec {
            toml::Value::String(v) => v.clone(),
            toml::Value::Table(t) => match t.get("version") {
                Some(toml::Value::String(v)) => v.clone(),
                _ => String::new(), // git/path/workspace 形态 → 空版本，条目保留
            },
            _ => String::new(),
        };
        out.push(TechStackEntry {
            name: name.clone(),
            version,
            category: "rust/cargo".into(),
            manifest: "Cargo.toml".into(),
        });
    }
}

/// Cargo.lock：`[[package]]` 数组，建 name → version 映射（lock 是权威版本源）。
fn parse_cargo_lock(content: &str) -> Result<HashMap<String, String>, String> {
    let doc: toml::Value = toml::from_str(content).map_err(|e| e.to_string())?;
    let mut map = HashMap::new();
    let Some(pkgs) = doc.get("package").and_then(|v| v.as_array()) else {
        return Ok(map);
    };
    for p in pkgs {
        let Some(t) = p.as_table() else {
            continue;
        };
        let (Some(name), Some(version)) = (
            t.get("name").and_then(|v| v.as_str()),
            t.get("version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        map.insert(name.to_string(), version.to_string());
    }
    Ok(map)
}

/// package.json：`dependencies`/`devDependencies` 对象；值按字符串原样保留
/// （含 `^`/`~`/`>=` 等 semver 范围，不规范化）。
fn parse_package_json(content: &str) -> Result<Vec<TechStackEntry>, String> {
    let doc: serde_json::Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for table in ["dependencies", "devDependencies"] {
        let Some(deps) = doc.get(table).and_then(|v| v.as_object()) else {
            continue;
        };
        for (name, spec) in deps {
            let version = match spec {
                serde_json::Value::String(v) => v.clone(),
                _ => String::new(),
            };
            out.push(TechStackEntry {
                name: name.clone(),
                version,
                category: "javascript/npm".into(),
                manifest: "package.json".into(),
            });
        }
    }
    Ok(out)
}

/// pyproject.toml：`[project].dependencies` 与 `[project.optional-dependencies]` 各数组。
/// 元素可为字符串（拆 name/version）或表（取 name/version 字段）。
fn parse_pyproject(content: &str) -> Result<Vec<TechStackEntry>, String> {
    let doc: toml::Value = toml::from_str(content).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(deps) = doc
        .get("project")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("dependencies"))
        .and_then(|v| v.as_array())
    {
        for item in deps {
            pyproject_push(&mut out, item);
        }
    }
    if let Some(optional) = doc
        .get("project")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("optional-dependencies"))
        .and_then(|v| v.as_table())
    {
        for group in optional.values() {
            if let Some(arr) = group.as_array() {
                for item in arr {
                    pyproject_push(&mut out, item);
                }
            }
        }
    }
    Ok(out)
}

/// push 单个 pyproject 依赖项：字符串形态拆 name/version；表形态取 name/version。
fn pyproject_push(out: &mut Vec<TechStackEntry>, item: &toml::Value) {
    match item {
        toml::Value::String(spec) => {
            let (name, version) = split_python_spec(spec);
            out.push(TechStackEntry {
                name,
                version,
                category: "python/pip".into(),
                manifest: "pyproject.toml".into(),
            });
        }
        toml::Value::Table(t) => {
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = t
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push(TechStackEntry {
                name,
                version,
                category: "python/pip".into(),
                manifest: "pyproject.toml".into(),
            });
        }
        _ => {}
    }
}

/// requirements.txt：逐行解析。跳过空行、`#` 注释、`-r`/`-e`/`--` 开头行；
/// `name==version` 等拆 name 与 version（范围原样保留）；无版本 → 空串。
fn parse_requirements(content: &str) -> Result<Vec<TechStackEntry>, String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("-r ")
            || trimmed.starts_with("-e ")
            || trimmed.starts_with("--")
        {
            continue;
        }
        let (name, version) = split_python_spec(trimmed);
        if name.is_empty() {
            continue;
        }
        out.push(TechStackEntry {
            name,
            version,
            category: "python/pip".into(),
            manifest: "requirements.txt".into(),
        });
    }
    Ok(out)
}

/// go.mod：`module <path>` 只记项目名（日志，不进条目）；`require (` 块与单行
/// `require name version` 各生成条目；`// indirect` 注释行照加，版本后注释剥离。
fn parse_go_mod(content: &str) -> Result<Vec<TechStackEntry>, String> {
    let mut out = Vec::new();
    let mut in_require_block = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("module ") {
            let project = rest.split_whitespace().next().unwrap_or("");
            tracing::info!(project, "go.mod 项目名");
            continue;
        }
        if line == "require (" {
            in_require_block = true;
            continue;
        }
        if in_require_block {
            if line == ")" {
                in_require_block = false;
            } else {
                go_push_require(&mut out, line);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("require ") {
            go_push_require(&mut out, rest);
        }
        // 其余指令（go / toolchain / exclude / replace）忽略
    }
    Ok(out)
}

/// 解析一行 require 依赖：首个 token=name，次 token=version（剥离 `//` 注释）。
fn go_push_require(out: &mut Vec<TechStackEntry>, line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let name = parts[0].to_string();
    let version = parts[1].split("//").next().unwrap_or("").trim().to_string();
    out.push(TechStackEntry {
        name,
        version,
        category: "go".into(),
        manifest: "go.mod".into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 建唯一临时目录，返回路径；块结束时清理（约定见仓库现有测试）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_techstack_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// 便捷找条目（category、name 精确定位）
    fn find<'a>(entries: &'a [TechStackEntry], category: &str, name: &str) -> &'a TechStackEntry {
        entries
            .iter()
            .find(|e| e.category == category && e.name == name)
            .unwrap_or_else(|| panic!("未找到 {category}/{name}"))
    }

    // ---- 六类清单正常解析 ----

    #[test]
    fn parse_cargo_toml_works() {
        let dir = temp_dir("cargo");
        write_file(
            &dir,
            "Cargo.toml",
            r#"[package]
name = "demo"

[dependencies]
serde = "1.0"
tokio = { version = "1.4", features = ["rt"] }
gitdep = { git = "https://example.com/gitdep" }

[dev-dependencies]
tempfile = "3"

[workspace.dependencies]
shared = "0.9"
"#,
        );
        let entries = parse_tech_stack(&dir);
        let serde = find(&entries, "rust/cargo", "serde");
        assert_eq!(serde.version, "1.0");
        assert_eq!(serde.manifest, "Cargo.toml");
        let tokio = find(&entries, "rust/cargo", "tokio");
        assert_eq!(tokio.version, "1.4", "表形态 version 字段");
        assert_eq!(
            find(&entries, "rust/cargo", "gitdep").version,
            "",
            "git 依赖版本留空"
        );
        assert_eq!(find(&entries, "rust/cargo", "tempfile").version, "3");
        assert_eq!(
            find(&entries, "rust/cargo", "shared").version,
            "0.9",
            "workspace 依赖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_cargo_lock_merges_version_and_no_transitive() {
        let dir = temp_dir("lock");
        write_file(
            &dir,
            "Cargo.toml",
            r#"[package]
name = "demo"

[dependencies]
serde = "1"
tokio = "1"
gitonly = { git = "https://example.com/gitonly" }
"#,
        );
        write_file(
            &dir,
            "Cargo.lock",
            r#"# Cargo.lock
version = 3

[[package]]
name = "serde"
version = "1.0.210"

[[package]]
name = "tokio"
version = "1.38.0"

[[package]]
name = "transitive-never"
version = "9.9.9"

[[package]]
name = "dummy"
version = "0.1"
"#,
        );
        let entries = parse_tech_stack(&dir);
        // lock 覆盖 Cargo.toml 版本（权威版本源）
        assert_eq!(find(&entries, "rust/cargo", "serde").version, "1.0.210");
        assert_eq!(find(&entries, "rust/cargo", "tokio").version, "1.38.0");
        // Cargo.toml 有而 lock 无（git 依赖）→ 保留 Cargo.toml 值（空）
        assert_eq!(find(&entries, "rust/cargo", "gitonly").version, "");
        // lock 中的传递依赖不输出
        assert!(
            !entries.iter().any(|e| e.name == "transitive-never"),
            "Cargo.lock 传递依赖不应输出"
        );
        assert!(!entries.iter().any(|e| e.name == "dummy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_package_json_works() {
        let dir = temp_dir("npm");
        write_file(
            &dir,
            "package.json",
            r#"{
  "name": "demo",
  "dependencies": { "react": "^18.2.0", "axios": "~1.4.0" },
  "devDependencies": { "typescript": ">=5.0" }
}
"#,
        );
        let entries = parse_tech_stack(&dir);
        assert_eq!(find(&entries, "javascript/npm", "react").version, "^18.2.0");
        assert_eq!(find(&entries, "javascript/npm", "axios").version, "~1.4.0");
        assert_eq!(
            find(&entries, "javascript/npm", "typescript").version,
            ">=5.0"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_pyproject_works() {
        let dir = temp_dir("pyproj");
        write_file(
            &dir,
            "pyproject.toml",
            r#"[project]
name = "demo"
dependencies = [
    "numpy>=1.26",
    { name = "requests", version = "2.31" },
    "click",
]

[project.optional-dependencies]
dev = ["pytest>=7.0", "mypy~=1.7"]
"#,
        );
        let entries = parse_tech_stack(&dir);
        assert_eq!(find(&entries, "python/pip", "numpy").version, ">=1.26");
        assert_eq!(find(&entries, "python/pip", "requests").version, "2.31");
        assert_eq!(find(&entries, "python/pip", "click").version, "");
        assert_eq!(find(&entries, "python/pip", "pytest").version, ">=7.0");
        assert_eq!(find(&entries, "python/pip", "mypy").version, "~=1.7");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_requirements_works() {
        let dir = temp_dir("req");
        write_file(
            &dir,
            "requirements.txt",
            "# 注释\n\nflask==3.0\nDjango>=5.0\nrequests~=2.31\nclick\n-r base.txt\n-e .\n--index-url https://pypi.org/simple\n",
        );
        let entries = parse_tech_stack(&dir);
        assert_eq!(find(&entries, "python/pip", "flask").version, "==3.0");
        assert_eq!(find(&entries, "python/pip", "Django").version, ">=5.0");
        assert_eq!(find(&entries, "python/pip", "requests").version, "~=2.31");
        assert_eq!(find(&entries, "python/pip", "click").version, "");
        // 注释 / 空行 / -r / -e / -- 行跳过
        assert!(!entries.iter().any(|e| e.name.starts_with('#')));
        assert!(!entries.iter().any(|e| e.name.contains("base")));
        assert!(!entries.iter().any(|e| e.name.contains("index")));
        assert!(!entries.iter().any(|e| e.name.contains("-e")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_go_mod_works() {
        let dir = temp_dir("go");
        write_file(
            &dir,
            "go.mod",
            r#"module example.com/demo

go 1.21

require (
	golang.org/x/net v0.24.0
	github.com/google/uuid v1.6.0 // indirect
)

require golang.org/x/text v0.15.0
"#,
        );
        let entries = parse_tech_stack(&dir);
        assert_eq!(find(&entries, "go", "golang.org/x/net").version, "v0.24.0");
        assert_eq!(
            find(&entries, "go", "github.com/google/uuid").version,
            "v1.6.0",
            "indirect 注释剥离"
        );
        assert_eq!(
            find(&entries, "go", "golang.org/x/text").version,
            "v0.15.0",
            "单行 require"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 边界：坏清单告警跳过、空/缺失清单、去重、确定性 ----

    #[test]
    fn bad_manifest_warns_and_skips_others() {
        let dir = temp_dir("bad");
        write_file(&dir, "Cargo.toml", "not = [valid toml");
        write_file(&dir, "package.json", "{ not valid json ");
        write_file(&dir, "requirements.txt", "flask==3.0");
        // 全部坏清单都跳过，其余清单照常返回部分结果，不 panic
        let entries = parse_tech_stack(&dir);
        assert_eq!(find(&entries, "python/pip", "flask").version, "==3.0");
        assert!(!entries.iter().any(|e| e.manifest == "Cargo.toml"));
        assert!(!entries.iter().any(|e| e.manifest == "package.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_or_missing_manifests_yield_empty() {
        let dir = temp_dir("empty");
        // 空目录（全部缺失）
        assert!(parse_tech_stack(&dir).is_empty());
        // 空内容清单 → 各自分类无条目，总结果空
        write_file(&dir, "Cargo.toml", "");
        write_file(&dir, "Cargo.lock", "");
        write_file(&dir, "package.json", "{}");
        write_file(&dir, "pyproject.toml", "");
        write_file(&dir, "requirements.txt", "");
        write_file(&dir, "go.mod", "");
        assert!(parse_tech_stack(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_manifest_dedup_prefers_pyproject() {
        let dir = temp_dir("dedup");
        write_file(
            &dir,
            "pyproject.toml",
            r#"[project]
dependencies = ["numpy>=1.26", "flask==3.0"]
"#,
        );
        write_file(
            &dir,
            "requirements.txt",
            "numpy==1.26.1\nflask==3.0.3\nnewdep>=2.0\n",
        );
        let entries = parse_tech_stack(&dir);
        // pyproject 已是 numpy 1.26（范围 >=1.26），requirements 的 numpy 重复项被丢弃
        let numpy = find(&entries, "python/pip", "numpy");
        assert_eq!(numpy.version, ">=1.26");
        assert_eq!(numpy.manifest, "pyproject.toml");
        // requirements 独有依赖保留（来源 requirements.txt）
        let newdep = find(&entries, "python/pip", "newdep");
        assert_eq!(newdep.version, ">=2.0");
        assert_eq!(newdep.manifest, "requirements.txt");
        assert_eq!(
            entries.iter().filter(|e| e.name == "flask").count(),
            1,
            "flask 两清单同分类只保留一个"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deterministic_sort_by_category_then_name() {
        let dir = temp_dir("sort");
        write_file(
            &dir,
            "Cargo.toml",
            r#"[package]
name = "demo"

[dependencies]
zeta = "1"
alpha = "2"
"#,
        );
        write_file(&dir, "requirements.txt", "requests>=2.0\nclick==8.0\n");
        write_file(
            &dir,
            "go.mod",
            r#"module example.com/demo

require (
	zap v1.24.0
	alpha/v2 v2.0.0
)
"#,
        );
        write_file(
            &dir,
            "package.json",
            r#"{ "dependencies": { "mochi": "1.0", "lodash": "^4.0" } }"#,
        );
        let entries = parse_tech_stack(&dir);
        // 全部按 (category, name) 字典序：go < javascript/npm < python/pip < rust/cargo
        for w in entries.windows(2) {
            assert!(
                (w[0].category.as_str(), w[0].name.as_str())
                    <= (w[1].category.as_str(), w[1].name.as_str()),
                "排序错误: {}/{} 应在 {}/{} 之前",
                w[0].category,
                w[0].name,
                w[1].category,
                w[1].name
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
